//! Persistent native-client sessions. No USB or prover implementation lives here.
use anyhow::{Context, Result, bail};
use dg_xch_core::{
    blockchain::{
        class_group_element::ClassgroupElement, sized_bytes::Bytes100, unsized_bytes::UnsizedBytes,
        vdf_info::VdfInfo, vdf_proof::VdfProof,
    },
    traits::SizedBytes,
};
use dg_xch_timelord::worker::{ProofRequest, ProofResult};
use dg_xch_vdf::{discriminant::create_discriminant_bytes, proof::verify_vdf_serial};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex as SyncMutex},
    time::{Duration, Instant},
};
use timelord_core::{Chain, Config};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::{Mutex, mpsc},
};
use vdf_client_protocol::{
    ClientMessage, MAX_REQUEST_ITERATIONS, UnverifiedProof, acknowledge_stop, encode_initial,
    read_message, request, request_stop,
};

pub struct Lease {
    pub socket: TcpStream,
    pub board: String,
    pub slot: u8,
    pub clients: Arc<SyncMutex<BTreeSet<(String, u8)>>>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        // The native client sends STOP even when it sees EOF. Keep the read half
        // alive briefly so cancellation does not turn its final write into an
        // uncaught C++ exception. This socket is retired, never reused for work.
        #[cfg(unix)]
        {
            use std::os::fd::AsFd;
            if let Ok(runtime) = tokio::runtime::Handle::try_current()
                && let Ok(fd) = self.socket.as_fd().try_clone_to_owned()
                && let Ok(mut retired) = TcpStream::from_std(std::net::TcpStream::from(fd))
            {
                runtime.spawn(async move {
                    let _ = retired.shutdown().await;
                    let _ = tokio::time::timeout(
                        Duration::from_secs(5),
                        tokio::io::copy(&mut retired, &mut tokio::io::sink()),
                    )
                    .await;
                });
            }
        }
        self.clients
            .lock()
            .expect("client state")
            .remove(&(self.board.clone(), self.slot));
    }
}
#[derive(Clone, Copy)]
pub struct ProofTiming {
    iterations: u64,
    started: Instant,
}
impl ProofTiming {
    pub fn started_unix(&self) -> u64 {
        crate::unix_now().saturating_sub(self.started.elapsed().as_secs())
    }
    pub fn delivered_ips(&self) -> f64 {
        self.iterations as f64 / self.started.elapsed().as_secs_f64()
    }
}
fn estimated_ips(iterations: u64, elapsed: Duration) -> f64 {
    (iterations as f64 / elapsed.as_secs_f64() * 10.).floor() / 10.
}

struct Active {
    lease: Lease,
    key: ProofRequest,
    started: Instant,
    estimated_started: Instant,
}
struct Lane {
    board: String,
    active: Mutex<Option<Active>>,
}
#[derive(Debug)]
pub struct AsicFailure {
    pub board_id: String,
    pub verification: bool,
    pub message: String,
}
impl std::fmt::Display for AsicFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.board_id, self.message)
    }
}
impl std::error::Error for AsicFailure {}
pub struct AsicBackend {
    lanes: Vec<(Chain, Lane)>,
    pools: HashMap<String, Mutex<mpsc::Receiver<Lease>>>,
    runtime: Arc<SyncMutex<crate::runtime::RuntimeState>>,
}
impl AsicBackend {
    pub fn new(
        config: &Config,
        runtime: Arc<SyncMutex<crate::runtime::RuntimeState>>,
    ) -> Result<(Self, HashMap<String, mpsc::Sender<Lease>>)> {
        let assignments = config.assignments().map_err(anyhow::Error::msg)?;
        let mut pools = HashMap::new();
        let mut offers = HashMap::new();
        for board in &config.hardware.boards {
            let (tx, rx) = mpsc::channel(3);
            offers.insert(board.id.clone(), tx);
            pools.insert(board.id.clone(), Mutex::new(rx));
        }
        let lanes = assignments
            .into_iter()
            .map(|a| {
                (
                    a.chain,
                    Lane {
                        board: a.board_id,
                        active: Mutex::new(None),
                    },
                )
            })
            .collect();
        Ok((
            Self {
                lanes,
                pools,
                runtime,
            },
            offers,
        ))
    }
    pub async fn prove(
        &self,
        chain: Chain,
        work: ProofRequest,
    ) -> Result<(ProofResult, ProofTiming)> {
        self.prove_inner(chain, work, true).await.map_err(|error| {
            if error.is::<AsicFailure>() {
                error
            } else {
                AsicFailure {
                    board_id: self
                        .lanes
                        .iter()
                        .find(|(c, _)| *c == chain)
                        .map(|(_, lane)| lane.board.clone())
                        .unwrap_or_default(),
                    verification: false,
                    message: error.to_string(),
                }
                .into()
            }
        })
    }
    pub async fn prime(&self, work: ProofRequest) -> Result<()> {
        // A one-iteration primer keeps ICC ready; it is not a scheduled VDF output.
        self.prove_inner(Chain::InfusedChallenge, work, false)
            .await?;
        Ok(())
    }
    async fn prove_inner(
        &self,
        chain: Chain,
        work: ProofRequest,
        record_metric: bool,
    ) -> Result<(ProofResult, ProofTiming)> {
        work.validate_regular()?;
        if work.iterations > MAX_REQUEST_ITERATIONS {
            bail!("Request exceeds the native challenge iteration limit");
        }
        let lane = &self
            .lanes
            .iter()
            .find(|(c, _)| *c == chain)
            .context("No board assigned to chain")?
            .1;
        let mut owner = lane.active.lock().await;
        // Take ownership during I/O. Cancellation retires the socket instead of
        // leaving a partially consumed frame in a reusable session.
        let mut active = owner.take();
        if active
            .as_ref()
            .is_some_and(|s| !same_challenge(&s.key, &work))
            && let Some(mut previous) = active.take()
        {
            let _ = stop(&mut previous.lease.socket).await;
        }
        let mut active = match active {
            Some(session) => session,
            None => {
                let mut pool = self
                    .pools
                    .get(&lane.board)
                    .context("Missing board listener")?
                    .lock()
                    .await;
                let mut lease = tokio::time::timeout(Duration::from_secs(30), pool.recv())
                    .await
                    .context("Waiting for ASIC client timed out")?
                    .context("ASIC listener stopped")?;
                // Match Python timelord: start when assigning the client, before
                // discriminant construction and the initial handshake.
                let estimated_started = Instant::now();
                let seed = work.challenge.const_bytes().to_vec();
                let bits = work.discriminant_bits;
                let magnitude =
                    tokio::task::spawn_blocking(move || create_discriminant_bytes(&seed, bits))
                        .await??;
                let decimal = format!("-{}", num_bigint::BigUint::from_bytes_be(&magnitude));
                let started = Instant::now();
                lease
                    .socket
                    .write_all(&encode_initial(&decimal, &work.input.data.const_bytes())?)
                    .await?;
                let mut ok = [0; 2];
                tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::io::AsyncReadExt::read_exact(&mut lease.socket, &mut ok),
                )
                .await??;
                if &ok != b"OK" {
                    bail!("ASIC client rejected the challenge");
                }
                Active {
                    lease,
                    key: work.clone(),
                    started,
                    estimated_started,
                }
            }
        };
        request(&mut active.lease.socket, work.iterations).await?;
        let proof = tokio::time::timeout(
            Duration::from_secs(3600),
            read_message(&mut active.lease.socket),
        )
        .await??;
        let ClientMessage::Proof(proof) = proof else {
            acknowledge_stop(&mut active.lease.socket).await?;
            bail!("ASIC stopped before returning its proof");
        };
        // Freeze IPS on receipt, before verification or scheduler/send delays.
        // Publish only after the proof has passed verification.
        let ips = estimated_ips(work.iterations, active.estimated_started.elapsed());
        let checked = work.clone();
        let result = tokio::task::spawn_blocking(move || verify(&checked, *proof))
            .await?
            .map_err(|error| AsicFailure {
                board_id: lane.board.clone(),
                verification: true,
                message: error.to_string(),
            })?;
        let timing = ProofTiming {
            iterations: work.iterations,
            started: active.started,
        };
        if record_metric {
            self.runtime.lock().expect("runtime").estimated(
                chain,
                ips,
                crate::unix_now().saturating_sub(active.estimated_started.elapsed().as_secs()),
            );
        }
        *owner = Some(active);
        Ok((result, timing))
    }
    pub async fn stop_chain(&self, chain: Chain) {
        if let Some((_, lane)) = self.lanes.iter().find(|(c, _)| *c == chain)
            && let Some(mut active) = lane.active.lock().await.take()
        {
            let _ = stop(&mut active.lease.socket).await;
        }
    }
    pub async fn stop_all(&self) {
        for (_, lane) in &self.lanes {
            if let Some(mut active) = lane.active.lock().await.take() {
                let _ = stop(&mut active.lease.socket).await;
            }
        }
        // Close idle sockets too; a reconnect gets an entirely new handshake.
        for pool in self.pools.values() {
            let mut pool = pool.lock().await;
            while pool.try_recv().is_ok() {}
        }
    }
}
fn same_challenge(a: &ProofRequest, b: &ProofRequest) -> bool {
    a.generation == b.generation
        && a.challenge == b.challenge
        && a.input == b.input
        && a.discriminant_bits == b.discriminant_bits
}
async fn stop(socket: &mut TcpStream) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        request_stop(socket).await?;
        loop {
            match read_message(socket).await? {
                ClientMessage::Stop => {
                    acknowledge_stop(socket).await?;
                    return Ok::<_, anyhow::Error>(());
                }
                ClientMessage::Proof(_) => {}
            }
        }
    })
    .await
    .context("ASIC stop handshake timed out")?
}
pub fn verify(work: &ProofRequest, proof: UnverifiedProof) -> Result<ProofResult> {
    if proof.iterations != work.iterations {
        bail!("ASIC proof has an unexpected iteration count");
    }
    let mut blob = proof.output.to_vec();
    blob.extend_from_slice(&proof.witness);
    for checkpoint in &proof.checkpoints {
        blob.extend_from_slice(&checkpoint.iterations.to_be_bytes());
        blob.extend_from_slice(&checkpoint.challenge_prime);
        blob.extend_from_slice(&checkpoint.witness);
    }
    if !verify_vdf_serial(
        work.challenge.as_ref(),
        work.input.data.as_ref(),
        &blob,
        work.discriminant_bits,
        work.iterations,
        proof.checkpoints.len() as u64,
    ) {
        bail!("ASIC proof failed verification");
    }
    Ok(ProofResult {
        generation: work.generation,
        info: VdfInfo {
            challenge: work.challenge,
            number_of_iterations: work.iterations,
            output: ClassgroupElement {
                data: Bytes100::parse(&proof.output)?,
            },
        },
        proof: VdfProof {
            witness_type: proof.checkpoints.len() as u8,
            witness: UnsizedBytes::new(blob[100..].to_vec()),
            normalized_to_identity: false,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dg_xch_core::blockchain::sized_bytes::Bytes32;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use vdf_client_protocol::Checkpoint;
    fn work(iterations: u64, bits: usize) -> ProofRequest {
        ProofRequest {
            generation: 1,
            challenge: Bytes32::const_new([42; 32]),
            input: ClassgroupElement::get_default_element(),
            iterations,
            discriminant_bits: bits,
        }
    }
    fn canonical() -> UnverifiedProof {
        let blob = include_bytes!("../tests/fixtures/checkpointed-proof.bin");
        UnverifiedProof {
            iterations: 360,
            output: blob[..100].try_into().unwrap(),
            witness: blob[100..200].try_into().unwrap(),
            checkpoints: blob[200..]
                .as_chunks::<141>()
                .0
                .iter()
                .map(|s| Checkpoint {
                    iterations: u64::from_be_bytes(s[..8].try_into().unwrap()),
                    challenge_prime: s[8..41].try_into().unwrap(),
                    witness: s[41..].try_into().unwrap(),
                })
                .collect(),
        }
    }
    #[test]
    fn converts_and_verifies_canonical_cpp_checkpoints_and_rejects_corruption() {
        let request = work(360, 1024);
        let proof = canonical();
        let verified = verify(&request, proof.clone()).unwrap();
        assert_eq!(verified.proof.witness_type, 2);
        assert_eq!(verified.proof.witness.bytes.len(), 382);
        let mut bad = proof.clone();
        bad.checkpoints.swap(0, 1);
        assert!(verify(&request, bad).is_err());
        let mut bad = proof.clone();
        bad.checkpoints[0].challenge_prime[3] ^= 1;
        assert!(verify(&request, bad).is_err());
        let mut bad = proof.clone();
        bad.output[5] ^= 1;
        assert!(verify(&request, bad).is_err());
        assert!(verify(&work(361, 1024), proof).is_err());
    }
    async fn setup() -> (
        Arc<AsicBackend>,
        TcpStream,
        Arc<SyncMutex<BTreeSet<(String, u8)>>>,
    ) {
        let config: Config =
            toml::from_str("[[hardware.boards]]\nid='a'\nname='A'\nclient_port=18110\n").unwrap();
        let (backend, offers) = AsicBackend::new(&config, Default::default()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let clients = Arc::new(SyncMutex::new(BTreeSet::from([("a".into(), 0)])));
        offers["a"]
            .send(Lease {
                socket,
                board: "a".into(),
                slot: 0,
                clients: clients.clone(),
            })
            .await
            .ok()
            .unwrap();
        (Arc::new(backend), client, clients)
    }
    async fn handshake(socket: &mut TcpStream) {
        let mut header = [0; 4];
        socket.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0], b'N');
        let count: usize = std::str::from_utf8(&header[1..]).unwrap().parse().unwrap();
        let mut body = vec![0; count + 101];
        socket.read_exact(&mut body).await.unwrap();
        socket.write_all(b"OK").await.unwrap();
    }
    async fn requested(socket: &mut TcpStream) -> u64 {
        let mut length = [0; 2];
        socket.read_exact(&mut length).await.unwrap();
        let size: usize = std::str::from_utf8(&length).unwrap().parse().unwrap();
        let mut value = vec![0; size];
        socket.read_exact(&mut value).await.unwrap();
        std::str::from_utf8(&value).unwrap().parse().unwrap()
    }
    #[tokio::test]
    async fn successive_requests_share_a_challenge_and_stop_with_ack() {
        let (backend, mut socket, clients) = setup().await;
        let fixture = tokio::spawn(async move {
            handshake(&mut socket).await;
            for expected in [1, 20] {
                assert_eq!(requested(&mut socket).await, expected);
                let request = work(expected, 64);
                let proof = dg_xch_vdf::proof::prove_result(
                    request.challenge.as_ref(),
                    request.input.data.as_ref(),
                    64,
                    expected,
                )
                .unwrap();
                let mut raw = expected.to_be_bytes().to_vec();
                raw.extend(100_u64.to_be_bytes());
                raw.extend_from_slice(&proof[..100]);
                raw.push(0);
                raw.extend_from_slice(&proof[100..]);
                let hex: Vec<_> = raw
                    .iter()
                    .flat_map(|b| format!("{b:02x}").into_bytes())
                    .collect();
                socket
                    .write_all(&(hex.len() as u32).to_be_bytes())
                    .await
                    .unwrap();
                for part in hex.chunks(7) {
                    socket.write_all(part).await.unwrap();
                }
            }
            assert_eq!(requested(&mut socket).await, 0);
            socket.write_all(b"STOP").await.unwrap();
            let mut ack = [0; 3];
            socket.read_exact(&mut ack).await.unwrap();
            assert_eq!(&ack, b"ACK");
        });
        // ICC priming must never become the global speed gauge.
        backend.prime(work(1, 64)).await.unwrap();
        assert!(
            backend
                .runtime
                .lock()
                .unwrap()
                .estimated_observations
                .is_empty()
        );
        for n in [20] {
            assert_eq!(
                backend
                    .prove(Chain::InfusedChallenge, work(n, 64))
                    .await
                    .unwrap()
                    .0
                    .info
                    .number_of_iterations,
                n
            );
        }
        {
            let state = backend.runtime.lock().unwrap();
            assert_eq!(state.estimated_observations.len(), 1);
            assert_eq!(
                state.estimated_observations[0].chain,
                Chain::InfusedChallenge
            );
            assert!(state.observations.is_empty());
            assert_eq!(state.proofs_delivered, 0);
        }
        backend.stop_all().await;
        fixture.await.unwrap();
        assert!(clients.lock().unwrap().is_empty());
    }
    #[test]
    fn exporter_ips_is_cumulative_receipt_speed_truncated_to_tenths() {
        assert_eq!(estimated_ips(1_000_000, Duration::from_secs(3)), 333_333.3);
        assert_eq!(
            estimated_ips(20_000_000, Duration::from_secs(20)),
            1_000_000.
        );
    }

    #[tokio::test]
    async fn rejected_proof_does_not_update_the_speed_gauge() {
        let (backend, mut socket, _) = setup().await;
        let fixture = tokio::spawn(async move {
            handshake(&mut socket).await;
            requested(&mut socket).await;
            // A structurally valid but mathematically invalid class-group output.
            let mut raw = 20_u64.to_be_bytes().to_vec();
            raw.extend(100_u64.to_be_bytes());
            raw.extend([0; 100]);
            raw.push(0);
            raw.extend([0; 100]);
            let hex: Vec<_> = raw
                .iter()
                .flat_map(|b| format!("{b:02x}").into_bytes())
                .collect();
            socket
                .write_all(&(hex.len() as u32).to_be_bytes())
                .await
                .unwrap();
            socket.write_all(&hex).await.unwrap();
        });
        assert!(backend.prove(Chain::Challenge, work(20, 64)).await.is_err());
        fixture.await.unwrap();
        assert!(
            backend
                .runtime
                .lock()
                .unwrap()
                .estimated_observations
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cancelled_proof_closes_its_partial_frame_and_releases_the_slot() {
        let (backend, mut socket, clients) = setup().await;
        let (sent, received) = tokio::sync::oneshot::channel();
        let fixture = tokio::spawn(async move {
            handshake(&mut socket).await;
            requested(&mut socket).await;
            socket.write_all(&[0, 0]).await.unwrap();
            sent.send(()).unwrap();
            let mut byte = [0];
            match socket.read(&mut byte).await {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                result => panic!("Expected closed cancelled session, got {result:?}"),
            }
            // Model the native EOF path's final STOP/proof writes. The retiring
            // server must drain them instead of resetting the peer connection.
            socket.write_all(b"STOP").await.unwrap();
            for _ in 0..8 {
                tokio::time::sleep(Duration::from_millis(5)).await;
                socket.write_all(&[0; 32768]).await.unwrap();
            }
        });
        let proving = tokio::spawn({
            let backend = backend.clone();
            async move { backend.prove(Chain::Challenge, work(10, 64)).await }
        });
        received.await.unwrap();
        proving.abort();
        let _ = proving.await;
        tokio::time::timeout(Duration::from_secs(2), fixture)
            .await
            .unwrap()
            .unwrap();
        assert!(clients.lock().unwrap().is_empty());
        backend.stop_all().await;
    }
}

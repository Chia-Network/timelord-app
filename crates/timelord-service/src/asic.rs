//! Persistent native-client sessions. No USB or prover implementation lives here.
mod arithmetic;
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
use timelord_core::{Assignment, Chain, Config};
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
    pub generation: u64,
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
    pub ready_at: Instant,
    pub ready_unix_ms: u128,
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
        assignments: &[Assignment],
        runtime: Arc<SyncMutex<crate::runtime::RuntimeState>>,
    ) -> Result<(Self, HashMap<String, mpsc::Sender<Lease>>)> {
        let mut pools = HashMap::new();
        let mut offers = HashMap::new();
        for board in &config.hardware.boards {
            let (tx, rx) = mpsc::channel(3);
            offers.insert(board.id.clone(), tx);
            pools.insert(board.id.clone(), Mutex::new(rx));
        }
        let lanes = assignments
            .iter()
            .cloned()
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
    // A hardware outage belongs to one physical board. Keep other proof futures
    // and the authenticated node session alive while that board's supervisor recovers.
    pub async fn prove_when_available(
        &self,
        chain: Chain,
        work: ProofRequest,
        record_metric: bool,
    ) -> Result<(ProofResult, ProofTiming)> {
        work.validate_regular()?;
        if work.iterations > MAX_REQUEST_ITERATIONS {
            bail!("Request exceeds the native challenge iteration limit");
        }
        let lane = self
            .lanes
            .iter()
            .find(|(c, _)| *c == chain)
            .map(|(_, lane)| lane)
            .context("Chain has no assigned ASIC")?;
        loop {
            let (generation, unavailable) = {
                let state = self.runtime.lock().expect("runtime");
                state
                    .boards
                    .get(&lane.board)
                    .map(|b| {
                        (
                            b.generation,
                            b.latched_fault
                                || ["fault", "thermal_stop", "reconnecting"]
                                    .contains(&b.state.as_str()),
                        )
                    })
                    .unwrap_or((0, false))
            };
            if unavailable {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            let result = tokio::select! {
                result = self.prove_inner(chain, work.clone(), record_metric) => result,
                _ = async {
                    loop {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        let state = self.runtime.lock().expect("runtime");
                        if state.boards.get(&lane.board).is_some_and(|b| b.generation != generation
                            || b.latched_fault || ["fault", "thermal_stop", "reconnecting"].contains(&b.state.as_str())) { break; }
                    }
                } => continue,
            };
            match result {
                Ok(proof) => {
                    let mut state = self.runtime.lock().expect("runtime");
                    for event in &mut state.events {
                        if event.board_id.as_ref() == Some(&lane.board)
                            && event.kind == "board_fault"
                            && event.end_unix.is_none()
                        {
                            event.end_unix = Some(crate::unix_now());
                        }
                    }
                    return Ok(proof);
                }
                Err(error) => {
                    let mut state = self.runtime.lock().expect("runtime");
                    let hardware = state.boards.entry(lane.board.clone()).or_default();
                    // A delayed EOF from the previous process must not kill its replacement.
                    if hardware.generation != generation {
                        continue;
                    }
                    let verification = error
                        .downcast_ref::<AsicFailure>()
                        .is_some_and(|e| e.verification);
                    hardware.latched_fault |= verification;
                    if hardware.state != "thermal_stop" {
                        hardware.state = "fault".into();
                    }
                    hardware.fault_reason = Some(error.to_string());
                    hardware.raw_speeds.clear();
                    let needs_attention = hardware.latched_fault;
                    if verification {
                        state.failed_proofs += 1;
                    }
                    if !state.events.iter().any(|e| {
                        e.board_id.as_ref() == Some(&lane.board)
                            && e.kind == "board_fault"
                            && e.end_unix.is_none()
                    }) {
                        state.event(Some(lane.board.clone()), "board_fault");
                    }
                    state.phase = "waiting_for_clients".into();
                    state.message = if needs_attention {
                        format!("An ASIC needs attention: {error}")
                    } else {
                        "Waiting for ASIC recovery.".into()
                    };
                    tracing::warn!(board=%lane.board, %error, verification, "ASIC proof lane unavailable");
                }
            }
        }
    }
    pub async fn prime(&self, work: ProofRequest) -> Result<()> {
        // A one-iteration primer keeps ICC ready; it is not a scheduled VDF output.
        self.prove_when_available(Chain::InfusedChallenge, work, false)
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
        let generation = self
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .get(&lane.board)
            .map(|b| b.generation)
            .unwrap_or(0);
        let mut active = owner.take().filter(|a| a.lease.generation == generation);
        // A new discriminant does not depend on the old socket. Start it before
        // retiring that session or waiting for the native client to reconnect.
        let discriminant = if active
            .as_ref()
            .is_none_or(|s| !same_challenge(&s.key, &work))
        {
            let seed = work.challenge.const_bytes().to_vec();
            let bits = work.discriminant_bits;
            Some(arithmetic::spawn(chain, move || create_discriminant_bytes(&seed, bits)).await?)
        } else {
            None
        };
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
                let mut lease = tokio::time::timeout(Duration::from_secs(30), async {
                    loop {
                        let lease = pool.recv().await.context("ASIC listener stopped")?;
                        let current = self
                            .runtime
                            .lock()
                            .expect("runtime")
                            .boards
                            .get(&lane.board)
                            .map(|b| b.generation)
                            .unwrap_or(0);
                        if lease.generation == current {
                            return Ok::<_, anyhow::Error>(lease);
                        }
                    }
                })
                .await
                .context("Waiting for ASIC client timed out")??;
                // Match Python timelord: start the IPS clock when assigning the
                // client, even if discriminant preparation is already running.
                let estimated_started = Instant::now();
                let magnitude = discriminant
                    .expect("A new session needs a discriminant")
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
        let result = arithmetic::spawn(chain, move || verify(&checked, *proof))
            .await?
            .await?
            .map_err(|error| AsicFailure {
                board_id: lane.board.clone(),
                verification: true,
                message: error.to_string(),
            })?;
        let timing = ProofTiming {
            iterations: work.iterations,
            started: active.started,
            ready_at: Instant::now(),
            ready_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
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
    pub async fn stop_board(&self, board: &str) {
        for (_, lane) in &self.lanes {
            if lane.board == board
                && let Some(mut active) = lane.active.lock().await.take()
            {
                let _ = stop(&mut active.lease.socket).await;
            }
        }
        if let Some(pool) = self.pools.get(board) {
            let mut pool = pool.lock().await;
            while pool.try_recv().is_ok() {}
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
mod tests;

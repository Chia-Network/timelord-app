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
    let blob = include_bytes!("../../tests/fixtures/checkpointed-proof.bin");
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
fn backend_routes_using_the_resolved_plan_without_recomputing_board_order() {
    let mut config = Config::default();
    config.hardware.boards = (0..3)
        .map(|i| {
            toml::from_str(&format!(
                "id='b{i}'\nname='B{i}'\nclient_port={}",
                18110 + i
            ))
            .unwrap()
        })
        .collect();
    let mut plan = config.assignments().unwrap();
    plan[0].board_id = "b2".into();
    plan[2].board_id = "b0".into();
    let (backend, _) = AsicBackend::new(&config, &plan, Default::default()).unwrap();
    assert_eq!(
        backend
            .lanes
            .iter()
            .map(|(chain, lane)| (*chain, lane.board.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (Chain::Challenge, "b2"),
            (Chain::Reward, "b1"),
            (Chain::InfusedChallenge, "b0")
        ]
    );
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
    let (backend, offers) =
        AsicBackend::new(&config, &config.assignments().unwrap(), Default::default()).unwrap();
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
            generation: 0,
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
async fn send_fixture_proof(socket: &mut TcpStream, request: &ProofRequest) {
    let proof = dg_xch_vdf::proof::prove_result(
        request.challenge.as_ref(),
        request.input.data.as_ref(),
        64,
        request.iterations,
    )
    .unwrap();
    let mut raw = request.iterations.to_be_bytes().to_vec();
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
    socket.write_all(&hex).await.unwrap();
}
#[tokio::test]
async fn failed_board_reconnects_without_retiring_a_healthy_chain_session() {
    let config:Config=toml::from_str("[[hardware.boards]]\nid='healthy'\nname='Healthy'\nclient_port=18110\n[[hardware.boards]]\nid='failed'\nname='Failed'\nclient_port=18111").unwrap();
    let runtime = Arc::new(SyncMutex::new(crate::runtime::RuntimeState::default()));
    let (backend, offers) =
        AsicBackend::new(&config, &config.assignments().unwrap(), runtime.clone()).unwrap();
    let backend = Arc::new(backend);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let clients = Arc::new(SyncMutex::new(BTreeSet::new()));
    let mut sockets = Vec::new();
    for board in ["healthy", "failed"] {
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        clients.lock().unwrap().insert((board.into(), 0));
        offers[board]
            .send(Lease {
                socket,
                board: board.into(),
                slot: 0,
                generation: 0,
                clients: clients.clone(),
            })
            .await
            .ok()
            .unwrap();
        sockets.push(client);
    }
    let mut healthy = sockets.remove(0);
    let mut failed = sockets.remove(0);
    let bad = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .prove_when_available(Chain::InfusedChallenge, work(20, 64), true)
                .await
        }
    });
    handshake(&mut failed).await;
    assert_eq!(requested(&mut failed).await, 20);
    drop(failed);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if runtime
                .lock()
                .unwrap()
                .boards
                .get("failed")
                .is_some_and(|b| b.state == "fault")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (release, hold) = tokio::sync::oneshot::channel();
    let healthy_fixture = tokio::spawn(async move {
        handshake(&mut healthy).await;
        for n in [20, 40] {
            assert_eq!(requested(&mut healthy).await, n);
            send_fixture_proof(&mut healthy, &work(n, 64)).await;
        }
        let _ = hold.await; // Keep the same socket alive through the sibling restart.
    });
    backend
        .prove_when_available(Chain::Challenge, work(20, 64), true)
        .await
        .unwrap();
    backend.stop_board("failed").await;
    runtime
        .lock()
        .unwrap()
        .boards
        .get_mut("failed")
        .unwrap()
        .generation = 1;
    runtime
        .lock()
        .unwrap()
        .boards
        .get_mut("failed")
        .unwrap()
        .state = "starting".into();
    // A queued socket from the dead process is discarded, never assigned to the replacement.
    let stale = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    offers["failed"]
        .send(Lease {
            socket,
            board: "failed".into(),
            slot: 1,
            generation: 0,
            clients: clients.clone(),
        })
        .await
        .ok()
        .unwrap();
    drop(stale);
    let mut replacement = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    offers["failed"]
        .send(Lease {
            socket,
            board: "failed".into(),
            slot: 0,
            generation: 1,
            clients: clients.clone(),
        })
        .await
        .ok()
        .unwrap();
    let replacement_fixture = tokio::spawn(async move {
        handshake(&mut replacement).await;
        assert_eq!(requested(&mut replacement).await, 20);
        send_fixture_proof(&mut replacement, &work(20, 64)).await;
    });
    backend
        .prove_when_available(Chain::Challenge, work(40, 64), true)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), bad)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    release.send(()).unwrap();
    healthy_fixture.await.unwrap();
    replacement_fixture.await.unwrap();
    let state = runtime.lock().unwrap();
    assert_eq!(state.failed_proofs, 0);
    assert_eq!(state.estimated_observations.len(), 3);
    assert!(
        state
            .events
            .iter()
            .any(|e| e.board_id.as_deref() == Some("failed")
                && e.kind == "board_fault"
                && e.end_unix.is_some())
    );
}
#[tokio::test]
async fn invalid_proof_latches_only_its_board_and_never_publishes_an_ips_sample() {
    let (backend, mut socket, _clients) = setup().await;
    let pending = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .prove_when_available(Chain::Challenge, work(20, 64), true)
                .await
        }
    });
    handshake(&mut socket).await;
    assert_eq!(requested(&mut socket).await, 20);
    // A mathematically valid proof for a different iteration count must be rejected.
    send_fixture_proof(&mut socket, &work(19, 64)).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if backend
                .runtime
                .lock()
                .unwrap()
                .boards
                .get("a")
                .is_some_and(|b| b.latched_fault)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    {
        let state = backend.runtime.lock().unwrap();
        assert_eq!(state.failed_proofs, 1);
        assert!(state.estimated_observations.is_empty());
        assert_eq!(state.boards["a"].state, "fault");
    }
    assert!(!pending.is_finished());
    pending.abort();
    let _ = pending.await;
}
#[test]
fn discriminant_starts_before_a_client_is_available_and_cancellation_releases_lane() {
    // No async worker threads: the callback can only come from spawn_blocking.
    // The old ordering waits for a client and never creates that thread.
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let signal = started.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .on_thread_start(move || signal.add_permits(1))
        .build()
        .unwrap();
    runtime.block_on(async {
        let config: Config =
            toml::from_str("[[hardware.boards]]\nid='a'\nname='A'\nclient_port=18110\n").unwrap();
        let (backend, _offers) =
            AsicBackend::new(&config, &config.assignments().unwrap(), Default::default()).unwrap();
        let backend = Arc::new(backend);
        let proving = tokio::spawn({
            let backend = backend.clone();
            async move { backend.prove(Chain::Challenge, work(10, 64)).await }
        });
        let permit = tokio::time::timeout(Duration::from_secs(2), started.acquire())
            .await
            .expect("Discriminant must start while the client pool is empty")
            .unwrap();
        permit.forget();
        assert!(!proving.is_finished());
        proving.abort();
        assert!(
            proving
                .await
                .err()
                .expect("Cancelled proof task")
                .is_cancelled()
        );
        assert!(backend.lanes[0].1.active.try_lock().unwrap().is_none());
        assert!(backend.pools["a"].try_lock().is_ok());
        assert!(
            backend
                .runtime
                .lock()
                .unwrap()
                .estimated_observations
                .is_empty()
        );
    });
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

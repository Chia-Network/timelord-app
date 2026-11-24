use super::*;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use timelord_core::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vdf_client_protocol::Checkpoint;
fn offers_for(
    backend: &AsicBackend,
    config: &Config,
) -> std::collections::HashMap<String, mpsc::Sender<Lease>> {
    config
        .hardware
        .boards
        .iter()
        .map(|b| (b.id.clone(), backend.add_board(&b.id)))
        .collect()
}
fn route(board: &str, chain: Chain, engine: u8) -> Assignment {
    Assignment {
        board_id: board.into(),
        chain,
        engine,
    }
}
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
fn backend_routes_follow_live_runtime_routes() {
    let runtime: Arc<SyncMutex<crate::runtime::RuntimeState>> = Default::default();
    let backend = AsicBackend::new(runtime.clone(), crate::operational::State::fixture());
    assert!(backend.route(Chain::Challenge).is_none());
    runtime.lock().unwrap().routing = vec![Assignment {
        board_id: "b2".into(),
        chain: Chain::Challenge,
        engine: 1,
    }];
    assert_eq!(backend.route(Chain::Challenge).unwrap().board_id, "b2");
    assert!(backend.route(Chain::Reward).is_none());
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
    let metrics = crate::observability::Metrics::new(config.node.network.clone()).unwrap();
    let process = crate::operational::State::new(metrics.clone());
    let runtime = {
        let mut fixture = crate::runtime::RuntimeState::new(false);
        fixture.metric_epoch = metrics.begin_session();
        // One fixture socket on engine 0 serves whichever chain a test uses.
        fixture.routing = vec![
            route("a", Chain::Challenge, 0),
            route("a", Chain::InfusedChallenge, 0),
        ];
        fixture
            .boards
            .entry("a".into())
            .or_default()
            .lifecycle
            .testing()
            .serve(crate::unix_now());
        fixture
    };
    let backend = AsicBackend::new(Arc::new(SyncMutex::new(runtime)), process);
    let offer = backend.add_board("a");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    let clients = Arc::new(SyncMutex::new(BTreeSet::from([("a".into(), 0)])));
    offer
        .send(Lease {
            socket,
            board: "a".into(),
            slot: 0,
            generation: 1,
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
async fn icc_primer_does_not_supply_unmanaged_readiness_evidence() {
    let mut config: Config =
        toml::from_str("[[hardware.boards]]\nid='a'\nname='A'\nclient_port=18110\n").unwrap();
    config.node.certificate = Some("/fixture/no-io.crt".into());
    config.node.private_key = Some("/fixture/no-io.key".into());
    config.node.ca_certificate = Some("/fixture/no-io-ca.crt".into());
    let metrics = crate::observability::Metrics::new(config.node.network.clone()).unwrap();
    let process = crate::operational::State::new(metrics.clone());
    let runtime = Arc::new(SyncMutex::new({
        let mut fixture = crate::runtime::RuntimeState::new(false);
        fixture.metric_epoch = metrics.begin_session();
        fixture.scheduler_initialized = true;
        fixture.node = "authenticated".into();
        fixture.routing = vec![
            route("a", Chain::InfusedChallenge, 0),
            route("a", Chain::Challenge, 1),
            route("a", Chain::Reward, 2),
        ];
        fixture
    }));
    let backend = AsicBackend::new(runtime.clone(), process.clone());
    let offers = offers_for(&backend, &config);
    let backend = Arc::new(backend);
    let shared = crate::Shared::new(
        None,
        config,
        runtime.clone(),
        process,
        crate::unix_now(),
        crate::unix_now(),
    );
    runtime
        .lock()
        .unwrap()
        .boards
        .entry("a".into())
        .or_default()
        .lifecycle
        .testing()
        .serve(crate::unix_now());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    for (slot, chain, record_metric) in [
        (0, Chain::InfusedChallenge, false),
        (1, Chain::Challenge, true),
        (2, Chain::Reward, true),
    ] {
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        shared.clients.lock().unwrap().insert(("a".into(), slot));
        offers["a"]
            .send(Lease {
                socket,
                board: "a".into(),
                slot,
                generation: 1,
                clients: shared.clients.clone(),
            })
            .await
            .ok()
            .unwrap();
        let request = work(if record_metric { 20 } else { 1 }, 64);
        let proving = tokio::spawn({
            let backend = backend.clone();
            let request = request.clone();
            async move {
                backend
                    .prove_when_available(chain, request, record_metric)
                    .await
            }
        });
        handshake(&mut client).await;
        assert_eq!(requested(&mut client).await, request.iterations);
        send_fixture_proof(&mut client, &request).await;
        proving.await.unwrap().unwrap();
        {
            let state = runtime.lock().unwrap();
            let board = &state.boards["a"];
            assert!(
                !board.lifecycle.slot_verified(0),
                "primer became capacity evidence"
            );
            assert_eq!(
                (0..3).filter(|s| board.lifecycle.slot_verified(*s)).count(),
                usize::from(slot)
            );
            assert_eq!(
                board.lifecycle.verified_generation(),
                record_metric.then_some(1)
            );
        }
        assert_eq!(
            crate::observability::ready(&shared),
            slot == 2,
            "readiness requires real proofs from both required chains"
        );
    }
}
#[tokio::test]
async fn a_dropped_session_retries_on_its_owner_and_repeated_failures_retire_only_that_board() {
    let config:Config=toml::from_str("[[hardware.boards]]\nid='healthy'\nname='Healthy'\nclient_port=18110\n[[hardware.boards]]\nid='failed'\nname='Failed'\nclient_port=18111").unwrap();
    let runtime = Arc::new(SyncMutex::new(crate::runtime::RuntimeState::default()));
    {
        let mut state = runtime.lock().unwrap();
        state.routing = vec![
            route("healthy", Chain::Challenge, 0),
            route("failed", Chain::InfusedChallenge, 0),
        ];
        for board in ["healthy", "failed"] {
            state
                .boards
                .entry(board.into())
                .or_default()
                .lifecycle
                .testing()
                .serve(crate::unix_now());
        }
    }
    let backend = AsicBackend::new(runtime.clone(), crate::operational::State::fixture());
    let offers = offers_for(&backend, &config);
    let backend = Arc::new(backend);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let clients = Arc::new(SyncMutex::new(BTreeSet::new()));
    let connect = |board: &'static str, generation: u64| {
        let listener = &listener;
        let clients = clients.clone();
        let offer = offers[board].clone();
        async move {
            let client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (socket, _) = listener.accept().await.unwrap();
            clients.lock().unwrap().insert((board.into(), 0));
            offer
                .send(Lease {
                    socket,
                    board: board.into(),
                    slot: 0,
                    generation,
                    clients,
                })
                .await
                .ok()
                .unwrap();
            client
        }
    };
    let mut healthy = connect("healthy", 1).await;
    let mut failed = connect("failed", 1).await;
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
    // One dropped session retries on the same owner.
    let mut retry = connect("failed", 1).await;
    handshake(&mut retry).await;
    assert_eq!(requested(&mut retry).await, 20);
    assert!(
        runtime.lock().unwrap().boards["failed"]
            .lifecycle
            .accepts_work()
    );
    drop(retry);
    // A second failure within the window retires only that owner.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if runtime
                .lock()
                .unwrap()
                .boards
                .get("failed")
                .is_some_and(|b| b.lifecycle.needs_retirement())
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
        .lifecycle
        .testing()
        .serve(crate::unix_now());
    // A queued socket from the dead process is discarded, never assigned to the replacement.
    let stale = connect("failed", 1).await;
    drop(stale);
    let mut replacement = connect("failed", 2).await;
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
            .estimated_observations
            .iter()
            .any(|o| o.board_id.as_deref() == Some("failed") && o.engine == Some(0))
    );
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
async fn a_routed_engine_that_never_connects_retires_its_owner_immediately() {
    let runtime = Arc::new(SyncMutex::new(crate::runtime::RuntimeState::default()));
    {
        let mut state = runtime.lock().unwrap();
        state.routing = vec![route("stuck", Chain::Challenge, 1)];
        state
            .boards
            .entry("stuck".into())
            .or_default()
            .lifecycle
            .testing()
            .serve(crate::unix_now());
    }
    let backend = AsicBackend::new(runtime.clone(), crate::operational::State::fixture());
    let _offer = backend.add_board("stuck");
    assert!(!backend.lane_failed("stuck", 1, &anyhow::Error::new(ClientWaitTimeout)));
    assert!(
        runtime.lock().unwrap().boards["stuck"]
            .lifecycle
            .needs_retirement()
    );
    // An ordinary session error on a fresh owner is retried first.
    runtime
        .lock()
        .unwrap()
        .boards
        .get_mut("stuck")
        .unwrap()
        .lifecycle
        .testing()
        .serve(crate::unix_now());
    assert!(backend.lane_failed("stuck", 2, &anyhow::anyhow!("early eof")));
    assert!(!backend.lane_failed("stuck", 2, &anyhow::anyhow!("early eof")));
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
                .is_some_and(|b| b.lifecycle.status().latched_fault)
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
        assert_eq!(
            state.boards["a"].lifecycle.status().phase.as_str(),
            "invalid_proof"
        );
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
        let runtime: Arc<SyncMutex<crate::runtime::RuntimeState>> = Default::default();
        runtime.lock().unwrap().routing = config.assignments().unwrap();
        let backend = AsicBackend::new(runtime, crate::operational::State::fixture());
        let _offers = offers_for(&backend, &config);
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
        assert!(backend.pools.read().unwrap()["a"].try_lock().is_ok());
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
        let metrics = &backend.process.metrics;
        assert_eq!(
            metrics.counter("proofs_verified_total", &["infused_challenge"]),
            1
        );
        assert_eq!(
            metrics.counter("proof_verification_failures_total", &["infused_challenge"]),
            0
        );
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
    let state = backend.runtime.lock().unwrap();
    assert!(state.estimated_observations.is_empty());
    let metrics = &backend.process.metrics;
    assert_eq!(
        metrics.counter("proof_verification_failures_total", &["challenge"]),
        1
    );
    assert_eq!(metrics.counter("proofs_verified_total", &["challenge"]), 0);
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

#[tokio::test]
async fn live_route_failover_preserves_the_healthy_engine_session() {
    let mut config = Config::default();
    config.hardware.boards = (0..2)
        .map(|i| {
            toml::from_str(&format!(
                "id='b{i}'\nname='B{i}'\nprofile='manual'\nclient_port={}",
                18110 + i
            ))
            .unwrap()
        })
        .collect();
    let plan = vec![
        Assignment {
            board_id: "b0".into(),
            engine: 1,
            chain: Chain::Challenge,
        },
        Assignment {
            board_id: "b1".into(),
            engine: 1,
            chain: Chain::Reward,
        },
    ];
    let mut runtime = crate::runtime::RuntimeState::new(false);
    runtime.routing = plan.clone();
    for id in ["b0", "b1"] {
        runtime.boards.entry(id.into()).or_default().lifecycle =
            crate::board_lifecycle::BoardLifecycle::fixture(
                crate::board_lifecycle::Phase::Connected,
                true,
                None,
                0,
            );
    }
    let runtime = Arc::new(SyncMutex::new(runtime));
    let backend = AsicBackend::new(runtime.clone(), crate::operational::State::fixture());
    let offers = offers_for(&backend, &config);
    let backend = Arc::new(backend);
    let clients = Arc::new(SyncMutex::new(BTreeSet::new()));
    let mut sockets = Vec::new();
    // The spare arrives first. Physical routing must park it, not consume it
    // as the healthy board's preferred engine or drop it during cancellation.
    for (id, slot) in [("b0", 1), ("b1", 0), ("b1", 1)] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        clients.lock().unwrap().insert((id.into(), slot));
        offers[id]
            .send(Lease {
                socket,
                board: id.into(),
                slot,
                generation: 0,
                clients: clients.clone(),
            })
            .await
            .ok()
            .unwrap();
        sockets.push(peer);
    }
    let mut failed = sockets.remove(0);
    let mut spare = sockets.remove(0);
    let mut healthy = sockets.remove(0);
    let first = tokio::spawn(async move {
        handshake(&mut failed).await;
        assert_eq!(requested(&mut failed).await, 1);
        send_fixture_proof(&mut failed, &work(1, 64)).await;
        let mut buf = [0; 1];
        let _ = failed.read(&mut buf).await;
    });
    let second = tokio::spawn(async move {
        handshake(&mut healthy).await;
        for n in [1, 20] {
            assert_eq!(requested(&mut healthy).await, n);
            send_fixture_proof(&mut healthy, &work(n, 64)).await;
        }
        assert_eq!(requested(&mut healthy).await, 0);
        healthy.write_all(b"STOP").await.unwrap();
        let mut ack = [0; 3];
        healthy.read_exact(&mut ack).await.unwrap();
        assert_eq!(&ack, b"ACK");
    });
    let fallback = tokio::spawn(async move {
        handshake(&mut spare).await;
        assert_eq!(requested(&mut spare).await, 20);
        send_fixture_proof(&mut spare, &work(20, 64)).await;
        assert_eq!(requested(&mut spare).await, 0);
        spare.write_all(b"STOP").await.unwrap();
        let mut ack = [0; 3];
        spare.read_exact(&mut ack).await.unwrap();
    });
    backend
        .prove_when_available(Chain::Challenge, work(1, 64), true)
        .await
        .unwrap();
    backend
        .prove_when_available(Chain::Reward, work(1, 64), true)
        .await
        .unwrap();
    runtime.lock().unwrap().routing[0] = Assignment {
        board_id: "b1".into(),
        engine: 0,
        chain: Chain::Challenge,
    };
    backend
        .prove_when_available(Chain::Challenge, work(20, 64), true)
        .await
        .unwrap();
    backend
        .prove_when_available(Chain::Reward, work(20, 64), true)
        .await
        .unwrap();
    backend.stop_all().await;
    first.await.unwrap();
    second.await.unwrap();
    fallback.await.unwrap();
    assert_eq!(
        runtime.lock().unwrap().boards["b1"]
            .lifecycle
            .status()
            .generation,
        0
    );
    assert_eq!(
        backend
            .process
            .metrics
            .counter("proofs_verified_total", &["reward"]),
        2
    );
}

async fn serve_recovery_proof(
    offer: &mpsc::Sender<Lease>,
    slot: u8,
    generation: u64,
    invalid: bool,
) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut peer = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (socket, _) = listener.accept().await.unwrap();
    offer
        .send(Lease {
            socket,
            board: "a".into(),
            slot,
            generation,
            clients: Default::default(),
        })
        .await
        .ok()
        .unwrap();
    tokio::spawn(async move {
        handshake(&mut peer).await;
        assert_eq!(requested(&mut peer).await, 360);
        let blob = include_bytes!("../../tests/fixtures/checkpointed-proof.bin");
        let mut raw = 360_u64.to_be_bytes().to_vec();
        raw.extend(100_u64.to_be_bytes());
        raw.extend_from_slice(&blob[..100]);
        raw.push(((blob.len() - 200) / 141) as u8);
        raw.extend_from_slice(&blob[100..]);
        if invalid {
            raw[16] ^= 1;
        }
        let frame: Vec<_> = raw
            .iter()
            .flat_map(|v| format!("{v:02x}").into_bytes())
            .collect();
        peer.write_all(&(frame.len() as u32).to_be_bytes())
            .await
            .unwrap();
        peer.write_all(&frame).await.unwrap();
        if !invalid {
            assert_eq!(requested(&mut peer).await, 0);
            peer.write_all(b"STOP").await.unwrap();
            let mut ack = [0; 3];
            peer.read_exact(&mut ack).await.unwrap();
        }
    })
}
fn checking_backend(
    config: &Config,
    fault: Option<crate::board_lifecycle::Fault>,
) -> (
    Arc<AsicBackend>,
    std::collections::HashMap<String, mpsc::Sender<Lease>>,
) {
    let mut state = crate::runtime::RuntimeState::new(false);
    state.routing = config.assignments().unwrap();
    state.boards.entry("a".into()).or_default().lifecycle =
        crate::board_lifecycle::BoardLifecycle::fixture(
            crate::board_lifecycle::Phase::Checking,
            true,
            fault,
            0,
        );
    let runtime = Arc::new(SyncMutex::new(state));
    let backend = AsicBackend::new(runtime, crate::operational::State::fixture());
    let offers = offers_for(&backend, config);
    (Arc::new(backend), offers)
}
#[tokio::test]
async fn recovery_proofs_use_the_live_owner_pool_without_publishing_or_trusting_bad_proofs() {
    let mut config = Config::default();
    config
        .hardware
        .boards
        .push(toml::from_str("id='a'\nname='A'\nprofile='manual'\nclient_port=18110").unwrap());
    let (backend, offers) =
        checking_backend(&config, Some(crate::board_lifecycle::Fault::InvalidProof));
    for invalid in [false, true] {
        let fixture = serve_recovery_proof(&offers["a"], 1, 0, invalid).await;
        let checked = backend.check_board("a").await;
        assert_eq!(checked.is_err(), invalid);
        if let Err(error) = checked {
            assert!(error.downcast_ref::<AsicFailure>().unwrap().verification);
        }
        fixture.await.unwrap();
    }
    let state = backend.runtime.lock().unwrap();
    assert_eq!(state.proofs_delivered, 0);
    assert!(state.observations.is_empty());
    assert!(state.estimated_observations.is_empty());
    assert_eq!(
        state.boards["a"].lifecycle.status().fault,
        Some(crate::board_lifecycle::Fault::InvalidProof)
    );
    assert_eq!(
        backend
            .process
            .metrics
            .counter("proofs_verified_total", &["challenge"]),
        0
    );
}
#[tokio::test]
async fn admission_checks_publish_no_production_telemetry() {
    let mut config = Config::default();
    config
        .hardware
        .boards
        .push(toml::from_str("id='a'\nname='A'\nprofile='manual'\nclient_port=18110").unwrap());
    let (backend, offers) = checking_backend(&config, None);
    let fixture = serve_recovery_proof(&offers["a"], 1, 0, false).await;
    backend.check_board("a").await.unwrap();
    fixture.await.unwrap();
    let metrics = &backend.process.metrics;
    assert_eq!(
        metrics.histogram_count("proof_duration_seconds", &["challenge", "discriminant"]),
        0,
        "Admission probes must not publish discriminant durations"
    );
    assert_eq!(
        metrics.histogram_count("proof_duration_seconds", &["challenge", "client_wait"]),
        0,
        "Admission probes must not publish client wait durations"
    );
    assert_eq!(
        metrics.session_starts(Chain::Challenge),
        0,
        "Admission probes must not publish client sessions"
    );
    assert_eq!(metrics.counter("work_cancelled_total", &["challenge"]), 0);
}
#[tokio::test]
async fn cancelled_scheduled_request_records_cancellation_and_releases_pending() {
    let (backend, mut socket, _clients) = setup().await;
    let (sent, received) = tokio::sync::oneshot::channel();
    let fixture = tokio::spawn(async move {
        handshake(&mut socket).await;
        requested(&mut socket).await;
        sent.send(()).unwrap();
        // Hold the session open without answering so the scheduled request stays pending.
        let mut byte = [0];
        let _ = socket.read(&mut byte).await;
    });
    let proving = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .prove_when_available(Chain::Challenge, work(10, 64), true)
                .await
        }
    });
    received.await.unwrap();
    assert_eq!(backend.runtime.lock().unwrap().pending[0], 1);
    proving.abort();
    assert!(
        proving
            .await
            .err()
            .expect("Cancelled proof task")
            .is_cancelled()
    );
    assert_eq!(backend.runtime.lock().unwrap().pending[0], 0);
    assert_eq!(
        backend
            .process
            .metrics
            .counter("work_cancelled_total", &["challenge"]),
        1
    );
    fixture.abort();
    let _ = fixture.await;
}

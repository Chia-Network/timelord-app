use super::pool::Pool;
use super::*;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

async fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (server, client)
}

fn lease(socket: TcpStream, slot: u8, generation: u64) -> Lease {
    Lease {
        socket,
        board: "a".into(),
        slot,
        generation,
        clients: Arc::new(SyncMutex::new(BTreeSet::new())),
    }
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
        challenge: dg_xch_core::blockchain::sized_bytes::Bytes32::const_new([42; 32]),
        input: ClassgroupElement::get_default_element(),
        iterations,
        discriminant_bits: bits,
    }
}

async fn read_initial(socket: &mut TcpStream) {
    let mut header = [0; 4];
    socket.read_exact(&mut header).await.unwrap();
    let count: usize = std::str::from_utf8(&header[1..]).unwrap().parse().unwrap();
    let mut body = vec![0; count + 101];
    socket.read_exact(&mut body).await.unwrap();
}

async fn handshake(socket: &mut TcpStream) {
    read_initial(socket).await;
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
async fn waiters_on_different_slots_do_not_block_each_other() {
    let (sender, receiver) = mpsc::channel(3);
    let pool = Arc::new(Pool::new(receiver));
    let slot0 = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(0), 1).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 1, 1)).await.unwrap();
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(1), 1))
        .await
        .expect("slot 1 must not wait behind slot 0")
        .expect("slot 1 lease");
    assert_eq!(acquired.slot, 1);
    assert!(!slot0.is_finished(), "slot 0 must still be waiting");

    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 0, 1)).await.unwrap();
    let acquired = tokio::time::timeout(Duration::from_millis(500), slot0)
        .await
        .expect("slot 0 lease")
        .unwrap()
        .expect("slot 0 lease");
    assert_eq!(acquired.slot, 0);
}

#[tokio::test]
async fn slow_handshake_on_one_engine_does_not_block_another_engine() {
    let mut state = crate::runtime::RuntimeState::default();
    state.routing = vec![
        route("a", Chain::Challenge, 0),
        route("a", Chain::InfusedChallenge, 1),
    ];
    state
        .boards
        .entry("a".into())
        .or_default()
        .lifecycle
        .testing()
        .serve(crate::unix_now());
    let runtime = Arc::new(SyncMutex::new(state));
    let backend = Arc::new(AsicBackend::new(
        runtime,
        crate::operational::State::fixture(),
    ));
    let offer = backend.add_board("a");

    let (socket, mut stalled_peer) = socket_pair().await;
    offer.send(lease(socket, 0, 1)).await.unwrap();
    let stalled = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .prove_when_available(Chain::Challenge, work(20, 64), false)
                .await
        }
    });
    let (ready, reached) = tokio::sync::oneshot::channel();
    let stalled_fixture = tokio::spawn(async move {
        // Consume the initial frame but never answer OK: the engine is stuck in
        // its natural 10s handshake read.
        read_initial(&mut stalled_peer).await;
        ready.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    tokio::time::timeout(Duration::from_secs(2), reached)
        .await
        .expect("first engine never reached its handshake")
        .unwrap();
    assert!(!stalled.is_finished());

    let (socket, mut peer) = socket_pair().await;
    offer.send(lease(socket, 1, 1)).await.unwrap();
    let proving = tokio::spawn({
        let backend = backend.clone();
        async move {
            backend
                .prove_when_available(Chain::InfusedChallenge, work(20, 64), false)
                .await
        }
    });
    let fixture = tokio::spawn(async move {
        handshake(&mut peer).await;
        assert_eq!(requested(&mut peer).await, 20);
        send_fixture_proof(&mut peer, &work(20, 64)).await;
        assert_eq!(requested(&mut peer).await, 0);
        peer.write_all(b"STOP").await.unwrap();
        let mut ack = [0; 3];
        peer.read_exact(&mut ack).await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), proving)
        .await
        .expect("second engine waited behind the first engine's handshake")
        .unwrap()
        .unwrap();
    assert!(!stalled.is_finished(), "first engine must still be stalled");

    stalled.abort();
    let _ = stalled.await;
    stalled_fixture.abort();
    let _ = stalled_fixture.await;
    backend.stop_all().await;
    fixture.await.unwrap();
}

#[tokio::test]
async fn a_cancelled_waiter_does_not_lose_an_arriving_lease() {
    let (sender, receiver) = mpsc::channel(3);
    let pool = Arc::new(Pool::new(receiver));
    let waiter = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(0), 1).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    waiter.abort();
    let _ = waiter.await;

    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 0, 1)).await.unwrap();
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(0), 1))
        .await
        .expect("later waiter")
        .expect("lease");
    assert_eq!(acquired.slot, 0);
}

#[tokio::test]
async fn cancelling_the_waiter_polling_a_slot_does_not_wedge_other_slots() {
    let (sender, receiver) = mpsc::channel(3);
    let pool = Arc::new(Pool::new(receiver));
    let slot0 = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(0), 1).await }
    });
    let slot1 = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(1), 1).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 1, 1)).await.unwrap();
    let acquired = tokio::time::timeout(Duration::from_millis(500), slot1)
        .await
        .expect("slot 1 must acquire while slot 0 waits")
        .unwrap()
        .expect("slot 1 lease");
    assert_eq!(acquired.slot, 1);

    slot0.abort();
    let _ = slot0.await;
    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 0, 1)).await.unwrap();
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(0), 1))
        .await
        .expect("later slot 0 waiter")
        .expect("slot 0 lease");
    assert_eq!(acquired.slot, 0);
}

#[tokio::test]
async fn a_lease_racing_waiter_cancellation_is_never_silently_dropped() {
    for _ in 0..50 {
        let (sender, receiver) = mpsc::channel(3);
        let pool = Arc::new(Pool::new(receiver));
        let waiter = tokio::spawn({
            let pool = pool.clone();
            async move { pool.receive(Some(0), 1).await }
        });
        tokio::task::yield_now().await;

        let (socket, _peer) = socket_pair().await;
        sender.send(lease(socket, 0, 1)).await.unwrap();
        waiter.abort();
        if let Ok(Some(_owned)) = waiter.await {
            // The waiter returned the lease; dropping it with the owner is correct.
            continue;
        }
        let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(0), 1))
            .await
            .expect("lease vanished when its waiter was cancelled")
            .expect("lease");
        assert_eq!(acquired.slot, 0);
    }
}

#[tokio::test]
async fn dropping_the_listener_wakes_every_waiter() {
    let (sender, receiver) = mpsc::channel(3);
    let pool = Arc::new(Pool::new(receiver));
    let mut waiters = Vec::new();
    for slot in 0..3u8 {
        let pool = pool.clone();
        waiters.push(tokio::spawn(
            async move { pool.receive(Some(slot), 1).await },
        ));
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(sender);
    for waiter in waiters {
        assert!(
            tokio::time::timeout(Duration::from_millis(500), waiter)
                .await
                .expect("waiter woke")
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn stale_generation_leases_are_discarded() {
    let (sender, receiver) = mpsc::channel(3);
    let pool = Arc::new(Pool::new(receiver));
    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 0, 0)).await.unwrap();
    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 0, 1)).await.unwrap();
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(0), 1))
        .await
        .expect("fresh generation")
        .expect("lease");
    assert_eq!(acquired.generation, 1);
}

#[tokio::test]
async fn old_generation_waiter_does_not_drop_a_newer_live_lease() {
    let (sender, receiver) = mpsc::channel(3);
    let pool = Arc::new(Pool::new(receiver));
    let slot0 = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(0), 1).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Park a slot 1 lease for a newer generation while the gen-1 waiter is
    // already blocked. The gen-1 waiter wakes first and must not discard it.
    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 1, 2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!slot0.is_finished(), "gen 1 slot 0 must still be waiting");

    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(1), 2))
        .await
        .expect("the gen-1 waiter dropped the gen-2 lease")
        .expect("gen 2 slot 1 lease");
    assert_eq!((acquired.generation, acquired.slot), (2, 1));
    assert!(
        !slot0.is_finished(),
        "gen 1 must remain blocked on slot 0 after gen 2 acquires slot 1"
    );

    // The overlapping generations must not wedge the original waiter.
    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 0, 1)).await.unwrap();
    let acquired = tokio::time::timeout(Duration::from_millis(500), slot0)
        .await
        .expect("gen 1 slot 0 lease")
        .unwrap()
        .expect("gen 1 slot 0 lease");
    assert_eq!((acquired.generation, acquired.slot), (1, 0));
}

#[tokio::test]
async fn stale_parked_slots_are_dropped_when_the_new_generation_waits() {
    let (sender, receiver) = mpsc::channel(3);
    let pool = Arc::new(Pool::new(receiver));
    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 1, 0)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 0, 1)).await.unwrap();
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(0), 1))
        .await
        .expect("fresh generation")
        .expect("lease");
    assert_eq!((acquired.generation, acquired.slot), (1, 0));

    drop(sender);
    let leftover = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(1), 1))
        .await
        .expect("closed pool");
    assert!(
        leftover.is_none(),
        "a stale parked lease must be dropped, not delivered"
    );
}

#[tokio::test]
async fn clear_drops_parked_and_queued_leases_but_accepts_new_ones() {
    let (sender, receiver) = mpsc::channel(3);
    let pool = Arc::new(Pool::new(receiver));
    for slot in [0, 1, 2] {
        let (socket, _peer) = socket_pair().await;
        sender.send(lease(socket, slot, 1)).await.unwrap();
    }
    pool.clear().await;
    for slot in [0, 1, 2] {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), pool.receive(Some(slot), 1))
                .await
                .is_err(),
            "a pre-clear lease survived clear"
        );
    }
    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 1, 1)).await.unwrap();
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(1), 1))
        .await
        .expect("post-clear lease")
        .expect("lease");
    assert_eq!(acquired.slot, 1);
}

#[tokio::test]
async fn clear_keeps_waiters_waiting_and_delivers_only_post_clear_leases() {
    let (sender, receiver) = mpsc::channel(3);
    let pool = Arc::new(Pool::new(receiver));
    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 1, 1)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let waiter = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(0), 1).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    pool.clear().await;

    assert!(
        tokio::time::timeout(Duration::from_millis(100), pool.receive(Some(1), 1))
            .await
            .is_err(),
        "the pre-clear lease must be dropped"
    );
    assert!(
        !waiter.is_finished(),
        "clear must not release waiting clients"
    );

    let (socket, _peer) = socket_pair().await;
    sender.send(lease(socket, 0, 1)).await.unwrap();
    let acquired = tokio::time::timeout(Duration::from_millis(500), waiter)
        .await
        .expect("post-clear lease")
        .unwrap()
        .expect("lease");
    assert_eq!(acquired.slot, 0);
}

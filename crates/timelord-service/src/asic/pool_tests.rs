use super::test_support::*;
use super::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn waiters_on_different_slots_do_not_block_each_other() {
    let (offers, pool) = pair();
    let slot0 = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(0), 1).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 1, 1));
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(1), 1))
        .await
        .expect("slot 1 must not wait behind slot 0")
        .expect("slot 1 lease");
    assert_eq!(acquired.slot, 1);
    assert!(!slot0.is_finished(), "slot 0 must still be waiting");

    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 0, 1));
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
    offer.offer(lease(socket, 0, 1));
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
    offer.offer(lease(socket, 1, 1));
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
    let (offers, pool) = pair();
    let waiter = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(0), 1).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    waiter.abort();
    let _ = waiter.await;

    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 0, 1));
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(0), 1))
        .await
        .expect("later waiter")
        .expect("lease");
    assert_eq!(acquired.slot, 0);
}

#[tokio::test]
async fn cancelling_the_waiter_polling_a_slot_does_not_wedge_other_slots() {
    let (offers, pool) = pair();
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
    offers.offer(lease(socket, 1, 1));
    let acquired = tokio::time::timeout(Duration::from_millis(500), slot1)
        .await
        .expect("slot 1 must acquire while slot 0 waits")
        .unwrap()
        .expect("slot 1 lease");
    assert_eq!(acquired.slot, 1);

    slot0.abort();
    let _ = slot0.await;
    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 0, 1));
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(0), 1))
        .await
        .expect("later slot 0 waiter")
        .expect("slot 0 lease");
    assert_eq!(acquired.slot, 0);
}

#[tokio::test]
async fn a_lease_racing_waiter_cancellation_is_never_silently_dropped() {
    for _ in 0..50 {
        let (offers, pool) = pair();
        let waiter = tokio::spawn({
            let pool = pool.clone();
            async move { pool.receive(Some(0), 1).await }
        });
        tokio::task::yield_now().await;

        let (socket, _peer) = socket_pair().await;
        offers.offer(lease(socket, 0, 1));
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
    let (offers, pool) = pair();
    let mut waiters = Vec::new();
    for slot in 0..3u8 {
        let pool = pool.clone();
        waiters.push(tokio::spawn(
            async move { pool.receive(Some(slot), 1).await },
        ));
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(offers);
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
    let (offers, pool) = pair();
    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 0, 0));
    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 0, 1));
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(0), 1))
        .await
        .expect("fresh generation")
        .expect("lease");
    assert_eq!(acquired.generation, 1);
}

#[tokio::test]
async fn old_generation_waiter_does_not_drop_a_newer_live_lease() {
    let (offers, pool) = pair();
    let slot0 = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(0), 1).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Park a slot 1 lease for a newer generation while the gen-1 waiter is
    // already blocked. The gen-1 waiter wakes first and must not discard it.
    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 1, 2));
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
    offers.offer(lease(socket, 0, 1));
    let acquired = tokio::time::timeout(Duration::from_millis(500), slot0)
        .await
        .expect("gen 1 slot 0 lease")
        .unwrap()
        .expect("gen 1 slot 0 lease");
    assert_eq!((acquired.generation, acquired.slot), (1, 0));
}

#[tokio::test]
async fn stale_parked_slots_are_dropped_when_the_new_generation_waits() {
    let (offers, pool) = pair();
    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 1, 0));
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 0, 1));
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(0), 1))
        .await
        .expect("fresh generation")
        .expect("lease");
    assert_eq!((acquired.generation, acquired.slot), (1, 0));

    drop(offers);
    let leftover = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(1), 1))
        .await
        .expect("closed pool");
    assert!(
        leftover.is_none(),
        "a stale parked lease must be dropped, not delivered"
    );
}

#[tokio::test]
async fn clear_drops_parked_leases_but_accepts_new_ones() {
    let (offers, pool) = pair();
    for slot in [0, 1, 2] {
        let (socket, _peer) = socket_pair().await;
        offers.offer(lease(socket, slot, 1));
    }
    pool.clear();
    for slot in [0, 1, 2] {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), pool.receive(Some(slot), 1))
                .await
                .is_err(),
            "a pre-clear lease survived clear"
        );
    }
    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 1, 1));
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(1), 1))
        .await
        .expect("post-clear lease")
        .expect("lease");
    assert_eq!(acquired.slot, 1);
}

#[tokio::test]
async fn clear_keeps_waiters_waiting_and_delivers_only_post_clear_leases() {
    let (offers, pool) = pair();
    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 1, 1));
    tokio::time::sleep(Duration::from_millis(20)).await;

    let waiter = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(0), 1).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    pool.clear();

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
    offers.offer(lease(socket, 0, 1));
    let acquired = tokio::time::timeout(Duration::from_millis(500), waiter)
        .await
        .expect("post-clear lease")
        .unwrap()
        .expect("lease");
    assert_eq!(acquired.slot, 0);
}

#[tokio::test]
async fn a_matching_lease_offered_after_the_first_poll_is_not_lost() {
    let (offers, pool) = pair();
    let (socket, _peer) = socket_pair().await;
    // Interleave right between the empty recheck and `notified.await`, the only
    // window a whole-future poll cannot reach. `enable()` has already registered
    // the `Notified`, so `notify_waiters` must wake the pending receive.
    pool.on_wait(move || offers.offer(lease(socket, 0, 1)));
    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(0), 1))
        .await
        .expect("the matching lease offered before the await did not wake receive")
        .expect("lease");
    assert_eq!(acquired.slot, 0);
}

#[tokio::test]
async fn a_newer_generation_prunes_older_parked_leases_and_releases_their_claim() {
    let (offers, pool) = pair();
    let clients = Arc::new(SyncMutex::new(BTreeSet::new()));
    let gen1 = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(0), 1).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // A gen-1 slot-1 lease is parked but cannot be used by the slot-0 waiter.
    let (socket, _peer) = socket_pair().await;
    offers.offer(claimed(socket, 1, 1, clients.clone()));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(clients.lock().unwrap().contains(&("a".to_string(), 1)));

    // The gen-2 waiter must prune it, releasing its (board, slot) claim.
    let gen2 = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(1), 2).await }
    });
    tokio::time::timeout(Duration::from_millis(500), async {
        while clients.lock().unwrap().contains(&("a".to_string(), 1)) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("a stale gen-1 lease stayed pinned by its generation");

    let (socket, _peer) = socket_pair().await;
    offers.offer(claimed(socket, 1, 2, clients.clone()));
    let acquired = tokio::time::timeout(Duration::from_millis(500), gen2)
        .await
        .expect("gen 2 waiter")
        .unwrap()
        .expect("gen 2 slot 1 lease");
    assert_eq!((acquired.generation, acquired.slot), (2, 1));
    assert!(!gen1.is_finished(), "gen 1 slot 0 must still be waiting");

    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 0, 1));
    let acquired = tokio::time::timeout(Duration::from_millis(500), gen1)
        .await
        .expect("gen 1 waiter")
        .unwrap()
        .expect("gen 1 slot 0 lease");
    assert_eq!((acquired.generation, acquired.slot), (1, 0));
}

#[tokio::test]
async fn a_waiter_never_takes_an_older_generation_lease() {
    let (offers, pool) = pair();
    let clients = Arc::new(SyncMutex::new(BTreeSet::new()));
    let (socket, _peer) = socket_pair().await;
    offers.offer(claimed(socket, 0, 0, clients.clone()));

    let waiter = tokio::spawn({
        let pool = pool.clone();
        async move { pool.receive(Some(0), 1).await }
    });
    tokio::time::timeout(Duration::from_millis(500), async {
        while clients.lock().unwrap().contains(&("a".to_string(), 0)) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("an older-generation lease was not pruned");
    assert!(!waiter.is_finished(), "the older lease was accepted");

    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 0, 1));
    let acquired = tokio::time::timeout(Duration::from_millis(500), waiter)
        .await
        .expect("matching-generation lease")
        .unwrap()
        .expect("lease");
    assert_eq!(acquired.generation, 1);
}

#[tokio::test]
async fn a_parked_matching_lease_is_delivered_after_close() {
    let (offers, pool) = pair();
    let (socket, _peer) = socket_pair().await;
    offers.offer(lease(socket, 0, 1));
    drop(offers);

    let acquired = tokio::time::timeout(Duration::from_millis(500), pool.receive(Some(0), 1))
        .await
        .expect("a closed pool hung instead of delivering its parked lease")
        .expect("parked lease");
    assert_eq!(acquired.slot, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), pool.receive(Some(0), 1))
            .await
            .expect("a closed pool must return None promptly")
            .is_none()
    );
}

#[tokio::test]
async fn clear_after_close_drops_parked_leases_and_receive_returns_none() {
    let (offers, pool) = pair();
    let clients = Arc::new(SyncMutex::new(BTreeSet::new()));
    let (socket, _peer) = socket_pair().await;
    offers.offer(claimed(socket, 0, 1, clients.clone()));
    drop(offers);

    pool.clear();
    assert!(clients.lock().unwrap().is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), pool.receive(Some(0), 1))
            .await
            .expect("clear after close must not hang")
            .is_none()
    );
}

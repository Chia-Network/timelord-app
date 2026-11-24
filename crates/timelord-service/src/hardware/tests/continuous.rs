use crate::{
    Shared,
    asic::AsicBackend,
    hardware::continuous,
    operational,
    runtime::{HardwareState, RuntimeState},
    safety,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use timelord_core::{BoardConfig, Chain, Config};
use tokio::{net::TcpListener, sync::watch};

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(tag: &str) -> Dir {
    let path = std::env::temp_dir().join(format!(
        "tl-continuous-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    Dir(path)
}

fn board(id: &str, port: u16) -> BoardConfig {
    toml::from_str(&format!(
        "id='{id}'\nname='{id}'\nserial='{id}'\nprofile='manual'\ndevice_location=1\nclient_port={port}"
    ))
    .unwrap()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn backend_fixture(shared: &Shared) -> Arc<AsicBackend> {
    let backend = Arc::new(AsicBackend::new(
        shared.runtime.clone(),
        shared.process.clone(),
    ));
    shared.runtime.lock().unwrap().prover = Arc::downgrade(&backend);
    backend
}

fn fixture(
    config: Config,
    safety: Option<Arc<safety::Store>>,
) -> (
    Arc<Shared>,
    watch::Sender<Arc<Shared>>,
    watch::Receiver<Arc<Shared>>,
) {
    let mut runtime = RuntimeState::new(false);
    runtime.safety = safety;
    let shared = Arc::new(Shared::new(
        None,
        config,
        Arc::new(std::sync::Mutex::new(runtime)),
        operational::State::fixture(),
        crate::unix_now(),
        crate::unix_now(),
    ));
    let (updates, receiver) = watch::channel(shared.clone());
    (shared, updates, receiver)
}

fn with_boards(shared: &Shared, boards: Vec<BoardConfig>) -> Arc<Shared> {
    let mut config = shared.config.clone();
    config.hardware.boards = boards;
    Arc::new(Shared {
        native: shared.native.clone(),
        config,
        clients: shared.clients.clone(),
        process: shared.process.clone(),
        scope: shared.scope,
        runtime: shared.runtime.clone(),
        started_unix: shared.started_unix,
        service_started_unix: shared.service_started_unix,
    })
}

fn port_available(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

async fn wait_until(message: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect(message);
}

#[tokio::test]
async fn a_retired_board_is_re_added_without_restarting_the_owner_loop() {
    let dir = temp_dir("readd");
    let store = Arc::new(safety::Store::open(&dir.0.join("config.toml")).unwrap());
    let board = board("readd", free_port());
    let mut config = Config::default();
    config.hardware.boards = vec![board.clone()];
    let (shared, updates, receiver) = fixture(config, Some(store));
    let backend = backend_fixture(&shared);
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(continuous::run(shared.clone(), backend, receiver, shutdown));

    wait_until("board listener never came up", || {
        !port_available(board.client_port)
    })
    .await;
    updates.send_replace(with_boards(&shared, Vec::new()));
    wait_until("retired board kept its listener", || {
        port_available(board.client_port)
    })
    .await;
    updates.send_replace(with_boards(&shared, vec![board.clone()]));
    wait_until("re-added board never listened", || {
        !port_available(board.client_port)
    })
    .await;

    stop.send(true).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("continuous loop hung on shutdown")
        .unwrap();
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn a_busy_client_port_is_retried_until_it_frees() {
    let dir = temp_dir("port-retry");
    let store = Arc::new(safety::Store::open(&dir.0.join("config.toml")).unwrap());
    let blocker = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = blocker.local_addr().unwrap().port();
    let board = board("retry", port);
    let mut config = Config::default();
    config.hardware.boards = vec![board.clone()];
    let (shared, _updates, receiver) = fixture(config, Some(store));
    let backend = backend_fixture(&shared);
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(continuous::run(shared.clone(), backend, receiver, shutdown));

    wait_until("board did not report a blocked client port", || {
        let state = shared.runtime.lock().unwrap();
        state.boards.get(&board.id).is_some_and(|hardware| {
            hardware
                .recovery_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("Waiting for ASIC client port"))
                && hardware.retry_after_unix.is_some()
        })
    })
    .await;
    let first_retry = shared.runtime.lock().unwrap().boards[&board.id]
        .retry_after_unix
        .unwrap();
    // A later timestamp means the loop bound again after the unix second advanced.
    wait_until("the owner loop stopped retrying the client port", || {
        shared
            .runtime
            .lock()
            .unwrap()
            .boards
            .get(&board.id)
            .is_some_and(|hardware| {
                hardware
                    .retry_after_unix
                    .is_some_and(|later| later > first_retry)
            })
    })
    .await;
    assert!(!task.is_finished(), "bind failure ended the owner loop");

    drop(blocker);
    wait_until("board never bound the freed client port", || {
        !port_available(port)
    })
    .await;

    stop.send(true).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("continuous loop hung on shutdown")
        .unwrap();
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn an_owner_group_that_exits_fails_the_continuous_loop() {
    let board = board("owner-exit", free_port());
    let mut config = Config::default();
    config.hardware.boards = vec![board];
    // No safety journal: `supervise_board_with` returns immediately, which is
    // what ends the group before `run` can take its retire path.
    let (shared, _updates, receiver) = fixture(config, None);
    let backend = backend_fixture(&shared);
    let (_stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(continuous::run(shared, backend, receiver, shutdown));

    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("continuous loop hung after the owner group exited")
        .unwrap();
    let error = result.expect_err("owner group exit must fail the loop");
    let chain = format!("{error:#}");
    assert!(
        chain.contains("A persistent board owner exited unexpectedly"),
        "{chain}"
    );
    assert!(
        !chain.contains("Board owner group exited unexpectedly"),
        "{chain}"
    );
}

#[tokio::test]
async fn a_retired_group_that_already_exited_reports_the_inner_failure() {
    // `run` binds the port itself, so a port handed out by `free_port` can be
    // stolen by a parallel test before `run` gets to it. A busy port alone is
    // not proof of ownership: the bind-failure path leaves a retry marker. Retry
    // the setup with a fresh port until this `run` really owns a listener.
    //
    // Freeze time before polling `run`. The interval's first tick is not
    // guaranteed to be ready on the first poll (the timer wheel rounds to the
    // next millisecond), and a tick left pending across `pause()` can later win
    // the unbiased `select!` before the retirement update. Pausing first keeps
    // any such pending tick from firing. The keepalive stops `pause()` from
    // auto-advancing to it while the setup is polled.
    tokio::time::pause();
    let keep_alive = tokio::spawn(std::future::pending::<()>());

    let mut attempt = 0;
    let (shared, updates, board, _stop, mut run) = loop {
        attempt += 1;
        assert!(
            attempt <= 20,
            "could not start the owner group: every client port was stolen"
        );
        let board = board("retire-exit", free_port());
        let mut config = Config::default();
        config.hardware.boards = vec![board.clone()];
        let (shared, updates, receiver) = fixture(config, None);
        let backend = backend_fixture(&shared);
        let (_stop, shutdown) = watch::channel(false);
        let mut run = Box::pin(continuous::run(shared.clone(), backend, receiver, shutdown));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let retried = loop {
            // Poll only until the owner has attempted the bind and parked in
            // its unbiased `select!` waiting for shutdown, updates or a tick.
            tokio::select! {
                biased;
                result = &mut run => {
                    panic!("continuous loop exited before the owner started: {result:?}")
                }
                _ = std::future::ready(()) => {}
            }
            let retried = {
                let state = shared.runtime.lock().unwrap();
                state.boards.get(&board.id).is_some_and(|hardware| {
                    hardware.retry_after_unix.is_some()
                        || hardware
                            .recovery_reason
                            .as_deref()
                            .is_some_and(|reason| reason.contains("Waiting for ASIC client port"))
                })
            };
            if retried {
                break true;
            }
            if !port_available(board.client_port) {
                break false;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the owner group never attempted to bind its port"
            );
            tokio::task::yield_now().await;
        };
        if retried {
            continue;
        }
        assert!(
            !port_available(board.client_port),
            "the owner group did not bind its port"
        );
        break (shared, updates, board, _stop, run);
    };
    let waiting = std::time::Instant::now();
    while !port_available(board.client_port) {
        assert!(
            waiting.elapsed() < Duration::from_secs(5),
            "the owner group never exited on its own"
        );
        tokio::task::yield_now().await;
    }

    // Retire the already-finished group while `run` is still parked and time is
    // paused, so only the already-ready watch update can wake `select!`. The
    // next iteration takes the retire path and `ensure!(stopping)` fails.
    updates.send_replace(with_boards(&shared, Vec::new()));
    let waiting = std::time::Instant::now();
    let result = loop {
        tokio::select! {
            biased;
            result = &mut run => break result,
            _ = std::future::ready(()) => {
                assert!(
                    waiting.elapsed() < Duration::from_secs(5),
                    "continuous loop hung after the owner group exited"
                );
                tokio::task::yield_now().await;
            }
        }
    };
    keep_alive.abort();
    let error = result.expect_err("a retired finished group must fail the loop");
    let chain = format!("{error:#}");
    assert!(
        chain.contains("Board owner group exited unexpectedly"),
        "{chain}"
    );
    assert!(
        !chain.contains("A persistent board owner exited unexpectedly"),
        "{chain}"
    );
}

fn reconcile_fixture(dir: &Dir) -> (Shared, Arc<safety::Store>) {
    let store = Arc::new(safety::Store::open(&dir.0.join("config.toml")).unwrap());
    let mut config = Config::default();
    config.hardware.boards = (0..3)
        .map(|i| {
            toml::from_str(&format!(
                "id='b{i}'\nname='B{i}'\nserial='S{i}'\nprofile='manual'\nclient_port={}",
                18110 + i
            ))
            .unwrap()
        })
        .collect();
    let mut runtime = RuntimeState::new(false);
    runtime.safety = Some(store.clone());
    runtime.scheduler_initialized = true;
    for board in &config.hardware.boards {
        let mut hardware = HardwareState::default();
        hardware.lifecycle.testing().serve(1);
        runtime.boards.insert(board.id.clone(), hardware);
    }
    let shared = Shared::new(
        None,
        config,
        Arc::new(std::sync::Mutex::new(runtime)),
        operational::State::fixture(),
        1,
        1,
    );
    (shared, store)
}

#[test]
fn a_board_failure_moves_only_its_chain_without_erasing_history() {
    let dir = temp_dir("reconcile-failure");
    let (shared, _store) = reconcile_fixture(&dir);
    let mut rebalance = continuous::Rebalance::default();
    continuous::reconcile(&shared, &mut rebalance).unwrap();
    let initial = shared.assignments();
    assert_eq!(initial.len(), 3);
    shared
        .runtime
        .lock()
        .unwrap()
        .estimated(Chain::Reward, 1.0e6, 1, "fixture", 1);
    let lost = initial
        .iter()
        .find(|a| a.chain == Chain::Reward)
        .unwrap()
        .board_id
        .clone();
    shared
        .runtime
        .lock()
        .unwrap()
        .boards
        .get_mut(&lost)
        .unwrap()
        .lifecycle
        .testing()
        .fault(crate::board_lifecycle::Fault::Transport, "lost".into(), 2);
    continuous::reconcile(&shared, &mut rebalance).unwrap();
    let state = shared.runtime.lock().unwrap();
    assert!(state.routing.iter().all(|a| a.board_id != lost));
    for chain in [Chain::Challenge, Chain::InfusedChallenge] {
        assert_eq!(
            state.routing.iter().find(|a| a.chain == chain),
            initial.iter().find(|a| a.chain == chain)
        );
    }
    assert_eq!(state.estimated_observations.iter().count(), 1);
    assert!(state.disturbed_until_unix > crate::unix_now());
}

#[test]
fn a_failing_board_keeps_working_and_hands_its_critical_chain_over_at_a_peak() {
    let dir = temp_dir("reconcile-failing");
    let (shared, store) = reconcile_fixture(&dir);
    let mut rebalance = continuous::Rebalance::default();
    continuous::reconcile(&shared, &mut rebalance).unwrap();
    let holder = |chain: Chain| {
        shared
            .assignments()
            .into_iter()
            .find(|a| a.chain == chain)
            .unwrap()
            .board_id
    };
    let critical = holder(Chain::Challenge);
    let serial = match critical.as_str() {
        "b0" => "S0",
        "b1" => "S1",
        _ => "S2",
    };
    for _ in 0..3 {
        store
            .record_failure(serial, "Waiting for ASIC client timed out")
            .unwrap();
    }
    // Still serving: nothing moves between peaks and it is never idled.
    let before = shared.assignments();
    continuous::reconcile(&shared, &mut rebalance).unwrap();
    assert_eq!(shared.assignments(), before);
    shared.runtime.lock().unwrap().peak_epoch += 1;
    continuous::reconcile(&shared, &mut rebalance).unwrap();
    assert_eq!(holder(Chain::InfusedChallenge), critical);
    assert_ne!(holder(Chain::Challenge), critical);
    // The operator's retry clears the streak; it is preferred again.
    store
        .update_health(serial, true, |h| h.retry_now(crate::unix_now()))
        .unwrap();
    assert_eq!(store.health(serial).consecutive_failures, 0);
}

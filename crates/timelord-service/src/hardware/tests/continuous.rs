use crate::{
    Shared,
    asic::AsicBackend,
    hardware::continuous,
    operational,
    runtime::{HardwareState, RuntimeState},
    safety,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
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
async fn retire_stale_groups_surfaces_a_failed_owners_inner_failure() {
    let (shared, _updates, _receiver) = fixture(Config::default(), None);
    let backend = backend_fixture(&shared);
    let task = tokio::spawn(async {
        Err::<(), anyhow::Error>(anyhow::anyhow!("Board owner group exited unexpectedly"))
    });
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
    let board = board("retire-failure", 0);
    let (stop, _receiver) = watch::channel(false);
    let mut groups = HashMap::from([(board.id.clone(), continuous::Group { board, stop, task })]);
    let error = continuous::retire_stale_groups(&mut groups, &[], &backend)
        .await
        .expect_err("a failed retired owner must surface its inner failure");
    let chain = format!("{error:#}");
    assert!(
        chain.contains("Board owner group exited unexpectedly"),
        "{chain}"
    );
    assert!(groups.is_empty());
}

#[tokio::test]
async fn own_group_reports_the_first_task_join_failure() {
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async {
        panic!("fixture owner task failed");
    });
    let (stop, _receiver) = watch::channel(false);
    let error = continuous::own_group(tasks, stop)
        .await
        .expect_err("a panicking owner task must fail the group");
    let chain = format!("{error:#}");
    assert!(chain.contains("Board group failed"), "{chain}");
}

#[tokio::test]
async fn own_group_fails_when_a_task_exits_without_a_stop_request() {
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async {});
    let (stop, _receiver) = watch::channel(false);
    let error = continuous::own_group(tasks, stop)
        .await
        .expect_err("an unexpected owner exit must fail the group");
    let chain = format!("{error:#}");
    assert!(
        chain.contains("Board owner group exited unexpectedly"),
        "{chain}"
    );
}

#[tokio::test]
async fn own_group_accepts_a_finished_task_after_a_stop_request() {
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async {});
    let (stop, _receiver) = watch::channel(false);
    stop.send(true).unwrap();
    continuous::own_group(tasks, stop)
        .await
        .expect("a stop request explains the owner exit");
}

#[tokio::test]
async fn a_retired_owner_reports_the_inner_group_failure() {
    let task = tokio::spawn(async {
        Err::<(), anyhow::Error>(anyhow::anyhow!("Board owner group exited unexpectedly"))
    });
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
    let error = continuous::join_retired_owner(task)
        .await
        .expect_err("the retired owner's inner failure must surface");
    let chain = format!("{error:#}");
    assert!(
        chain.contains("Board owner group exited unexpectedly"),
        "{chain}"
    );
}

#[tokio::test]
async fn a_retired_owner_reports_a_join_failure() {
    let task: tokio::task::JoinHandle<anyhow::Result<()>> = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    task.abort();
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
    let error = continuous::join_retired_owner(task)
        .await
        .expect_err("a cancelled owner task must fail the retire path");
    let chain = format!("{error:#}");
    assert!(chain.contains("Board owner group failed"), "{chain}");
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

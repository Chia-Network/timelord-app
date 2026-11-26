use super::*;

struct Dir(std::path::PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn supervisor_fixture(
    tag: &str,
) -> (
    BoardSupervisor,
    Arc<crate::safety::Store>,
    std::path::PathBuf,
    Dir,
) {
    let dir = Dir(std::env::temp_dir().join(format!(
        "tl-unplug-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )));
    std::fs::create_dir_all(&dir.0).unwrap();
    let binary = dir.0.join("client");
    let board: BoardConfig = toml::from_str(
        "id='unplug'\nname='Unplug'\nserial='unplug-0'\nprofile='manual'\ndevice_location=1\nclient_port=18110",
    )
    .unwrap();
    let mut config = Config::default();
    config.hardware.client_binary = Some(binary.clone());
    config.hardware.boards = vec![board.clone()];
    let store = Arc::new(crate::safety::Store::open(&dir.0.join("config.toml")).unwrap());
    let mut runtime = crate::runtime::RuntimeState::new(false);
    runtime.safety = Some(store.clone());
    let shared = Arc::new(crate::Shared::new(
        None,
        config,
        Arc::new(std::sync::Mutex::new(runtime)),
        crate::operational::State::fixture(),
        0,
        0,
    ));
    let backend = Arc::new(crate::asic::AsicBackend::new(
        shared.runtime.clone(),
        shared.process.clone(),
    ));
    let (_stop, shutdown) = watch::channel(false);
    let supervisor = BoardSupervisor {
        shared,
        backend,
        board,
        shutdown,
        safety: store.clone(),
        previous_failure: crate::observability::RestartReason::Topology,
        timing: Timing::default(),
        forgivable: None,
    };
    (supervisor, store, binary, dir)
}

#[tokio::test]
async fn initialization_wait_cancels_without_waiting_for_another_native_owner() {
    let _held = INITIALIZATION.lock().await;
    let (stop, mut shutdown) = watch::channel(false);
    let wait = acquire_initialization(&mut shutdown);
    tokio::pin!(wait);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut wait)
            .await
            .is_err()
    );
    stop.send(true).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .unwrap()
            .is_none()
    );
    let mut already_stopped = stop.subscribe();
    assert!(acquire_initialization(&mut already_stopped).await.is_none());
}

#[tokio::test]
async fn recover_observes_unplug_without_waiting_for_initialization() {
    let (mut supervisor, store, binary, _dir) = supervisor_fixture("unplug");
    crate::inventory::fixture::set(binary, vec![]);
    let initialization = INITIALIZATION.lock().await;
    tokio::time::timeout(
        Duration::from_secs(5),
        supervisor.recover(anyhow::anyhow!("native client exited")),
    )
    .await
    .expect("recover must not wait for INITIALIZATION");
    assert_eq!(store.health("unplug-0").consecutive_failures, 0);
    let state = supervisor.shared.runtime.lock().expect("runtime");
    let board = &state.boards[&supervisor.board.id];
    assert_eq!(
        board.recovery_reason.as_deref(),
        Some("Unplug was unplugged")
    );
    assert_eq!(board.lifecycle.inventory_present(), Some(false));
    drop(initialization);
}

#[tokio::test]
async fn recover_counts_a_usb_discovery_error_as_a_failure() {
    let (mut supervisor, store, binary, _dir) = supervisor_fixture("discover-error");
    crate::inventory::fixture::fail(binary);
    supervisor
        .recover(anyhow::anyhow!("native client exited"))
        .await;
    assert_eq!(store.health("unplug-0").consecutive_failures, 1);
    assert!(supervisor.forgivable.is_some());
}

#[tokio::test(start_paused = true)]
async fn recover_counts_a_timed_out_usb_check_as_a_failure() {
    let (mut supervisor, store, binary, _dir) = supervisor_fixture("discover-timeout");
    crate::inventory::fixture::hang(binary);
    let recovering = tokio::spawn(async move {
        supervisor
            .recover(anyhow::anyhow!("native client exited"))
            .await;
        supervisor
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(15)).await;
    let supervisor = recovering.await.unwrap();
    assert_eq!(store.health("unplug-0").consecutive_failures, 1);
    assert!(supervisor.forgivable.is_some());
}

#[test]
fn restart_delays_follow_the_persisted_failure_streak_and_never_exhaust() {
    let timing = Timing::default();
    let mut health = crate::hardware::health::Health::default();
    let mut delays = vec![];
    for t in 0..6 {
        health.record_failure(t, "x");
        delays.push(timing.restart_delay(&health).as_secs());
    }
    assert_eq!(delays, [1, 5, 30, 30, 30, 30]);
}

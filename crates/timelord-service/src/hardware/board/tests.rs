use super::*;

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
    struct Dir(std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = Dir(std::env::temp_dir().join(format!(
        "tl-unplug-race-{}-{}",
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
    let mut supervisor = BoardSupervisor {
        shared,
        backend,
        board: board.clone(),
        shutdown,
        safety: store.clone(),
        previous_failure: crate::observability::RestartReason::Topology,
        timing: Timing::default(),
        forgivable: None,
    };
    let device = crate::inventory::Device {
        name: "Chia VDF A".into(),
        location: 1,
        serial: Some("unplug-0".into()),
    };
    crate::inventory::fixture::set(binary.clone(), vec![device.clone()]);
    crate::inventory::fixture::set(binary.clone(), vec![]);
    let initialization = INITIALIZATION.lock().await;
    let recovering = supervisor.recover(anyhow::anyhow!("native client exited"));
    tokio::pin!(recovering);
    tokio::time::timeout(Duration::from_millis(20), &mut recovering)
        .await
        .expect("recover must not wait for INITIALIZATION");
    assert_eq!(store.health("unplug-0").consecutive_failures, 0);
    crate::inventory::fixture::set(binary, vec![device]);
    drop(initialization);
    assert_eq!(store.health("unplug-0").consecutive_failures, 0);
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

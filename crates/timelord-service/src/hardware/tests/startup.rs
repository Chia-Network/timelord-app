//! Native setup and readiness budgets retain initialization ownership.
use super::*;

#[tokio::test]
async fn temperature_only_client_releases_initialization_but_is_retired_without_network_readiness()
{
    let board: BoardConfig = toml::from_str(
        "id='temperature-only'\nname='Temperature only'\nclient_port=18110\nprofile='manual'",
    )
    .unwrap();
    let shared = Arc::new(Shared {
        native: None,
        config: Default::default(),
        assignments: vec![],
        clients: Default::default(),
        process: crate::operational::State::fixture(),
        scope: crate::runtime::RuntimeScope::Production,
        runtime: Default::default(),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    });
    let child = Command::new("/bin/sh")
        .args([
            "-c",
            r#"read command version id frequency voltage
printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
while :; do printf '%s\n' 'ASIC Temp = 40 C; Frequency = 1450 MHz' >&2; sleep .01; done &
logger=$!
cat >/dev/null
kill "$logger"
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2"#,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    // An independent lock makes this fixture deterministic alongside other
    // native-client tests, without changing the production lock contract.
    let initialization: &'static tokio::sync::Mutex<()> =
        Box::leak(Box::new(tokio::sync::Mutex::new(())));
    let (ready, initialized) = oneshot::channel();
    let (_stop, rx) = watch::channel(false);
    let startup = startup(&shared, &board).await.unwrap();
    let owner = tokio::spawn(supervise(
        shared.clone(),
        board.clone(),
        child,
        ready,
        rx,
        initialization.lock().await,
        None,
        startup,
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        while !shared
            .runtime
            .lock()
            .unwrap()
            .boards
            .get(&board.id)
            .is_some_and(|b| b.temperature_c == Some(40.))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        initialization.try_lock().is_ok(),
        "valid telemetry releases USB preparation independently of readiness"
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    let error = tokio::time::timeout(Duration::from_secs(3), owner)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("initialization timed out"));
    assert!(initialized.await.is_err());
    assert!(initialization.try_lock().is_ok());
    assert!(
        shared.runtime.lock().unwrap().boards[&board.id]
            .lifecycle
            .status()
            .quiesced
    );
    assert_eq!(
        unsafe { libc::kill(pid as i32, 0) },
        -1,
        "owner was not reaped"
    );
}

#[tokio::test]
async fn slow_valid_native_setup_receives_a_fresh_readiness_window() {
    for profile in [ProfileMode::Manual, ProfileMode::Automatic] {
        slow_native_setup(profile).await;
    }
}
async fn slow_native_setup(profile: ProfileMode) {
    let dir = std::env::temp_dir().join(format!(
        "tl-native-ready-budget-{}-{profile:?}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let script = format!(
        r#"read command version id frequency voltage
printf started > '{root}/started'
while [ ! -f '{root}/ack' ]; do sleep .01; done
printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
while [ ! -f '{root}/temperature' ]; do sleep .01; done
printf '%s\n' 'ASIC Temp = 40 C; Frequency = 1450 MHz' >&2
while [ ! -f '{root}/ready' ]; do sleep .01; done
printf '%s\n' 'Connected to timelord, waiting for challenge' >&2
cat >/dev/null
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#,
        root = dir.display()
    );
    let child = Command::new("/bin/sh")
        .args(["-c", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut board: BoardConfig =
        toml::from_str("id='slow-setup'\nname='Slow setup'\nclient_port=18110\nprofile='manual'")
            .unwrap();
    board.profile = profile;
    board.serial = Some("fixture".into());
    let shared = Arc::new(Shared {
        native: Some(Arc::new(Facts {
            hash: "fixture".into(),
            host: "fixture".into(),
            frequencies: vec![1450.],
        })),
        config: Default::default(),
        assignments: vec![],
        clients: Default::default(),
        process: crate::operational::State::fixture(),
        scope: crate::runtime::RuntimeScope::Production,
        runtime: Default::default(),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    });
    let progress = shared.process.progress.clone();
    let preparing = progress.begin(
        timelord_supervisor::progress::Resource::BoardPreparation(
            timelord_supervisor::progress::BoardSlot::First,
        ),
        1,
        0,
        timelord_supervisor::progress::Stage::BoardPreparation,
    );
    let initialization: &'static tokio::sync::Mutex<()> =
        Box::leak(Box::new(tokio::sync::Mutex::new(())));
    let (ready, mut initialized) = oneshot::channel();
    let (stop, rx) = watch::channel(false);
    let startup = startup(&shared, &board).await.unwrap();
    let owner = tokio::spawn(supervise(
        shared.clone(),
        board.clone(),
        child,
        ready,
        rx,
        initialization.lock().await,
        Some(preparing),
        startup,
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        while !dir.join("started").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        progress
            .snapshot(1, 1)
            .unwrap()
            .work
            .iter()
            .flatten()
            .any(|work| work.stage == timelord_supervisor::progress::Stage::BoardSetup)
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(29)).await;
    std::fs::write(dir.join("ack"), "release setup").unwrap();
    // Keep virtual time fixed while the real fixture exchanges its ACK.
    let waiting = std::time::Instant::now();
    while !progress
        .snapshot(1, 2)
        .unwrap()
        .work
        .iter()
        .flatten()
        .any(|work| work.stage == timelord_supervisor::progress::Stage::BoardReadiness)
    {
        assert!(
            waiting.elapsed() < Duration::from_secs(3),
            "setup ACK was not accepted"
        );
        tokio::task::yield_now().await;
    }
    assert_eq!(
        initialization.try_lock().is_ok(),
        profile == ProfileMode::Automatic,
        "Automatic ACK releases USB setup; Manual requires hardware telemetry"
    );
    assert!(matches!(
        initialized.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    std::fs::write(dir.join("temperature"), "release telemetry").unwrap();
    let waiting = std::time::Instant::now();
    while !shared
        .runtime
        .lock()
        .unwrap()
        .boards
        .get(&board.id)
        .is_some_and(|hardware| hardware.temperature_c == Some(40.))
    {
        assert!(
            waiting.elapsed() < Duration::from_secs(3),
            "temperature was not consumed"
        );
        tokio::task::yield_now().await;
    }
    assert!(
        progress
            .snapshot(1, 2)
            .unwrap()
            .work
            .iter()
            .flatten()
            .any(|work| work.stage == timelord_supervisor::progress::Stage::BoardReadiness)
    );
    assert!(initialization.try_lock().is_ok());
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    std::fs::write(dir.join("ready"), "release readiness").unwrap();
    let waiting = std::time::Instant::now();
    loop {
        match initialized.try_recv() {
            Ok(()) => break,
            Err(oneshot::error::TryRecvError::Empty) => {
                assert!(
                    waiting.elapsed() < Duration::from_secs(3),
                    "ready signal did not arrive"
                );
                tokio::task::yield_now().await;
            }
            Err(error) => panic!("valid ready signal lost after slow setup: {error}"),
        }
    }
    tokio::time::resume();
    assert!(initialization.try_lock().is_ok());
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), owner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        shared.runtime.lock().unwrap().boards[&board.id]
            .lifecycle
            .status()
            .quiesced
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn thermal_initialization_after_another_fault_records_the_thermal_stop() {
    let board: BoardConfig = toml::from_str(
        "id='hot-after-fault'\nname='Hot after fault'\nserial='fixture'\nclient_port=18110\ndevice_location=1\nprofile='manual'",
    )
    .unwrap();
    let mut config = Config::default();
    config.hardware.boards = vec![board.clone()];
    let runtime = {
        let mut state = crate::runtime::RuntimeState::default();
        state.boards.insert(
            board.id.clone(),
            crate::runtime::HardwareState {
                lifecycle: crate::board_lifecycle::BoardLifecycle::fixture(
                    crate::board_lifecycle::Phase::Fault,
                    false,
                    Some(crate::board_lifecycle::Fault::InvalidProof),
                    Some(true),
                    1,
                ),
                ..Default::default()
            },
        );
        state
    };
    let shared = Arc::new(Shared {
        native: None,
        assignments: config.assignments().unwrap(),
        config,
        clients: Default::default(),
        process: crate::operational::State::fixture(),
        scope: crate::runtime::RuntimeScope::Production,
        runtime: Arc::new(std::sync::Mutex::new(runtime)),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    });
    let child = Command::new("/bin/sh")
        .args([
            "-c",
            r#"read command version id frequency voltage
printf 'TLERR 1 %s temperature_guard\n' "$id"
cat >/dev/null
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2"#,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let startup = startup(&shared, &board).await.unwrap();
    let (ready, _ready) = oneshot::channel();
    let (_stop, rx) = watch::channel(false);
    let initialization: &'static tokio::sync::Mutex<()> =
        Box::leak(Box::new(tokio::sync::Mutex::new(())));
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        supervise(
            shared.clone(),
            board.clone(),
            child,
            ready,
            rx,
            initialization.lock().await,
            None,
            startup,
        ),
    )
    .await
    .expect("thermal initialization did not retire the native owner")
    .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<NativeFailure>()
            .map(|failure| failure.kind),
        Some(NativeFailureKind::Thermal)
    );
    let state = shared.runtime.lock().unwrap();
    let hardware = &state.boards[&board.id];
    assert_eq!(
        hardware.lifecycle.status().fault,
        Some(crate::board_lifecycle::Fault::Thermal)
    );
    assert_eq!(
        state
            .events
            .iter()
            .filter(|event| {
                event.board_id.as_deref() == Some(board.id.as_str()) && event.kind == "thermal_stop"
            })
            .count(),
        1
    );
}

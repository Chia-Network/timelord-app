use super::*;

#[tokio::test]
async fn thermal_shutdown_does_not_wait_for_the_safety_journal() {
    let dir = std::env::temp_dir().join(format!("tl-thermal-journal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("client");
    crate::test_scripts::write_executable_script(
        &binary,
        r#"#!/bin/sh
while read command version id frequency voltage; do
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
 printf '%s\n' 'Connected to timelord, waiting for challenge' 'ASIC Temp = 60 C; Frequency = 1450 MHz' >&2
done
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#,
    );
    let board:BoardConfig=toml::from_str("id='hot'\nname='Hot'\nserial='fixture'\nclient_port=18110\ndevice_location=1\nprofile='manual'").unwrap();
    let mut config = Config::default();
    config.hardware.client_binary = Some(binary);
    config.hardware.boards = vec![board.clone()];
    let store = Arc::new(crate::safety::Store::open(&dir.join("config.toml")).unwrap());
    store
        .begin_owner(
            "fixture",
            Point {
                frequency_mhz: 1450.,
                voltage_v: 1.,
            },
        )
        .unwrap();
    let runtime = {
        let mut state = crate::runtime::RuntimeState::default();
        state.safety = Some(store.clone());
        state
    };
    let shared = Arc::new(
        crate::Shared::new(
            if config
                .hardware
                .boards
                .iter()
                .any(|b| b.profile == ProfileMode::Automatic)
            {
                Some(Facts::load(&config).await.unwrap())
            } else {
                None
            },
            config,
            Arc::new(std::sync::Mutex::new(runtime)),
            crate::operational::State::fixture(),
            crate::unix_now(),
            crate::unix_now(),
        )
        .routed_by_config(),
    );
    let initialization = INITIALIZATION.lock().await;
    let (entered, waiting) = std::sync::mpsc::channel();
    let (release, done) = std::sync::mpsc::channel();
    let held = store.clone();
    let holder = std::thread::spawn(move || held.held_fixture(entered, done));
    waiting.recv().unwrap();
    let before = tokio::time::Instant::now();
    let startup = startup(&shared, &board).await.unwrap();
    let child = launch(&shared, &board, &startup).await.unwrap();
    let (_stop, rx) = watch::channel(false);
    let result = NativeSupervision {
        shared: shared.clone(),
        backend: crate::hardware::fixture_backend(&shared),
        board: board.clone(),
        shutdown: rx,
        timing: Timing::default(),
    }
    .run(child, initialization, None, startup)
    .await;
    let elapsed = before.elapsed();
    let _ = release.send(());
    holder.join().unwrap();
    let quiesced = shared.runtime.lock().unwrap().boards[&board.id]
        .lifecycle
        .status()
        .quiesced;
    checkpoint_retired(&shared, &board, &store, "fixture").unwrap();
    let record = store.board("fixture").unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(result.is_err());
    assert!(
        elapsed < Duration::from_secs(1),
        "thermal stop waited {elapsed:?} for journal"
    );
    assert!(quiesced);
    assert_eq!(record.restriction, Some(crate::safety::Reason::Thermal));
    assert!(!record.owner_active);
}

#[tokio::test]
async fn thermal_trip_during_initialization_records_exactly_one_stop() {
    let dir = std::env::temp_dir().join(format!("tl-init-thermal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("client");
    crate::test_scripts::write_executable_script(
        &binary,
        r#"#!/bin/sh
while read command version id frequency voltage; do
 printf '%s\n' 'ASIC Temp = 60 C; Frequency = 1450 MHz' >&2
 sleep 0.2
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
done
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#,
    );
    let board: BoardConfig = toml::from_str("id='hot-init'\nname='Hot init'\nserial='fixture'\nclient_port=18110\ndevice_location=1\nprofile='manual'").unwrap();
    let mut config = Config::default();
    config.hardware.client_binary = Some(binary);
    config.hardware.boards = vec![board.clone()];
    let assignments = config.assignments().unwrap();
    let shared = Arc::new(
        crate::Shared::new(
            None,
            config,
            std::sync::Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState::new(
                false,
            ))),
            crate::operational::State::fixture(),
            crate::unix_now(),
            crate::unix_now(),
        )
        .routed(assignments),
    );
    let startup = startup(&shared, &board).await.unwrap();
    let child = launch(&shared, &board, &startup).await.unwrap();
    let (_stop, rx) = watch::channel(false);
    let initialization: &'static tokio::sync::Mutex<()> =
        Box::leak(Box::new(tokio::sync::Mutex::new(())));
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        NativeSupervision {
            shared: shared.clone(),
            backend: crate::hardware::fixture_backend(&shared),
            board: board.clone(),
            shutdown: rx,
            timing: Timing::default(),
        }
        .run(child, initialization.lock().await, None, startup),
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
    assert!(state.boards[&board.id].lifecycle.status().latched_fault);
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
    drop(state);
    std::fs::remove_dir_all(dir).unwrap();
}

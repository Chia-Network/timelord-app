#[test]
fn capability_accepts_the_complete_2200_mhz_grid_and_rejects_invalid_lists() {
    let full: Vec<f64> = (200..=2200).step_by(10).map(f64::from).collect();
    assert_eq!(
        super::decode_frequencies(&serde_json::to_vec(&full).unwrap()).unwrap(),
        full
    );
    for invalid in [
        vec![],
        vec![200., 2201.],
        vec![200., 200.],
        vec![2200., 200.],
        vec![200.; 202],
    ] {
        assert!(super::decode_frequencies(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }
}

#[test]
fn raw_speed_telemetry_is_bounded_and_engine_specific() {
    use super::RawState::*;
    assert_eq!(
        super::raw_speed("time TLRAW 2 2 1055000 running"),
        Some((2, 1055000., Running))
    );
    assert_eq!(
        super::raw_speed("TLRAW 2 0 -1 sampling"),
        Some((0, -1., Sampling))
    );
    assert_eq!(super::raw_speed("TLRAW 2 0 -1 idle"), Some((0, -1., Idle)));
    for line in [
        "TLRAW 2 3 10 running",
        "TLRAW 2 0 NaN running",
        "TLRAW 2 0 inf running",
        "TLRAW 2 0 -2 idle",
        "TLRAW 2 0 12 running extra",
        "TLRAW 1 0 10",
        "TLRAW 2 0 -1 running",
        "TLRAW 2 0 12 idle",
    ] {
        assert!(super::raw_speed(line).is_none(), "{line}");
    }
}
#[test]
fn handoff_preserves_last_raw_value_without_refreshing_its_age_or_calling_it_current() {
    use super::RawState::*;
    let mut h = super::HardwareState::default();
    h.raw_sample(0, 1055000., Running, 100);
    h.raw_sample(0, -1., Sampling, 101);
    assert_eq!(h.raw_speeds[0].ips, 1055000.);
    assert_eq!(h.raw_speeds[0].timestamp_unix, 100);
    assert_eq!(h.raw_speeds[0].state, Sampling);
    h.raw_sample(0, -1., Idle, 102);
    assert_eq!(h.raw_speeds[0].state, Idle);
    h.raw_sample(1, -1., Sampling, 103);
    assert_eq!(h.raw_speeds[1].ips, -1.);
    h.raw_sample(0, 1060000., Running, 104);
    assert_eq!(h.raw_speeds[0].timestamp_unix, 104);
    assert_eq!(h.raw_speeds[0].state, Running);
    assert_eq!(h.raw_speeds.len(), 2);
}

use super::*;
#[tokio::test]
async fn live_backoff_drains_a_full_stderr_pipe_before_acknowledgement() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tl-storm-pipe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("client");
    std::fs::write(
        &binary,
        r#"#!/bin/sh
if [ "$1" = --control-frequencies ]; then echo '[1430,1450,1460]'; exit 0; fi
while read command version id frequency voltage; do
 if [ "$id" = 2 ]; then
  n=0
  while [ "$n" -lt 5000 ]; do
   printf '%s\n' 'VDF 0: Restarting VDF at 123456789 iters' >&2
   n=$((n + 1))
  done
 fi
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config: Config = toml::from_str("[[hardware.boards]]\nid='storm'\nname='Storm'\nclient_port=18110\ndevice_location=1\nserial='fixture-only'\n").unwrap();
    config.hardware.client_binary = Some(binary.clone());
    let board = config.hardware.boards[0].clone();
    let shared = crate::Shared::new(
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
        std::sync::Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState::new(
            false,
        ))),
        crate::operational::State::fixture(),
        crate::unix_now(),
        crate::unix_now(),
    )
    .routed_by_config();
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut owner = NativeOwner::attach(&board, &mut child).unwrap();
    let startup = startup(&shared, &board).await.unwrap();
    let mut log = ClientLog::new(child.stderr.take().unwrap());
    owner
        .initialize(&shared, &board, &mut log, &startup)
        .await
        .unwrap();
    let result = owner
        .control
        .change_with_logs(
            Point {
                frequency_mhz: 1430.,
                voltage_v: 1.,
            },
            5,
            &mut log,
            &shared,
            &board,
        )
        .await;
    assert_eq!(result.unwrap().frequency_mhz, 1430.);
    while shared.runtime.lock().unwrap().boards["storm"].recoveries < 5000 {
        tokio::time::timeout(Duration::from_secs(1), log.read(&shared, &board))
            .await
            .unwrap()
            .unwrap();
    }
    terminate(&mut child).await;
    assert_eq!(
        shared.runtime.lock().unwrap().boards["storm"].recoveries,
        5000
    );
    assert_eq!(log.recent.len(), 8);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn initialization_drains_a_full_stderr_pipe_before_the_first_acknowledgement() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tl-init-storm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("client");
    std::fs::write(
        &binary,
        r#"#!/bin/sh
if [ "$1" = --control-frequencies ]; then echo '[1430,1450,1460]'; exit 0; fi
while read command version id frequency voltage; do
 n=0
 while [ "$n" -lt 5000 ]; do
  printf '%s\n' 'VDF 0: Restarting VDF at 123456789 iters' >&2
  n=$((n + 1))
 done
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config: Config = toml::from_str("[[hardware.boards]]\nid='storm-init'\nname='Storm init'\nclient_port=18110\ndevice_location=1\nprofile='manual'\n").unwrap();
    config.hardware.client_binary = Some(binary.clone());
    let board = config.hardware.boards[0].clone();
    let assignments = config.assignments().unwrap();
    let shared = crate::Shared::new(
        None,
        config,
        std::sync::Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState::new(
            false,
        ))),
        crate::operational::State::fixture(),
        crate::unix_now(),
        crate::unix_now(),
    )
    .routed(assignments);
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut owner = NativeOwner::attach(&board, &mut child).unwrap();
    let startup = startup(&shared, &board).await.unwrap();
    let mut log = ClientLog::new(child.stderr.take().unwrap());
    tokio::time::timeout(
        Duration::from_secs(3),
        owner.initialize(&shared, &board, &mut log, &startup),
    )
    .await
    .expect("initialization blocked on a full stderr pipe")
    .unwrap();
    // stdout acknowledgement and stderr diagnostics have independent pipes.
    // Initialization must drain enough to unblock the writer, but the final
    // buffered diagnostics may still arrive after its acknowledgement.
    tokio::time::timeout(Duration::from_secs(3), async {
        while shared.runtime.lock().unwrap().boards["storm-init"].recoveries < 5000 {
            log.read(&shared, &board).await.unwrap();
        }
    })
    .await
    .expect("initialization lost trailing stderr diagnostics");
    assert_eq!(
        shared.runtime.lock().unwrap().boards["storm-init"].recoveries,
        5000
    );
    terminate(&mut child).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn healthy_journal_failure_does_not_fail_profile_record_or_retire_owner() {
    let dir = std::env::temp_dir().join(format!("tl-healthy-journal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    let store = Arc::new(crate::safety::Store::open(&config_path).unwrap());
    let point = Point {
        frequency_mhz: 1450.,
        voltage_v: 1.,
    };
    store.begin_owner("fixture", point).unwrap();
    store
        .restrict("fixture", crate::safety::Reason::InvalidProof)
        .unwrap();
    let paths = timelord_supervisor::Paths::for_config(&config_path).unwrap();
    let journal = paths.directory.join("board-safety.json");
    std::fs::remove_file(&journal).unwrap();
    std::fs::create_dir(&journal).unwrap(); // Force the atomic rename to fail.
    let board: BoardConfig =
        toml::from_str("id='a'\nname='A'\nserial='fixture'\nclient_port=18110").unwrap();
    let mut config = Config::default();
    config.hardware.automatic_state = Some(dir.join("learned.json"));
    let runtime = {
        let mut state = crate::runtime::RuntimeState::default();
        state.safety = Some(store.clone());
        state
    };
    let shared = crate::Shared::new(
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
        0,
        0,
    );
    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut owner = Tuning {
        experiment: None,
        policy: Policy::new(
            board.automatic.clone(),
            point,
            crate::unix_now(),
            vec![1450.],
        ),
        serial: "fixture".into(),
        engines: 1,
        hash: "a".repeat(64),
        host: "fixture".into(),
        raw_window: Default::default(),
        last_record_unix: 0,
        journal: None,
        cautious: false,
        learned: None,
    };
    owner
        .checkpoint(&shared, &board, board.automatic.healthy_seconds)
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert!(store.board("fixture").unwrap().cautious);
    assert!(child.try_wait().unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(2), async {
        while !dir.join("learned.json").is_file() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    child.kill().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn eof_drains_large_native_output_and_reaps_a_confirmed_owner() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tl-eof-drain-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("client");
    std::fs::write(
        &binary,
        r#"#!/bin/sh
while read command version id frequency voltage; do
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
done
n=0
while [ "$n" -lt 4000 ]; do
 printf '%s\n' 'pending stdout pending stdout pending stdout pending stdout'
 printf '%s\n' 'pending stderr pending stderr pending stderr pending stderr' >&2
 n=$((n + 1))
done
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#,
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let board: BoardConfig = toml::from_str(
        "id='eof'\nname='EOF'\nclient_port=18110\ndevice_location=1\nprofile='manual'",
    )
    .unwrap();
    let mut config = Config::default();
    config.hardware.boards = vec![board.clone()];
    config.hardware.client_binary = Some(binary);
    let shared = crate::Shared::new(
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
        std::sync::Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState::new(
            false,
        ))),
        crate::operational::State::fixture(),
        crate::unix_now(),
        crate::unix_now(),
    )
    .routed_by_config();
    let startup = startup(&shared, &board).await.unwrap();
    let mut child = launch(&shared, &board, &startup).await.unwrap();
    let mut owner = NativeOwner::attach(&board, &mut child).unwrap();
    let mut log = ClientLog::new(child.stderr.take().unwrap());
    owner
        .initialize(&shared, &board, &mut log, &startup)
        .await
        .unwrap();
    assert!(owner.stop(&mut child, log).await);
    assert!(child.try_wait().unwrap().unwrap().success());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn live_thermal_rejection_survives_retirement_checkpoint_and_policy_restore() {
    let dir = std::env::temp_dir().join(format!("tl-control-retirement-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let board: BoardConfig =
        toml::from_str("id='hot'\nname='Hot'\nserial='fixture'\nclient_port=18110").unwrap();
    let store = crate::safety::Store::open(&dir.join("config.toml")).unwrap();
    store
        .begin_owner(
            "fixture",
            Point {
                frequency_mhz: 1450.,
                voltage_v: 1.,
            },
        )
        .unwrap();
    let mut config = Config::default();
    config.hardware.boards.push(board.clone());
    let shared = crate::Shared::new(
        None,
        config,
        std::sync::Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState::new(
            false,
        ))),
        crate::operational::State::fixture(),
        crate::unix_now(),
        crate::unix_now(),
    )
    .routed_by_config();

    let now = crate::unix_now();
    let error = anyhow::Error::new(NativeFailure::rejection(2, "temperature_guard"));
    native::record_retirement(&shared, &board, true, Some(&error));
    checkpoint_retired(&shared, &board, &store, "fixture").unwrap();
    let record = store.board("fixture").unwrap();
    assert_eq!(record.restriction, Some(crate::safety::Reason::Thermal));
    assert!(record.retry_after_unix >= now + 30);
    assert!(!record.owner_active);
    let mut restored = crate::board_lifecycle::BoardLifecycle::default();
    restored.restore_policy(store.policy(&board));
    assert_eq!(
        restored.status().phase,
        crate::board_lifecycle::Phase::ThermalStop
    );
    assert!(store.policy(&board).hold_reason(now + 5).is_some());
    assert!(store.policy(&board).hold_reason(now + 31).is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn thermal_shutdown_does_not_wait_for_the_safety_journal() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tl-thermal-journal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("client");
    std::fs::write(&binary,r#"#!/bin/sh
while read command version id frequency voltage; do
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
 printf '%s\n' 'Connected to timelord, waiting for challenge' 'ASIC Temp = 60 C; Frequency = 1450 MHz' >&2
done
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
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
    let result = supervise(
        shared.clone(),
        crate::hardware::fixture_backend(&shared),
        board.clone(),
        child,
        rx,
        initialization,
        None,
        startup,
    )
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
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tl-init-thermal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("client");
    std::fs::write(
        &binary,
        r#"#!/bin/sh
while read command version id frequency voltage; do
 printf '%s\n' 'ASIC Temp = 60 C; Frequency = 1450 MHz' >&2
 sleep 0.2
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
done
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#,
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
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
        supervise(
            shared.clone(),
            crate::hardware::fixture_backend(&shared),
            board.clone(),
            child,
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

#[test]
fn automatic_and_manual_arguments_share_the_optimized_proof_settings() {
    let mut board: BoardConfig =
        toml::from_str("id='a'\nname='A'\nclient_port=18110\ndevice_location=1").unwrap();
    let automatic = arguments(&board, 3).unwrap();
    assert!(automatic.contains(&"--live-control".into()));
    assert!(automatic.contains(&"--retain-asic-values".into()));
    assert!(automatic.windows(2).any(|w| w == ["--freq", "1450"]));
    board.profile = ProfileMode::Manual;
    board.frequency_mhz = 1540.;
    let manual = arguments(&board, 1).unwrap();
    assert!(manual.contains(&"--live-control".into()));
    assert!(manual.contains(&"--retain-asic-values".into()));
    assert!(manual.windows(2).any(|w| w == ["--freq", "1540"]));
    assert!(
        manual
            .windows(2)
            .any(|w| w == ["--checkpoint-threshold", "250000"])
    );
}

#[test]
fn native_mask_matches_physical_assignments_and_profile_mode() {
    let mut config = Config::default();
    for boards in 1..=3 {
        config.hardware.boards = (0..boards)
            .map(|n| {
                toml::from_str(&format!(
                    "id='b{n}'\nname='Board {n}'\nclient_port={}\ndevice_location={}\n",
                    18110 + n,
                    n + 1
                ))
                .unwrap()
            })
            .collect();
        let assignments = config.assignments().unwrap();
        for board in &config.hardware.boards {
            let assigned: Vec<_> = assignments
                .iter()
                .filter(|a| a.board_id == board.id)
                .collect();
            let expected = assigned.iter().fold(0u8, |mask, a| mask | (1 << a.engine));
            for mode in [ProfileMode::Automatic, ProfileMode::Manual] {
                let mut board = board.clone();
                board.profile = mode;
                let args = arguments(&board, assigned.len()).unwrap();
                assert!(
                    args.windows(2)
                        .any(|w| w == ["--vdfs-mask", &expected.to_string()])
                );
            }
        }
    }
    let board = &config.hardware.boards[0];
    assert!(arguments(board, 0).is_err());
    assert!(arguments(board, 4).is_err());
}

#[test]
fn temperature_parser_rejects_nonfinite_measurements() {
    assert_eq!(
        temperature("2026 ASIC Temp = 39.90 C; Frequency = 1540.0 MHz; freq_idx = 1"),
        Some((39.9, 1540.0))
    );
    assert_eq!(
        temperature("ASIC Temp = NaN C; Frequency = 1540.0 MHz"),
        None
    );
}

#[tokio::test]
async fn oversized_or_invalid_native_diagnostic_does_not_retire_the_owner_or_hide_following_telemetry()
 {
    let board: BoardConfig =
        toml::from_str("id='large-log'\nname='Large log'\nclient_port=18110\nprofile='manual'")
            .unwrap();
    let shared = crate::Shared::new(
        None,
        Config::default(),
        std::sync::Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState::new(
            false,
        ))),
        crate::operational::State::fixture(),
        0,
        0,
    );
    let mut child = Command::new("/bin/sh")
        .args(["-c", "printf '%4200s\\n' oversized >&2; printf 'bad \\377\\n' >&2; printf 'ASIC Temp = 40 C; Frequency = 1450 MHz\\n' >&2; exec /bin/sleep 60"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped())
        .kill_on_drop(true).spawn().unwrap();
    let mut log = ClientLog::new(child.stderr.take().unwrap());
    for _ in 0..3 {
        assert!(log.read(&shared, &board).await.unwrap().is_none());
    }
    assert!(child.try_wait().unwrap().is_none());
    {
        let state = shared.runtime.lock().unwrap();
        let hardware = &state.boards[&board.id];
        assert_eq!(hardware.temperature_c, Some(40.));
        assert_eq!(hardware.temperature_samples, 1);
        assert!(!hardware.lifecycle.status().latched_fault);
    }
    child.kill().await.unwrap();
}

mod chunked;
mod continuous;
mod faults;
#[cfg(test)]
mod retirement;
#[cfg(test)]
mod shutdown_drain;

mod startup;

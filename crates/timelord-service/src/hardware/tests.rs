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
async fn native_control_requires_matching_acknowledgements_and_keeps_hard_limits() {
    use std::os::unix::fs::PermissionsExt;
    let directory = std::env::temp_dir().join(format!("tl-ack-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let binary = directory.join("fake-client");
    std::fs::write(&binary,"#!/bin/sh\nif [ \"$1\" = --control-frequencies ]; then echo '[1450,1460,1470,1480,1490]'; exit 0; fi\nwhile read command version id frequency voltage; do\n  if [ \"$id\" = 4 ]; then printf '%0600d\\n' 0; continue; fi\n  if [ \"$id\" = 3 ]; then id=999; fi\n  if [ \"$id\" = 5 ]; then printf 'TLERR 1 5 readback frequency=0.000000\\n'; continue; fi\n  printf 'TLACK 1 %s %s %s\\n' \"$id\" \"$frequency\" \"$voltage\"\ndone\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config:Config=toml::from_str("[[hardware.boards]]\nid='a'\nname='A'\nclient_port=18110\ndevice_location=4113\nserial='a'\nprofile='automatic'").unwrap();
    config.hardware.client_binary = Some(binary.clone());
    config.hardware.automatic_state = Some(directory.join("cache.json"));
    let board = config.hardware.boards[0].clone();
    let args = arguments(&board, 3).unwrap();
    assert!(args.windows(2).any(|a| a == ["--freq", "1450"]));
    assert!(args.windows(2).any(|a| a == ["--voltage", "1"]));
    assert!(args.contains(&"--live-control".into()));
    let shared = Shared {
        assignments: config.assignments().unwrap(),
        config,
        clients: Default::default(),
        runtime: Default::default(),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    };
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut owner = NativeOwner::attach(&board, &mut child).unwrap();
    owner.initialize(&shared, &board).await.unwrap();
    assert_eq!(
        owner.tuning.as_ref().unwrap().policy.point.frequency_mhz,
        1450.
    );
    assert!(
        owner
            .control
            .change(
                Point {
                    frequency_mhz: 2201.,
                    voltage_v: 1.
                },
                1
            )
            .await
            .is_err()
    );
    assert_eq!(
        owner
            .control
            .change(
                Point {
                    frequency_mhz: 1460.,
                    voltage_v: 1.
                },
                1
            )
            .await
            .unwrap()
            .frequency_mhz,
        1460.
    );
    assert!(
        owner
            .control
            .change(
                Point {
                    frequency_mhz: 1470.,
                    voltage_v: 1.
                },
                1
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
    assert!(
        owner
            .control
            .change(
                Point {
                    frequency_mhz: 1480.,
                    voltage_v: 1.
                },
                1
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("too large")
    );
    assert!(
        owner
            .control
            .change(
                Point {
                    frequency_mhz: 1490.,
                    voltage_v: 1.
                },
                1
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("readback frequency=0.000000")
    );
    terminate(&mut child).await;
    std::fs::remove_dir_all(directory).unwrap();
}

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
   printf '%s\n' 'VDF 0: Restarting VDF at 123456789 iters (fixture recovery log flood)' >&2
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
    let shared = Shared {
        assignments: config.assignments().unwrap(),
        config,
        clients: Default::default(),
        runtime: Default::default(),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    };
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut owner = NativeOwner::attach(&board, &mut child).unwrap();
    owner.initialize(&shared, &board).await.unwrap();
    let (ready, _receiver) = oneshot::channel();
    let mut log = ClientLog::new(child.stderr.take().unwrap(), ready);
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
        .quarantine("fixture", crate::safety::Reason::UncleanOwner)
        .unwrap();
    let paths = timelord_supervisor::Paths::for_config(&config_path).unwrap();
    let journal = paths.directory.join("board-safety.json");
    std::fs::remove_file(&journal).unwrap();
    std::fs::create_dir(&journal).unwrap(); // Force the atomic rename to fail.
    let board: BoardConfig =
        toml::from_str("id='a'\nname='A'\nserial='fixture'\nclient_port=18110").unwrap();
    let mut config = Config::default();
    config.hardware.automatic_state = Some(dir.join("learned.json"));
    let runtime = crate::runtime::RuntimeState {
        safety: Some(store.clone()),
        ..Default::default()
    };
    let shared = Shared {
        config,
        assignments: vec![],
        clients: Default::default(),
        runtime: Arc::new(std::sync::Mutex::new(runtime)),
        started_unix: 0,
        service_started_unix: 0,
    };
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
    };
    owner
        .checkpoint(&shared, &board, board.automatic.healthy_seconds)
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert!(store.board("fixture").unwrap().force_safe);
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
    let shared = Shared {
        assignments: config.assignments().unwrap(),
        config,
        clients: Default::default(),
        runtime: Default::default(),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    };
    let mut child = launch(&shared, &board).await.unwrap();
    let mut owner = NativeOwner::attach(&board, &mut child).unwrap();
    owner.initialize(&shared, &board).await.unwrap();
    let (ready, _rx) = oneshot::channel();
    let log = ClientLog::new(child.stderr.take().unwrap(), ready);
    assert!(owner.stop(&mut child, log).await);
    assert!(child.try_wait().unwrap().unwrap().success());
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
    let runtime = crate::runtime::RuntimeState {
        safety: Some(store.clone()),
        ..Default::default()
    };
    let shared = Arc::new(Shared {
        assignments: config.assignments().unwrap(),
        config,
        clients: Default::default(),
        runtime: Arc::new(std::sync::Mutex::new(runtime)),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    });
    let initialization = INITIALIZATION.lock().await;
    let (entered, waiting) = std::sync::mpsc::channel();
    let (release, done) = std::sync::mpsc::channel();
    let held = store.clone();
    let holder = std::thread::spawn(move || held.held_fixture(entered, done));
    waiting.recv().unwrap();
    let before = tokio::time::Instant::now();
    let child = launch(&shared, &board).await.unwrap();
    let (ready, _ready) = oneshot::channel();
    let (_stop, rx) = watch::channel(false);
    let result = supervise(
        shared.clone(),
        board.clone(),
        child,
        ready,
        rx,
        initialization,
        None,
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
    assert_eq!(record.quarantine, Some(crate::safety::Reason::Thermal));
    assert!(!record.owner_active);
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

#[tokio::test]
async fn temperature_guard_stops_only_the_owned_fixture() {
    use std::os::unix::fs::PermissionsExt;
    let directory = std::env::temp_dir().join(format!(
        "timelord-client-fixture-{}-{}",
        std::process::id(),
        crate::unix_now()
    ));
    std::fs::create_dir(&directory).unwrap();
    let binary = directory.join("hw_vdf_client");
    std::fs::write(&binary, "#!/bin/sh\ntrap 'exit 0' TERM INT\nread command version id frequency voltage\nprintf 'TLACK 1 %s %s %s\\n' \"$id\" \"$frequency\" \"$voltage\"\nprintf '%s\\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' 'VDF 0: Connected to timelord, waiting for challenge' 'ASIC Temp = 55.0 C; Frequency = 1500.0 MHz; freq_idx = 1' >&2\ncat >/dev/null\nprintf '%s\\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config: Config = toml::from_str(
            "[[hardware.boards]]\nid='a'\nname='A'\nclient_port=18110\ndevice_location=12305\nprofile='manual'\n",
        )
        .unwrap();
    config.hardware.client_binary = Some(binary);
    let shared = Arc::new(Shared {
        assignments: config.assignments().unwrap(),
        config,
        clients: Arc::new(std::sync::Mutex::new(Default::default())),
        runtime: Default::default(),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    });
    let (stop, receiver) = watch::channel(false);
    let (backend, _offers) =
        crate::asic::AsicBackend::new(&shared.config, &shared.assignments, shared.runtime.clone())
            .unwrap();
    let task = tokio::spawn(run(shared.clone(), Arc::new(backend), receiver));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let stopped = {
                let state = shared.runtime.lock().unwrap();
                state.boards.get("a").is_some_and(|b| {
                    b.lifecycle.status().phase == crate::board_lifecycle::Phase::ThermalStop
                        && b.lifecycle.status().latched_fault
                }) && state.message.contains("needs attention")
            };
            if stopped {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !task.is_finished(),
        "A thermal stop must not end other board supervision"
    );
    assert!(
        shared
            .runtime
            .lock()
            .unwrap()
            .message
            .contains("needs attention")
    );
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn restart_delay_is_capped_and_never_exhausts_for_communication_faults() {
    let mut budget = RestartBackoff::default();
    assert_eq!(budget.next(false), Duration::from_secs(1));
    assert_eq!(budget.next(false), Duration::from_secs(5));
    assert_eq!(budget.next(false), Duration::from_secs(30));
    for _ in 0..100 {
        assert_eq!(budget.next(false), Duration::from_secs(30));
    }
    assert_eq!(budget.next(true), Duration::from_secs(1));
}
#[tokio::test]
async fn hotplug_recovers_after_repeated_failures_without_restarting_healthy_board() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tl-board-isolation-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("client");
    std::fs::write(&binary,r#"#!/bin/sh
root=$(dirname "$0")
while [ "$#" -gt 0 ]; do
 if [ "$1" = --device-loc ]; then loc=$2; break; fi
 shift
done
echo $$ >> "$root/launches-$loc"
trap 'exit 0' TERM INT
if [ "$loc" = 2 ] && [ ! -f "$root/reconnected" ]; then echo "ASIC Temp = 0.0 C; Frequency = 1500.0 MHz" >&2; exit 1; fi
read command version id frequency voltage
printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
printf '%s\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' 'VDF 0: Connected to timelord, waiting for challenge' >&2
while :; do printf '%s\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' >&2; sleep .05; done &
logger=$!
cat >/dev/null
kill "$logger"
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config:Config=toml::from_str("[[hardware.boards]]\nid='healthy'\nname='Healthy'\nclient_port=18110\ndevice_location=1\nprofile='manual'\n[[hardware.boards]]\nid='flaky'\nname='Flaky'\nclient_port=18111\ndevice_location=2\nprofile='manual'").unwrap();
    config.hardware.client_binary = Some(binary);
    let shared = Arc::new(Shared {
        assignments: config.assignments().unwrap(),
        config,
        clients: Default::default(),
        runtime: Default::default(),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    });
    let (backend, _offers) =
        crate::asic::AsicBackend::new(&shared.config, &shared.assignments, shared.runtime.clone())
            .unwrap();
    let backend = Arc::new(backend);
    let (stop, rx) = watch::channel(false);
    let (tx, _) = oneshot::channel();
    let healthy = tokio::spawn(supervise_board(
        shared.clone(),
        backend.clone(),
        shared.config.hardware.boards[0].clone(),
        tx,
        rx.clone(),
        RestartBackoff::default(),
    ));
    let (tx, _) = oneshot::channel();
    let flaky = tokio::spawn(supervise_board(
        shared.clone(),
        backend.clone(),
        shared.config.hardware.boards[1].clone(),
        tx,
        rx.clone(),
        RestartBackoff {
            used: 0,
            delays: [Duration::from_millis(10); 3],
        },
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if shared
                .runtime
                .lock()
                .unwrap()
                .boards
                .get("flaky")
                .is_some_and(|b| b.lifecycle.status().generation >= 6)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!flaky.is_finished());
    assert!(!healthy.is_finished());
    std::fs::write(dir.join("reconnected"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if shared
                .runtime
                .lock()
                .unwrap()
                .boards
                .get("flaky")
                .is_some_and(|b| {
                    b.lifecycle.status().phase == crate::board_lifecycle::Phase::Connected
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    {
        let state = shared.runtime.lock().unwrap();
        assert!(!state.boards["flaky"].lifecycle.status().latched_fault);
        assert!(state.boards["flaky"].lifecycle.status().generation >= 6);
        assert_eq!(state.boards["healthy"].lifecycle.status().generation, 1);
        assert_eq!(
            state.boards["healthy"].lifecycle.status().phase.as_str(),
            "connected"
        );
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("launches-1"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    stop.send(true).unwrap();
    for task in [flaky, healthy] {
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
    }
    std::fs::remove_file(dir.join("reconnected")).unwrap();
    let (stop, rx) = watch::channel(false);
    let (tx, _) = oneshot::channel();
    let retrying = tokio::spawn(supervise_board(
        shared.clone(),
        backend,
        shared.config.hardware.boards[1].clone(),
        tx,
        rx,
        RestartBackoff {
            used: 0,
            delays: [Duration::from_secs(30); 3],
        },
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if shared
                .runtime
                .lock()
                .unwrap()
                .boards
                .get("flaky")
                .is_some_and(|b| {
                    b.lifecycle.status().phase == crate::board_lifecycle::Phase::Reconnecting
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), retrying)
        .await
        .unwrap()
        .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn missing_sensor_data_is_not_a_thermal_rejection() {
    for reason in [
        "temperature_unavailable",
        "initial_readback attempts=3",
        "ASIC temperature telemetry stopped",
        "USB open failed",
    ] {
        assert!(!thermal_rejection(&anyhow::anyhow!(
            "Native control rejected request 2: {reason}"
        )));
    }
    for reason in ["temperature_limit", "temperature_guard"] {
        assert!(thermal_rejection(&anyhow::anyhow!(
            "Native control rejected request 2: {reason}"
        )));
    }
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
async fn oversized_native_diagnostic_does_not_retire_the_owner_or_hide_following_telemetry() {
    let board: BoardConfig =
        toml::from_str("id='large-log'\nname='Large log'\nclient_port=18110\nprofile='manual'")
            .unwrap();
    let shared = Shared {
        config: Config::default(),
        assignments: vec![],
        clients: Default::default(),
        runtime: Default::default(),
        started_unix: 0,
        service_started_unix: 0,
    };
    let mut child = Command::new("/bin/sh")
        .args(["-c", "printf '%4200s\\n' oversized >&2; printf 'ASIC Temp = 40 C; Frequency = 1450 MHz\\n' >&2; exec /bin/sleep 60"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped())
        .kill_on_drop(true).spawn().unwrap();
    let (ready, _rx) = oneshot::channel();
    let mut log = ClientLog::new(child.stderr.take().unwrap(), ready);
    assert!(!log.read(&shared, &board).await.unwrap());
    assert!(!log.read(&shared, &board).await.unwrap());
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

mod retirement;

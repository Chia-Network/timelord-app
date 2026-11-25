use super::*;

async fn native_fixture(tag: &str) -> (std::path::PathBuf, crate::Shared, BoardConfig) {
    let dir = std::env::temp_dir().join(format!(
        "tl-native-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("client");
    crate::test_scripts::write_executable_script(
        &binary,
        r#"#!/bin/sh
if [ "$1" = --control-version ]; then echo timelord-live-control-v1; exit 0; fi
if [ "$1" = --control-frequencies ]; then echo '[1400,1410,1420,1430,1440,1450,1460,1470,1480,1490,1500]'; exit 0; fi
while read command version id frequency voltage; do
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
done
"#,
    );
    let board: BoardConfig = toml::from_str(&format!(
        "id='{tag}'\nname='{tag}'\nserial='fixture'\nclient_port=18110\ndevice_location=1\nprofile='automatic'\n[automatic]\nstart_frequency_mhz=1400.0"
    ))
    .unwrap();
    let mut config = Config::default();
    config.hardware.boards = vec![board.clone()];
    config.hardware.client_binary = Some(binary);
    let profiles = dir.join("profiles.json");
    config.hardware.automatic_state = Some(profiles.clone());
    automatic::save(
        &profiles,
        automatic::Learned {
            serial: board.serial.clone().unwrap(),
            engines: 1,
            engine_mask: timelord_core::engine_mask(1),
            host: automatic::host(),
            client_sha256: "previous-native-build".into(),
            point: Point {
                frequency_mhz: 1500.,
                voltage_v: 1.,
            },
            active_seconds: 0,
            updated_unix: crate::unix_now(),
            ceiling_mhz: 2200.,
            cooldown_seconds: 0,
            performance: None,
            retest: None,
            stable_point: Some(Point {
                frequency_mhz: 1500.,
                voltage_v: 1.,
            }),
        },
    )
    .unwrap();
    let shared = crate::Shared::new(
        Some(Facts::load(&config).await.unwrap()),
        config,
        Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState::new(
            false,
        ))),
        crate::operational::State::fixture(),
        crate::unix_now(),
        crate::unix_now(),
    );
    (dir, shared, board)
}

#[tokio::test]
async fn thermal_probe_is_allowed_below_optimization_minima() {
    let board: BoardConfig = toml::from_str("id='probe'\nname='Probe'\nprofile='automatic'\nclient_port=18110\n[automatic]\nmin_frequency_mhz=1600.0\nmin_voltage_v=0.9").unwrap();
    let mut child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let owner = NativeOwner::attach(&board, &mut child).unwrap();
    assert!(owner.control.accepts(Point {
        frequency_mhz: 200.,
        voltage_v: 0.7
    }));
    child.kill().await.unwrap();
}
#[test]
fn recovery_records_are_parsed_once_at_the_native_boundary() {
    assert_eq!(
        Recovery::parse("2026-11-04 VDF 2: Restarting VDF at 123 iters"),
        Some(Recovery { engine: 2 })
    );
    for line in [
        "Restarting VDF at 123 iters",
        "VDF 3: Restarting VDF at 123 iters",
        "VDF 0: Restarting VDF at nope iters",
        "VDF 0: Restarting VDF at 123 iters unrelated text",
    ] {
        assert_eq!(Recovery::parse(line), None);
    }
}

#[test]
fn recovery_records_match_a_captured_hardware_client_log() {
    let log = include_str!("../../../tests/fixtures/native-recovery-stderr.txt");
    let engines: Vec<u8> = log
        .lines()
        .filter_map(Recovery::parse)
        .map(|recovery| recovery.engine)
        .collect();
    assert_eq!(engines, [0, 0, 0, 2, 0, 0, 1, 0, 0]);
}

#[tokio::test]
async fn drain_keeps_clean_shutdown_evidence_it_consumes() {
    let board: BoardConfig =
        toml::from_str("id='probe'\nname='Probe'\nprofile='manual'\nclient_port=18110").unwrap();
    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            "printf 'Failed to open device\\nHardware shutdown: engines_paused=1 safe_point_restored=1\\n' >&2; exit 1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    // Wait until the fixture has written its shutdown evidence and exited, so
    // the drain below reads a completed pipe regardless of scheduler pressure.
    child.wait().await.unwrap();
    let mut log = ClientLog::new(stderr);
    log.drain(&board, Duration::from_millis(500)).await;
    assert!(log.open_failed, "fixture must print an open failure");
    assert!(
        log.shutdown_confirmed,
        "a clean shutdown consumed by the drain must still count as confirmation"
    );
}

#[tokio::test]
async fn inherited_start_is_kept_when_the_first_route_appears() {
    let (dir, shared, board) = native_fixture("inherit").await;
    let startup = startup(&shared, &board).await.unwrap();
    assert_eq!(startup.point.frequency_mhz, 1450.);
    let mut child = launch(&shared, &board, &startup).await.unwrap();
    let mut owner = NativeOwner::attach(&board, &mut child).unwrap();
    let mut log = ClientLog::new(child.stderr.take().unwrap());
    owner
        .initialize(&shared, &board, &mut log, &startup)
        .await
        .unwrap();
    assert_eq!(
        owner.tuning.as_ref().unwrap().engines,
        1,
        "an idle board still has one prospective engine"
    );
    assert_eq!(
        owner.tuning.as_ref().unwrap().policy.point.frequency_mhz,
        1450.
    );
    shared.runtime.lock().unwrap().routing = vec![timelord_core::Assignment {
        board_id: board.id.clone(),
        engine: 0,
        chain: timelord_core::Chain::Challenge,
    }];
    {
        use crate::board_lifecycle::{BoardLifecycle, Phase};
        shared.runtime.lock().unwrap().boards.insert(
            board.id.clone(),
            crate::runtime::HardwareState {
                lifecycle: BoardLifecycle::fixture(Phase::Connected, true, None, 1),
                ..Default::default()
            },
        );
    }
    let (_stop, mut shutdown) = watch::channel(false);
    let tuning = owner.tuning.as_mut().unwrap();
    tuning
        .tick(&mut owner.control, &shared, &board, &mut log, &mut shutdown)
        .await
        .unwrap();
    assert_eq!(
        tuning.policy.point.frequency_mhz, 1450.,
        "the first route must not clamp the inherited point to the configured start"
    );
    child.kill().await.unwrap();
    let _ = child.wait().await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn engine_count_snapshot_is_taken_at_startup_not_initialize() {
    let (dir, shared, board) = native_fixture("engines").await;
    // No routes yet: the startup snapshot has the idle board's single engine.
    let startup = startup(&shared, &board).await.unwrap();
    shared.runtime.lock().unwrap().routing = vec![
        timelord_core::Assignment {
            board_id: board.id.clone(),
            engine: 0,
            chain: timelord_core::Chain::Challenge,
        },
        timelord_core::Assignment {
            board_id: board.id.clone(),
            engine: 1,
            chain: timelord_core::Chain::Reward,
        },
    ];
    let mut child = launch(&shared, &board, &startup).await.unwrap();
    let mut owner = NativeOwner::attach(&board, &mut child).unwrap();
    let mut log = ClientLog::new(child.stderr.take().unwrap());
    owner
        .initialize(&shared, &board, &mut log, &startup)
        .await
        .unwrap();
    assert_eq!(
        owner.tuning.as_ref().unwrap().engines,
        1,
        "initialize must use the startup snapshot, not the live route count"
    );
    {
        use crate::board_lifecycle::{BoardLifecycle, Phase};
        shared.runtime.lock().unwrap().boards.insert(
            board.id.clone(),
            crate::runtime::HardwareState {
                lifecycle: BoardLifecycle::fixture(Phase::Connected, true, None, 1),
                ..Default::default()
            },
        );
    }
    let (_stop, mut shutdown) = watch::channel(false);
    let tuning = owner.tuning.as_mut().unwrap();
    tuning
        .tick(&mut owner.control, &shared, &board, &mut log, &mut shutdown)
        .await
        .unwrap();
    assert_eq!(
        tuning.engines, 2,
        "the tuning loop must apply the engine-count change"
    );
    child.kill().await.unwrap();
    let _ = child.wait().await;
    std::fs::remove_dir_all(dir).unwrap();
}

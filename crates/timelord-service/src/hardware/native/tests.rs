use super::*;
use crate::{automatic::Point, hardware::run, runtime::HardwareState};
use timelord_core::Config;

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
    let mut h = HardwareState::default();
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
    let native = Some(crate::hardware::Facts::load(&config).await.unwrap());
    let shared = Shared {
        native,
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
    let mut owner = AutomaticOwner::new(&shared, &board, &mut child)
        .await
        .unwrap();
    let (ready, _receiver) = oneshot::channel();
    let mut log = ClientLog::new(child.stderr.take().unwrap(), ready);
    let result = owner
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
async fn temperature_guard_stops_only_the_owned_fixture() {
    use std::os::unix::fs::PermissionsExt;
    let directory = std::env::temp_dir().join(format!(
        "timelord-client-fixture-{}-{}",
        std::process::id(),
        crate::unix_now()
    ));
    std::fs::create_dir(&directory).unwrap();
    let binary = directory.join("hw_vdf_client");
    std::fs::write(&binary, "#!/bin/sh\ntrap 'exit 0' TERM INT\nprintf '%s\\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' 'VDF 0: Connected to timelord, waiting for challenge' 'ASIC Temp = 55.0 C; Frequency = 1500.0 MHz; freq_idx = 1' >&2\nwhile :; do sleep 1; done\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config: Config = toml::from_str(
        "[[hardware.boards]]\nid='a'\nname='A'\nclient_port=18110\ndevice_location=12305\nprofile='manual'\n",
    )
    .unwrap();
    config.hardware.client_binary = Some(binary);
    let shared = Arc::new(Shared {
        native: None,
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
                state
                    .boards
                    .get("a")
                    .is_some_and(|b| b.state == "thermal_stop" && b.latched_fault)
                    && state.message.contains("needs attention")
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
async fn temperature_releases_usb_initialization_without_claiming_network_readiness() {
    let board: BoardConfig = toml::from_str(
        "id='temperature-only'\nname='Temperature only'\nclient_port=18110\nprofile='manual'",
    )
    .unwrap();
    let shared = Arc::new(Shared {
        native: None,
        config: Default::default(),
        assignments: vec![],
        clients: Default::default(),
        runtime: Default::default(),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    });
    let child = Command::new("/bin/sh")
        .args(["-c", "while :; do printf '%s\\n' 'ASIC Temp = 40 C; Frequency = 1450 MHz' >&2; sleep .01; done"])
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn().unwrap();
    let pid = child.id().unwrap();
    // An independent lock makes this fixture deterministic alongside other
    // native-client tests, without changing the production lock contract.
    let initialization: &'static tokio::sync::Mutex<()> =
        Box::leak(Box::new(tokio::sync::Mutex::new(())));
    let (ready, initialized) = oneshot::channel();
    let (_stop, rx) = watch::channel(false);
    let owner = tokio::spawn(supervise(
        shared.clone(),
        board.clone(),
        child,
        ready,
        rx,
        initialization.lock().await,
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
        "USB initialization must not wait for a network connection"
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
    assert_eq!(
        unsafe { libc::kill(pid as i32, 0) },
        -1,
        "owner was not reaped"
    );
}

#[tokio::test]
async fn slow_successful_automatic_ack_gets_a_fresh_ready_budget() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tl-slow-ack-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("client");
    std::fs::write(
        &binary,
        r#"#!/bin/sh
root=$(dirname "$0")
if [ "$1" = --control-frequencies ]; then echo '[1450,1460]'; exit 0; fi
read command version id frequency voltage
echo requested > "$root/requested"
while [ ! -f "$root/ack" ]; do sleep .01; done
printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
printf '%s\n' 'ASIC Temp = 40 C; Frequency = 1450 MHz' >&2
while [ ! -f "$root/ready" ]; do sleep .01; done
printf '%s\n' 'Connected to timelord, waiting for challenge' >&2
cat >/dev/null
"#,
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let board: BoardConfig = toml::from_str(
        "id='slow'\nname='Slow'\nserial='slow'\nclient_port=18110\ndevice_location=1\nprofile='automatic'",
    )
    .unwrap();
    let mut config = Config::default();
    config.hardware.client_binary = Some(binary.clone());
    config.hardware.boards = vec![board.clone()];
    let native = Some(crate::hardware::Facts::load(&config).await.unwrap());
    let shared = Arc::new(Shared {
        native,
        assignments: config.assignments().unwrap(),
        config,
        clients: Default::default(),
        runtime: Default::default(),
        started_unix: crate::unix_now(),
        service_started_unix: crate::unix_now(),
    });
    let child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let initialization: &'static tokio::sync::Mutex<()> =
        Box::leak(Box::new(tokio::sync::Mutex::new(())));
    let (ready, initialized) = oneshot::channel();
    let (stop, rx) = watch::channel(false);
    let owner = tokio::spawn(supervise(
        shared.clone(),
        board.clone(),
        child,
        ready,
        rx,
        initialization.lock().await,
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dir.join("requested").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(29)).await;
    tokio::time::resume();
    std::fs::write(dir.join("ack"), "release ACK").unwrap();
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
        "acknowledged USB setup must release initialization independently of readiness"
    );
    // Total elapsed time now exceeds both setup limits, but the fresh ready
    // wait is still within its own thirty-second allowance.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(20)).await;
    tokio::time::resume();
    std::fs::write(dir.join("ready"), "release readiness").unwrap();
    tokio::time::timeout(Duration::from_secs(3), initialized)
        .await
        .unwrap()
        .unwrap();
    assert!(initialization.try_lock().is_ok());
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), owner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
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

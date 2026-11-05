use super::*;
use crate::hardware::{arguments, native::terminate};
use std::process::Stdio;
use timelord_core::Config;
use tokio::process::Command;

#[tokio::test]
async fn native_control_requires_matching_acknowledgements_and_keeps_hard_limits() {
    use std::os::unix::fs::PermissionsExt;
    let directory = std::env::temp_dir().join(format!("tl-ack-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let binary = directory.join("fake-client");
    std::fs::write(&binary, r#"#!/bin/sh
if [ "$1" = --control-frequencies ]; then echo '[1450,1460,1470,1480,1490]'; exit 0; fi
while read command version id frequency voltage; do
  if [ "$id" = 4 ]; then printf '%0600d\n' 0; continue; fi
  if [ "$id" = 3 ]; then id=999; fi
  if [ "$id" = 5 ] || [ "$id" = 9 ]; then printf 'TLERR 1 %s readback frequency=0.000000\n' "$id"; continue; fi
  if [ "$id" = 8 ]; then printf 'TLERR 1 8 temperature_guard temperature=58\n'; continue; fi
  printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
done
"#).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config:Config=toml::from_str("[[hardware.boards]]\nid='a'\nname='A'\nclient_port=18110\ndevice_location=4113\nserial='a'\nprofile='automatic'").unwrap();
    config.hardware.client_binary = Some(binary.clone());
    config.hardware.automatic_state = Some(directory.join("cache.json"));
    let board = config.hardware.boards[0].clone();
    let args = arguments(&board, 3).unwrap();
    assert!(args.windows(2).any(|a| a == ["--freq", "1450"]));
    assert!(args.windows(2).any(|a| a == ["--voltage", "1"]));
    assert!(args.contains(&"--live-control".into()));
    let native = Some(crate::hardware::Facts::load(&config).await.unwrap());
    let shared = Shared {
        native,
        assignments: config.assignments().unwrap(),
        config,
        clients: Default::default(),
        process: crate::operational::State::fixture(),
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
    shared
        .runtime
        .lock()
        .unwrap()
        .boards
        .entry(board.id.clone())
        .or_default()
        .recoveries = 20;
    let mut owner = AutomaticOwner::new(&shared, &board, &mut child)
        .await
        .unwrap();
    assert_eq!(owner.policy.point.frequency_mhz, 1450.);
    // Exercise the lifetime baseline after settling and above the lowest point,
    // where an unsynchronized counter really would cause a protective write.
    owner.policy.applied(
        Point {
            frequency_mhz: 1470.,
            voltage_v: 1.,
        },
        crate::unix_now().saturating_sub(10),
        "settled fixture point".into(),
    );
    assert!(
        owner
            .policy
            .observe(
                crate::unix_now(),
                automatic::Input {
                    healthy: false,
                    recoveries: 20,
                    score: None,
                    temperature: Some(40.),
                    temperature_limit: board.temperature_limit_c,
                    paused: false,
                }
            )
            .is_none(),
        "recreated owner interpreted lifetime recoveries as a new storm"
    );
    assert!(
        owner
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
    let (ready, _ready) = tokio::sync::oneshot::channel();
    let mut log = ClientLog::new(child.stderr.take().unwrap(), ready);
    for (index, bytes) in ["not valid JSON", r#"{"schema_version":99,"entries":[]}"#]
        .into_iter()
        .enumerate()
    {
        let path = shared.config.hardware.automatic_state.as_ref().unwrap();
        std::fs::write(path, bytes).unwrap();
        owner.last_record_unix = 7;
        let last_success = owner.last_record_unix;
        assert!(!owner.record(&shared, &board, 0, &mut log).await.unwrap());
        assert_eq!(owner.last_record_unix, last_success);
        // A failed checkpoint backs off instead of retrying every watchdog tick.
        let attempt = owner.last_record_attempt_unix;
        assert!(attempt > last_success);
        assert!(!owner.record_due(attempt + RECORD_RETRY_SECONDS - 1));
        assert!(owner.record_due(attempt + RECORD_RETRY_SECONDS));
        owner.policy.applied(
            Point {
                frequency_mhz: 1470.,
                voltage_v: 1.,
            },
            crate::unix_now().saturating_sub(10),
            "fixture point".into(),
        );
        shared
            .runtime
            .lock()
            .unwrap()
            .boards
            .get_mut(&board.id)
            .unwrap()
            .recoveries = 40 + 20 * index as u64;
        // A genuine new storm must still lower the native setting even though
        // saving the resulting learned limit fails. The owner keeps running.
        owner.tick(&shared, &board, &mut log).await.unwrap();
        assert!(owner.policy.point.frequency_mhz < 1470.);
        assert_eq!(std::fs::read_to_string(path).unwrap(), bytes);
        assert!(child.try_wait().unwrap().is_none());
    }
    let path = shared.config.hardware.automatic_state.as_ref().unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(owner.record(&shared, &board, 0, &mut log).await.unwrap());
    assert!(owner.last_record_unix > 7);
    for (recoveries, kind) in [
        (80, crate::hardware::NativeFailureKind::Thermal),
        (100, crate::hardware::NativeFailureKind::Readback),
    ] {
        owner.policy.applied(
            Point {
                frequency_mhz: 1470.,
                voltage_v: 1.,
            },
            crate::unix_now().saturating_sub(10),
            "settled fixture".into(),
        );
        shared
            .runtime
            .lock()
            .unwrap()
            .boards
            .get_mut(&board.id)
            .unwrap()
            .recoveries = recoveries;
        let error = owner.tick(&shared, &board, &mut log).await.unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<crate::hardware::NativeFailure>()
                .unwrap()
                .kind,
            kind
        );
        assert!(
            error.to_string().contains(&board.name),
            "board display context was lost"
        );
    }
    terminate(&mut child).await;
    std::fs::remove_dir_all(directory).unwrap();
}

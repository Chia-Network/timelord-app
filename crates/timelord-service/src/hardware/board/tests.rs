use super::*;
use timelord_core::Config;

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
printf '%s\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' 'VDF 0: Connected to timelord, waiting for challenge' >&2
while :; do printf '%s\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' >&2; sleep .05; done
"#).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config:Config=toml::from_str("[[hardware.boards]]\nid='healthy'\nname='Healthy'\nclient_port=18110\ndevice_location=1\nprofile='manual'\n[[hardware.boards]]\nid='flaky'\nname='Flaky'\nclient_port=18111\ndevice_location=2\nprofile='manual'").unwrap();
    config.hardware.client_binary = Some(binary);
    let shared = Arc::new(Shared {
        native: None,
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
                .is_some_and(|b| b.generation >= 6)
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
                .is_some_and(|b| b.state == "connected")
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
        assert!(!state.boards["flaky"].latched_fault);
        assert!(state.boards["flaky"].generation >= 6);
        assert_eq!(state.boards["healthy"].generation, 1);
        assert_eq!(state.boards["healthy"].state, "connected");
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
                .is_some_and(|b| b.state == "reconnecting")
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
        let error = anyhow::Error::new(crate::hardware::NativeFailure::rejection(2, reason))
            .context("Board A: failed");
        assert!(thermal_rejection(&error));
        assert!(!thermal_rejection(&anyhow::anyhow!(
            "unrelated text mentioning {reason}"
        )));
    }
}

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
    let mut owner = AutomaticOwner::new(&shared, &board, &mut child)
        .await
        .unwrap();
    assert_eq!(owner.policy.point.frequency_mhz, 1450.);
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
    terminate(&mut child).await;
    std::fs::remove_dir_all(directory).unwrap();
}

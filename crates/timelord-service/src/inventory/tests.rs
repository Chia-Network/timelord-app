use super::*;
#[tokio::test]
async fn initialization_wait_does_not_arm_discovery_watchdog() {
    let process = crate::operational::State::fixture();
    let mut config = Config::default();
    config.hardware.client_binary = Some("/not-present/client".into());
    config.hardware.driver_library = Some("/not-present/driver".into());
    let state = crate::runtime::RuntimeState::new(false);
    let shared = crate::Shared::new(
        None,
        config,
        std::sync::Arc::new(std::sync::Mutex::new(state)),
        process.clone(),
        0,
        0,
    );
    let topology = |sequence| {
        process
            .progress
            .snapshot(0, sequence)
            .unwrap()
            .slot(timelord_supervisor::progress::Resource::Topology)
    };
    let initialization = crate::hardware::INITIALIZATION.lock().await;
    let pending = live_configuration(&shared);
    tokio::pin!(pending);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut pending)
            .await
            .is_err()
    );
    assert!(topology(1).is_none());
    drop(initialization);
    assert!(pending.await.is_err());
    assert!(topology(2).is_none());
}

fn configured() -> Config {
    let mut config = Config::default();
    config.hardware.boards = (0..3).map(|i| toml::from_str(&format!(
        "id='b{i}'\nname='B{i}'\nserial='serial-{i}'\ndevice_location={}\nclient_port={}\nprofile='automatic'", i+1, i+18110
    )).unwrap()).collect();
    config.hardware.boards[0].automatic.start_frequency_mhz = 1460.;
    config
}
fn devices(indices: &[u32]) -> Vec<Device> {
    indices
        .iter()
        .map(|i| Device {
            name: "Chia VDF A".into(),
            location: i + 1,
            serial: Some(format!("serial-{i}")),
        })
        .collect()
}
#[test]
fn automatic_ports_skip_http_listeners_across_hotplug() {
    let mut config = Config::default();
    config.service.status_listen = "127.0.0.1:18111".parse().unwrap();
    config.service.observability_listen = Some("[::]:18110".parse().unwrap());
    config.service.allow_remote_observability = true;
    config.service.supervisor_observability_listen = "127.0.0.1:18112".parse().unwrap();
    config.validate().unwrap();
    for present in [&[0, 1, 2][..], &[], &[0], &[0, 1, 2]] {
        config = connected_plan(config, devices(present)).unwrap();
        config.validate().unwrap();
        assert_eq!(config.hardware.boards.len(), present.len());
        let ports: std::collections::BTreeSet<_> = config
            .hardware
            .boards
            .iter()
            .map(|b| b.client_port)
            .collect();
        assert_eq!(ports.len(), present.len());
        assert!(!ports.contains(&18110));
        assert!(!ports.contains(&18111));
        assert!(!ports.contains(&18112));
        assert_eq!(
            config.assignments().unwrap().len(),
            if present.is_empty() { 0 } else { 3 }
        );
    }
}
#[test]
fn automatic_topology_survives_three_one_zero_two_three_and_remembers_settings() {
    let original = configured();
    let mut config = original.clone();
    for (present, counts) in [
        (vec![0], vec![3]),
        (vec![], vec![]),
        (vec![0, 1], vec![2, 1]),
        (vec![0, 1, 2], vec![1, 1, 1]),
    ] {
        config = connected_plan(config, devices(&present)).unwrap();
        let plan = config.assignments().unwrap();
        assert_eq!(
            config
                .hardware
                .boards
                .iter()
                .map(|b| plan.iter().filter(|a| a.board_id == b.id).count())
                .collect::<Vec<_>>(),
            counts
        );
        assert_eq!(plan.len(), if present.is_empty() { 0 } else { 3 });
        assert_eq!(config.hardware.remembered_boards.len(), 3);
    }
    assert_eq!(
        config.hardware.boards[0].automatic.start_frequency_mhz,
        1460.
    );
    for board in &config.hardware.boards {
        let saved = original
            .hardware
            .boards
            .iter()
            .find(|b| b.id == board.id)
            .unwrap();
        assert_eq!(saved.serial, board.serial);
        assert_eq!(
            serde_json::to_value(&saved.automatic).unwrap(),
            serde_json::to_value(&board.automatic).unwrap()
        );
    }
}
#[test]
fn visible_managed_board_is_not_synthesized_twice() {
    let config = configured();
    let mut runtime = crate::runtime::RuntimeState::default();
    let mut board = crate::runtime::HardwareState::default();
    board.lifecycle.testing().inventory(true);
    runtime.boards.insert("b0".into(), board);
    let mut detected = devices(&[0, 1, 2]);
    preserve_managed_boards(&config, &mut detected, &runtime, 1000);
    assert_eq!(detected.len(), 3);
    assert_eq!(
        connected_plan(config, detected)
            .unwrap()
            .hardware
            .boards
            .len(),
        3
    );
}

#[test]
fn idle_owner_without_socket_keeps_three_board_layout_through_metadata_gaps() {
    let config = configured();
    let mut runtime = crate::runtime::RuntimeState::default();
    runtime.boards.insert(
        "b2".into(),
        crate::runtime::HardwareState {
            lifecycle: crate::board_lifecycle::BoardLifecycle::fixture_usb(
                crate::board_lifecycle::Phase::Connected,
                false,
                None,
                Some(true),
                0,
            ),
            temperature_unix: 1000,
            device_location: Some(3),
            ..Default::default()
        },
    );
    // No socket or recent raw IPS is required for an idle, supervised owner.
    for elapsed in [0, 10, 20, 30, 45, 59, 60, 90, 300] {
        let mut detected = devices(&[0, 1]);
        preserve_managed_boards(&config, &mut detected, &runtime, 1000 + elapsed);
        let next = connected_plan(config.clone(), detected).unwrap();
        assert_eq!(next.hardware.boards.len(), 3);
    }
}

#[test]
fn transport_and_sensor_recovery_do_not_become_false_usb_removals() {
    let config = configured();
    for phase in [
        crate::board_lifecycle::Phase::Connected,
        crate::board_lifecycle::Phase::Reconnecting,
        crate::board_lifecycle::Phase::Reconnecting,
        crate::board_lifecycle::Phase::Starting,
    ] {
        let mut runtime = crate::runtime::RuntimeState::default();
        runtime.boards.insert(
            "b2".into(),
            crate::runtime::HardwareState {
                lifecycle: crate::board_lifecycle::BoardLifecycle::fixture_usb(
                    phase,
                    false,
                    None,
                    Some(true),
                    0,
                ),
                temperature_unix: 1000,
                ..Default::default()
            },
        );
        let mut detected = devices(&[0, 1]);
        preserve_managed_boards(&config, &mut detected, &runtime, 2000);
        assert_eq!(
            connected_plan(config.clone(), detected)
                .unwrap()
                .hardware
                .boards
                .len(),
            3
        );
    }
}

#[test]
fn live_owner_preserves_anonymous_usb_row_without_overriding_a_replacement() {
    let config = configured();
    let mut runtime = crate::runtime::RuntimeState::default();
    runtime.boards.insert(
        "b2".into(),
        crate::runtime::HardwareState {
            lifecycle: crate::board_lifecycle::BoardLifecycle::fixture_usb(
                crate::board_lifecycle::Phase::Connected,
                false,
                None,
                Some(true),
                0,
            ),
            ..Default::default()
        },
    );
    let mut detected = devices(&[0, 1, 2]);
    detected[2].serial = None;
    preserve_managed_boards(&config, &mut detected, &runtime, 1001);
    assert_eq!(detected.len(), 3);
    assert_eq!(detected[2].serial.as_deref(), Some("serial-2"));
    let next = connected_plan(config.clone(), detected.clone()).unwrap();
    assert_eq!(next.assignments().unwrap(), config.assignments().unwrap());

    detected[2].serial = Some("replacement".into());
    preserve_managed_boards(&config, &mut detected, &runtime, 1001);
    assert_eq!(detected.len(), 3);
    assert_eq!(detected[2].serial.as_deref(), Some("replacement"));
}
#[test]
fn closed_owner_confirmed_absence_wins_over_old_socket_or_telemetry() {
    let config = configured();
    let mut runtime = crate::runtime::RuntimeState::default();
    runtime.boards.insert(
        "b2".into(),
        crate::runtime::HardwareState {
            lifecycle: crate::board_lifecycle::BoardLifecycle::fixture_usb(
                crate::board_lifecycle::Phase::Connected,
                false,
                None,
                Some(false),
                0,
            ),
            temperature_unix: 1000,
            ..Default::default()
        },
    );
    let mut detected = devices(&[0, 1]);
    preserve_managed_boards(&config, &mut detected, &runtime, 1001);
    assert_eq!(
        connected_plan(config, detected)
            .unwrap()
            .hardware
            .boards
            .len(),
        2
    );
}
#[test]
fn absent_faulted_or_stale_owner_does_not_hide_an_unplugged_board() {
    let config = configured();
    for (phase, latched, age) in [
        (crate::board_lifecycle::Phase::Reconnecting, false, 1),
        (crate::board_lifecycle::Phase::Reconnecting, false, 1),
        (crate::board_lifecycle::Phase::Starting, false, 1),
        (crate::board_lifecycle::Phase::Connected, true, 1),
        (crate::board_lifecycle::Phase::Connected, false, 61),
    ] {
        let mut runtime = crate::runtime::RuntimeState::default();
        runtime.boards.insert(
            "b2".into(),
            crate::runtime::HardwareState {
                lifecycle: crate::board_lifecycle::BoardLifecycle::fixture_usb(
                    phase,
                    false,
                    latched.then_some(crate::board_lifecycle::Fault::InvalidProof),
                    None,
                    0,
                ),
                temperature_unix: 1000,
                ..Default::default()
            },
        );
        let mut detected = devices(&[0, 1]);
        preserve_managed_boards(&config, &mut detected, &runtime, 1000 + age);
        let next = connected_plan(config.clone(), detected).unwrap();
        assert_eq!(next.hardware.boards.len(), 2);
        assert_eq!(
            next.assignments()
                .unwrap()
                .iter()
                .filter(|a| a.board_id == "b0")
                .count(),
            2
        );
    }
}
#[test]
fn explicit_mapping_keeps_absent_board_while_refreshing_connected_locations() {
    let mut config = Config::default();
    config.hardware.boards = (0..3)
        .map(|i| {
            toml::from_str(&format!(
                "id='b{i}'\nname='B{i}'\nserial='serial-{i}'\ndevice_location={}\nclient_port={}",
                i + 1,
                i + 18110
            ))
            .unwrap()
        })
        .collect();
    config.hardware.chain_assignments = Some(timelord_core::ChainAssignments {
        challenge: "serial-2".into(),
        reward: "serial-0".into(),
        infused_challenge: "serial-1".into(),
    });
    let expected = config.assignments().unwrap();
    let devices = vec![Device {
        name: "Chia VDF A".into(),
        location: 99,
        serial: Some("serial-2".into()),
    }];
    let plan = connected_plan(config.clone(), devices).unwrap();
    assert_eq!(plan.assignments().unwrap(), expected);
    assert_eq!(plan.hardware.boards.len(), 3);
    assert_eq!(plan.hardware.boards[2].device_location, Some(99));
    assert_eq!(
        connected_plan(config, vec![])
            .unwrap()
            .assignments()
            .unwrap(),
        expected
    );
}
#[test]
fn hotplug_follows_the_serial_and_never_a_replacement_at_the_old_port() {
    let board =
        toml::from_str("id='a'\nname='A'\nclient_port=18110\ndevice_location=1\nserial='known'")
            .unwrap();
    let other = Device {
        name: "Chia VDF A".into(),
        location: 1,
        serial: Some("other".into()),
    };
    assert!(board_location(&board, &[]).is_err());
    assert!(board_location(&board, std::slice::from_ref(&other)).is_err());
    let moved = Device {
        location: 9,
        serial: Some("known".into()),
        ..other.clone()
    };
    assert_eq!(board_location(&board, &[other, moved.clone()]).unwrap(), 9);
    assert!(board_location(&board, &[moved.clone(), moved]).is_err());
}

#[test]
fn a_replacement_at_the_old_location_never_overwrites_the_remembered_board() {
    let x: timelord_core::BoardConfig = toml::from_str(
        "id='usb-4'\nname='Old board'\nserial='serial-0'\nprofile='manual'\ndevice_location=4\nclient_port=18110\nfrequency_mhz=1500\nvoltage_v=0.95",
    )
    .unwrap();
    let collided: timelord_core::BoardConfig = toml::from_str(
        "id='usb-4'\nname='Replacement board'\nserial='serial-1'\nprofile='automatic'\ndevice_location=4\nclient_port=18111",
    )
    .unwrap();
    let mut collision = Config::default();
    collision.hardware.remembered_boards = vec![x.clone()];
    collision.hardware.boards = vec![collided];
    let collision = connected_plan(
        collision,
        vec![Device {
            name: "ASIC 2".into(),
            location: 4,
            serial: Some("serial-1".into()),
        }],
    )
    .expect("a pre-existing id collision must not break discovery");
    let preserved = collision
        .hardware
        .remembered_boards
        .iter()
        .find(|board| board.serial.as_deref() == Some("serial-0"))
        .expect("the remembered board survives a same-id collision");
    assert_eq!(preserved.id, "usb-4");
    assert_eq!(preserved.serial.as_deref(), Some("serial-0"));
    assert_eq!(preserved.profile, timelord_core::ProfileMode::Manual);
    assert_eq!(preserved.frequency_mhz, x.frequency_mhz);
    assert_eq!(preserved.voltage_v, x.voltage_v);

    let mut config = Config::default();
    config.hardware.boards = vec![x.clone()];
    let y = Device {
        name: "ASIC 2".into(),
        location: 4,
        serial: Some("serial-1".into()),
    };
    let mut plan = connected_plan(config, vec![y.clone()]).unwrap();
    plan = connected_plan(plan, vec![y.clone()]).unwrap();
    let remembered = plan
        .hardware
        .remembered_boards
        .iter()
        .find(|board| board.serial.as_deref() == Some("serial-0"))
        .expect("the unplugged board keeps its remembered identity");
    assert_eq!(remembered.id, "usb-4");
    assert_eq!(remembered.profile, timelord_core::ProfileMode::Manual);
    assert_eq!(remembered.frequency_mhz, x.frequency_mhz);
    assert_eq!(remembered.voltage_v, x.voltage_v);
    let live = plan
        .hardware
        .boards
        .iter()
        .find(|board| board.serial.as_deref() == Some("serial-1"))
        .expect("the replacement board is live");
    assert_eq!(live.id, "usb-4-1");
    assert_eq!(live.profile, timelord_core::ProfileMode::Automatic);

    let next = connected_plan(
        plan,
        vec![
            y,
            Device {
                name: "ASIC 1".into(),
                location: 5,
                serial: Some("serial-0".into()),
            },
        ],
    )
    .unwrap();
    let restored = next
        .hardware
        .boards
        .iter()
        .find(|board| board.serial.as_deref() == Some("serial-0"))
        .expect("the original board returns");
    assert_eq!(restored.id, "usb-4");
    assert_eq!(restored.profile, timelord_core::ProfileMode::Manual);
    assert_eq!(restored.frequency_mhz, x.frequency_mhz);
    assert_eq!(restored.device_location, Some(5));
    assert!(next.hardware.boards.iter().all(|board| board.id != "usb-5"));
}

#[derive(serde::Deserialize)]
struct IdentityVectors {
    vectors: Vec<IdentityVector>,
}
#[derive(serde::Deserialize)]
struct IdentityVector {
    name: String,
    #[serde(default)]
    boards: Vec<VectorBoard>,
    #[serde(default)]
    remembered_boards: Vec<VectorBoard>,
    devices: Vec<VectorDevice>,
    expected_boards: Vec<VectorBoard>,
    #[serde(default)]
    expected_remembered_boards: Option<Vec<VectorBoard>>,
}
#[derive(serde::Deserialize)]
struct VectorBoard {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    client_port: Option<u16>,
    #[serde(default)]
    device_location: Option<u32>,
    #[serde(default)]
    serial: Option<String>,
}
#[derive(serde::Deserialize)]
struct VectorDevice {
    #[serde(default)]
    name: Option<String>,
    location: u32,
    #[serde(default)]
    serial: Option<String>,
}

fn vector_board(board: &VectorBoard, index: usize) -> timelord_core::BoardConfig {
    let mut toml = format!(
        "id='{}'\nname='{}'\nclient_port={}\ndevice_location={}\nprofile='automatic'",
        board.id,
        board
            .name
            .clone()
            .unwrap_or_else(|| format!("ASIC {}", index + 1)),
        board.client_port.unwrap_or(18110 + index as u16),
        board.device_location.unwrap_or_else(|| index as u32 + 1)
    );
    if let Some(serial) = &board.serial {
        toml.push_str(&format!("\nserial='{serial}'"));
    }
    toml::from_str(&toml).unwrap()
}
fn assert_boards(actual: &[timelord_core::BoardConfig], expected: &[VectorBoard], name: &str) {
    assert_eq!(actual.len(), expected.len(), "{name}");
    for (board, expected) in actual.iter().zip(expected) {
        assert_eq!(board.id, expected.id, "{name}");
        assert_eq!(board.serial, expected.serial, "{name}");
        assert_eq!(board.device_location, expected.device_location, "{name}");
    }
}
#[test]
fn shared_board_identity_vectors_match_the_service_plan() {
    let vectors: IdentityVectors = serde_json::from_str(include_str!(
        "../../../../tests/board-identity-vectors.json"
    ))
    .unwrap();
    for vector in vectors.vectors {
        let mut config = Config::default();
        config.hardware.boards = vector
            .boards
            .iter()
            .enumerate()
            .map(|(index, board)| vector_board(board, index))
            .collect();
        config.hardware.remembered_boards = vector
            .remembered_boards
            .iter()
            .enumerate()
            .map(|(index, board)| vector_board(board, index))
            .collect();
        let devices = vector
            .devices
            .iter()
            .map(|device| Device {
                name: device.name.clone().unwrap_or_else(|| "ASIC".into()),
                location: device.location,
                serial: device.serial.clone(),
            })
            .collect();
        let plan = connected_plan(config, devices)
            .unwrap_or_else(|error| panic!("{}: {error:#}", vector.name));
        assert_boards(&plan.hardware.boards, &vector.expected_boards, &vector.name);
        let ids: Vec<_> = plan.hardware.boards.iter().map(|b| &b.id).collect();
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{}", vector.name);
        if let Some(expected) = vector.expected_remembered_boards {
            assert_boards(&plan.hardware.remembered_boards, &expected, &vector.name);
        }
    }
}

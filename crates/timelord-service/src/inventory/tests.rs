use super::*;
#[tokio::test]
async fn initialization_wait_does_not_arm_discovery_watchdog() {
    let process = crate::operational::State::fixture();
    let mut config = Config::default();
    config.hardware.client_binary = Some("/not-present/client".into());
    config.hardware.driver_library = Some("/not-present/driver".into());
    let state = crate::runtime::RuntimeState::new(false);
    let shared = crate::Shared {
        native: None,
        config,
        assignments: vec![],
        clients: Default::default(),
        process: process.clone(),
        scope: crate::runtime::RuntimeScope::Production,
        runtime: std::sync::Arc::new(std::sync::Mutex::new(state)),
        started_unix: 0,
        service_started_unix: 0,
    };
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

#[test]
fn admitted_capacity_requires_matching_fresh_inventory_despite_display_changes() {
    let config = configured();
    let board = &config.hardware.boards[0];
    {
        let mut h = crate::runtime::HardwareState {
            lifecycle: {
                let mut owner = crate::board_lifecycle::BoardLifecycle::default();
                owner.testing().admitted(7);
                owner
            },
            ..Default::default()
        };
        let blocked = |h: &crate::runtime::HardwareState| {
            crate::hardware::unavailable_for(
                &config,
                &std::collections::HashMap::from([(board.id.clone(), h.clone())]),
                None,
            )
            .contains(&board.id)
        };
        assert!(blocked(&h));
        confirm_admitted_presence(&mut h, board, &devices(&[0]), 6);
        assert!(blocked(&h), "old inventory released a newer admission");
        confirm_admitted_presence(&mut h, board, &[], 7);
        assert!(blocked(&h));
        let mut duplicate = devices(&[0]);
        duplicate.extend(devices(&[0]));
        confirm_admitted_presence(&mut h, board, &duplicate, 7);
        assert!(blocked(&h), "ambiguous inventory released capacity");
        confirm_admitted_presence(&mut h, board, &devices(&[0]), 7);
        assert!(!blocked(&h));
        assert_eq!(h.lifecycle.status().usb_present, Some(true));
    }
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
fn admitted_board_unplug_during_debounce_removes_capacity_and_presence_fallback() {
    let config = configured();
    let board = &config.hardware.boards[0];
    let mut state = crate::runtime::RuntimeState::default();
    let mut hardware = crate::runtime::HardwareState::default();
    hardware.lifecycle.testing().begin_attempt(1);
    hardware.lifecycle.testing().admitted(7);
    confirm_admitted_presence(&mut hardware, board, &devices(&[0]), 7);
    assert!(hardware.lifecycle.capacity(2, true).usable);
    assert_eq!(hardware.lifecycle.admitted_inventory_epoch(), Some(7));
    let epoch = hardware.lifecycle.admitted_inventory_epoch().unwrap();
    confirm_admitted_presence(&mut hardware, board, &[], epoch);
    assert!(!hardware.lifecycle.capacity(3, true).usable);
    assert_eq!(hardware.lifecycle.status().usb_present, Some(false));
    state.boards.insert(board.id.clone(), hardware);
    let mut empty = vec![];
    preserve_managed_boards(&config, &mut empty, &state, 3);
    assert!(empty.is_empty());
    let hardware = state.boards.get_mut(&board.id).unwrap();
    hardware.lifecycle.testing().start_native();
    confirm_admitted_presence(hardware, board, &devices(&[0]), epoch);
    assert_eq!(hardware.lifecycle.status().usb_present, Some(false));
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
fn topology_requires_three_consistent_successes_and_ignores_order() {
    let current = configured();
    let one = connected_plan(current.clone(), devices(&[0])).unwrap();
    let two = connected_plan(current.clone(), devices(&[0, 1])).unwrap();
    let mut debounce = TopologyChange::default();
    let mut now = std::time::Instant::now();
    let mut observe = |candidate| {
        now += std::time::Duration::from_secs(5);
        debounce.observe_at(&current, candidate, now)
    };
    assert!(observe(Ok(one.clone())).is_none());
    assert!(observe(Ok(one.clone())).is_none());
    assert!(observe(Err(anyhow::anyhow!("busy"))).is_none());
    assert!(observe(Ok(one.clone())).is_none());
    assert!(observe(Ok(two)).is_none());
    assert!(observe(Ok(one.clone())).is_none());
    assert!(observe(Ok(current.clone())).is_none());
    let mut reordered = current.clone();
    reordered.hardware.boards.reverse();
    assert!(observe(Ok(reordered)).is_none());
    assert!(observe(Ok(one.clone())).is_none());
    assert!(observe(Ok(one.clone())).is_none());
    assert_eq!(observe(Ok(one)).unwrap().hardware.boards.len(), 1);
}
#[test]
fn missed_poll_ticks_cannot_shorten_the_debounce_period() {
    let current = configured();
    let one = connected_plan(current.clone(), devices(&[0])).unwrap();
    let mut change = TopologyChange::default();
    let now = std::time::Instant::now();
    for _ in 0..4 {
        assert!(change.observe_at(&current, Ok(one.clone()), now).is_none());
    }
    assert!(
        change
            .observe_at(
                &current,
                Ok(one.clone()),
                now + std::time::Duration::from_secs(9)
            )
            .is_none()
    );
    assert!(
        change
            .observe_at(&current, Ok(one), now + std::time::Duration::from_secs(10))
            .is_some()
    );
}
#[test]
fn ambiguous_serial_is_not_a_topology_change() {
    assert!(connected_plan(configured(), devices(&[0, 0])).is_err());
}
#[test]
fn visible_managed_board_is_not_synthesized_twice() {
    let config = configured();
    let mut runtime = crate::runtime::RuntimeState::default();
    let mut board = crate::runtime::HardwareState::default();
    board
        .lifecycle
        .testing()
        .inventory(None, crate::board_lifecycle::Inventory::Present);
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
            lifecycle: crate::board_lifecycle::BoardLifecycle::fixture(
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
    let mut topology = TopologyChange::default();
    let instant = std::time::Instant::now();
    // No socket or recent raw IPS is required for an idle, supervised owner.
    for elapsed in [0, 10, 20, 30, 45, 59, 60, 90, 300] {
        let mut detected = devices(&[0, 1]);
        preserve_managed_boards(&config, &mut detected, &runtime, 1000 + elapsed);
        let next = connected_plan(config.clone(), detected).unwrap();
        assert_eq!(next.hardware.boards.len(), 3);
        assert!(
            topology
                .observe_at(&config, Ok(next), instant + Duration::from_secs(elapsed))
                .is_none()
        );
    }
}

#[test]
fn transport_and_sensor_recovery_do_not_become_false_usb_removals() {
    let config = configured();
    for phase in [
        crate::board_lifecycle::Phase::Connected,
        crate::board_lifecycle::Phase::Fault,
        crate::board_lifecycle::Phase::Reconnecting,
        crate::board_lifecycle::Phase::Starting,
    ] {
        let mut runtime = crate::runtime::RuntimeState::default();
        runtime.boards.insert(
            "b2".into(),
            crate::runtime::HardwareState {
                lifecycle: crate::board_lifecycle::BoardLifecycle::fixture(
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
            lifecycle: crate::board_lifecycle::BoardLifecycle::fixture(
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
            lifecycle: crate::board_lifecycle::BoardLifecycle::fixture(
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
        (crate::board_lifecycle::Phase::Fault, false, 1),
        (crate::board_lifecycle::Phase::Starting, false, 1),
        (crate::board_lifecycle::Phase::Connected, true, 1),
        (crate::board_lifecycle::Phase::Connected, false, 61),
    ] {
        let mut runtime = crate::runtime::RuntimeState::default();
        runtime.boards.insert(
            "b2".into(),
            crate::runtime::HardwareState {
                lifecycle: crate::board_lifecycle::BoardLifecycle::fixture(
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

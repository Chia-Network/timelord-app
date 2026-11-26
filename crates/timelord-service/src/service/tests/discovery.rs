use super::*;

async fn status_boards(address: std::net::SocketAddr) -> Option<Vec<serde_json::Value>> {
    Some(
        fetch_status(address).await.ok()?["boards"]
            .as_array()?
            .clone(),
    )
}

fn board_ids(boards: &[serde_json::Value]) -> Vec<String> {
    boards
        .iter()
        .filter_map(|board| board["id"].as_str().map(str::to_owned))
        .collect()
}

fn manual_board(id: &str, serial: &str, location: u32, port: u16) -> timelord_core::BoardConfig {
    toml::from_str(&format!(
        "id='{id}'\nname='{id}'\nserial='{serial}'\nprofile='manual'\nclient_port={port}\ndevice_location={location}"
    ))
    .unwrap()
}

async fn wait_for_board_ids(
    address: std::net::SocketAddr,
    expected: &[&str],
    task: &mut tokio::task::JoinHandle<Result<()>>,
    budget: Duration,
) -> Option<Vec<String>> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(boards) = status_boards(address).await {
            let ids = board_ids(&boards);
            if ids.len() == expected.len()
                && expected
                    .iter()
                    .all(|id| ids.iter().any(|actual| actual == id))
            {
                return Some(ids);
            }
        }
        if task.is_finished() || tokio::time::Instant::now() >= deadline {
            return status_boards(address)
                .await
                .map(|boards| board_ids(&boards));
        }
        yield_paused(16).await;
    }
}

async fn wait_for_board_location(
    address: std::net::SocketAddr,
    id: &str,
    location: u32,
    budget: Duration,
) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(boards) = status_boards(address).await
            && boards
                .iter()
                .any(|board| board["id"] == id && board["device_location"] == location)
        {
            return boards;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{id} never moved to device location {location}"
        );
        tokio::time::advance(Duration::from_millis(100)).await;
        yield_paused(16).await;
    }
}

#[tokio::test(start_paused = true)]
async fn continuous_discovery_restores_a_configured_board_missing_from_inventory() {
    let dir = fixture_dir("discover-known-config");
    let binary = dir.join("client");
    crate::test_scripts::write_executable_script(&binary, "#!/bin/sh\nexit 0\n");
    let mut listeners = Vec::new();
    for _ in 0..3 {
        listeners.push(TcpListener::bind("127.0.0.1:0").await.unwrap());
    }
    let ports: Vec<u16> = listeners
        .iter()
        .map(|listener| listener.local_addr().unwrap().port())
        .collect();
    let status = loopback_status().await;
    drop(listeners);
    let config_path = dir.join("config.toml");
    // Disabling TL-TEST-3 keeps b2 from launching the exit-0 client.
    crate::safety::Store::open(&config_path)
        .unwrap()
        .update_health("TL-TEST-3", false, |health| health.disable(None))
        .unwrap();
    let mut config = Config::default();
    config.service.status_listen = status;
    config.hardware.client_binary = Some(binary.clone());
    for (index, port) in ports.iter().enumerate() {
        config.hardware.boards.push(manual_board(
            &format!("b{index}"),
            &format!("TL-TEST-{}", index + 1),
            index as u32 + 1,
            *port,
        ));
    }
    crate::inventory::fixture::set(binary.clone(), discovered_devices(3));
    let (signal, shutdown) = tokio::sync::oneshot::channel::<()>();
    let mut task = tokio::spawn(serve(
        config,
        dir.join("history.json"),
        config_path,
        false,
        async {
            let _ = shutdown.await;
        },
    ));
    let ids = wait_for_board_ids(
        status,
        &["b0", "b1", "b2"],
        &mut task,
        Duration::from_secs(5),
    )
    .await
    .expect("status listener reports the configured boards");
    assert_eq!(ids.len(), 3);
    crate::inventory::fixture::set(
        binary,
        vec![
            crate::inventory::Device {
                name: "ASIC 1".into(),
                location: 1,
                serial: Some("TL-TEST-1".into()),
            },
            crate::inventory::Device {
                name: "ASIC 3".into(),
                location: 4,
                serial: Some("TL-TEST-3".into()),
            },
        ],
    );
    let boards = wait_for_board_location(status, "b2", 4, Duration::from_secs(12)).await;
    let ids = board_ids(&boards);
    assert_eq!(
        ids.len(),
        3,
        "continuous discovery must keep the absent configured board"
    );
    assert!(
        ids.iter().any(|id| id == "b1"),
        "the absent configured board must be restored"
    );
    assert!(
        boards
            .iter()
            .any(|board| board["id"] == "b2" && board["serial"] == "TL-TEST-3"),
        "the configured board must remain at its new location"
    );
    let _ = signal.send(());
    yield_paused(64).await;
    tokio::time::advance(Duration::from_secs(3)).await;
    yield_paused(64).await;
    await_shutdown(&mut task, Duration::from_secs(10))
        .await
        .expect("shutdown after discovery");
    await_history_writers(Duration::from_secs(10)).await;
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(start_paused = true)]
async fn startup_promotes_remembered_board_before_the_first_inventory_poll() {
    let dir = fixture_dir("startup-known-config");
    let binary = dir.join("client");
    crate::test_scripts::write_executable_script(&binary, "#!/bin/sh\nexit 0\n");
    let mut listeners = Vec::new();
    for _ in 0..3 {
        listeners.push(TcpListener::bind("127.0.0.1:0").await.unwrap());
    }
    let ports: Vec<u16> = listeners
        .iter()
        .map(|listener| listener.local_addr().unwrap().port())
        .collect();
    let status = loopback_status().await;
    drop(listeners);
    let mut config = Config::default();
    config.service.status_listen = status;
    config.hardware.client_binary = Some(binary.clone());
    for (index, port) in ports.iter().take(2).enumerate() {
        config.hardware.boards.push(manual_board(
            &format!("b{index}"),
            &format!("TL-TEST-{}", index + 1),
            index as u32 + 1,
            *port,
        ));
    }
    config
        .hardware
        .remembered_boards
        .push(manual_board("b2", "TL-TEST-3", 3, ports[2]));
    crate::inventory::fixture::set(binary, discovered_devices(3));
    // Keep inventory from running until the startup configuration is observed.
    let initialization = crate::hardware::INITIALIZATION.lock().await;
    let (signal, shutdown) = tokio::sync::oneshot::channel::<()>();
    let mut task = tokio::spawn(serve(
        config,
        dir.join("history.json"),
        dir.join("config.toml"),
        false,
        async {
            let _ = shutdown.await;
        },
    ));
    let ids = wait_for_board_ids(
        status,
        &["b0", "b1", "b2"],
        &mut task,
        Duration::from_secs(5),
    )
    .await
    .expect("status listener reports the startup configuration");
    assert_eq!(
        ids.len(),
        3,
        "startup must publish the remembered board before inventory"
    );
    let boards = status_boards(status)
        .await
        .expect("status after startup configuration");
    assert!(
        boards
            .iter()
            .any(|board| board["id"] == "b2" && board["serial"] == "TL-TEST-3"),
        "the remembered board must be present at startup"
    );
    let _ = signal.send(());
    yield_paused(64).await;
    tokio::time::advance(Duration::from_secs(3)).await;
    yield_paused(64).await;
    await_shutdown(&mut task, Duration::from_secs(10))
        .await
        .expect("shutdown after startup");
    await_history_writers(Duration::from_secs(10)).await;
    drop(initialization);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn reload_node_accepts_a_node_edit_with_a_remembered_board() {
    let dir = fixture_dir("reload-node-remembered");
    let path = dir.join("config.toml");
    let mut listeners = Vec::new();
    for _ in 0..3 {
        listeners.push(TcpListener::bind("127.0.0.1:0").await.unwrap());
    }
    let ports: Vec<u16> = listeners
        .iter()
        .map(|listener| listener.local_addr().unwrap().port())
        .collect();
    drop(listeners);
    let mut config = Config::default();
    for (index, port) in ports.iter().take(2).enumerate() {
        config.hardware.boards.push(manual_board(
            &format!("b{index}"),
            &format!("rn{index}"),
            index as u32 + 1,
            *port,
        ));
    }
    config
        .hardware
        .remembered_boards
        .push(manual_board("b2", "rn2", 3, ports[2]));
    crate::save(&path, &config).unwrap();
    let mut running = crate::load(&path).unwrap();
    running.service.control_socket = Some(crate::automatic::socket_path(&path, &running));
    crate::set_default_automatic_state(&mut running, &path).unwrap();
    let (node_requests, _node_reloads) = tokio::sync::mpsc::channel(1);
    let mut run = start_run(
        hardware::continuous::known_configuration(running),
        None,
        dir.join("history.json"),
        Arc::new(safety::Store::open(&path).unwrap()),
        unix_now(),
        operational::State::fixture(),
        node_requests,
    )
    .await
    .unwrap();
    let (current, _state) = watch::channel(run.shared.clone());
    assert!(
        run.shared
            .config
            .hardware
            .boards
            .iter()
            .any(|board| board.id == "b2")
    );
    assert!(
        node_reload(&mut run, &path, &current)
            .await
            .unwrap()
            .is_none()
    );
    let mut edited = crate::load(&path).unwrap();
    edited.node_mut().port = 58444;
    crate::save(&path, &edited).unwrap();
    assert!(
        node_reload(&mut run, &path, &current)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(run.shared.config.node().port, 58444);
    run.stop().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

use super::*;

async fn status_boards(address: std::net::SocketAddr) -> Option<Vec<serde_json::Value>> {
    let mut socket = TcpStream::connect(address).await.ok()?;
    socket
        .write_all(b"GET /v1/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .ok()?;
    let mut response = String::new();
    socket.read_to_string(&mut response).await.ok()?;
    let (_, body) = response.split_once("\r\n\r\n")?;
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(value["boards"].as_array()?.clone())
}

fn board_ids(boards: &[serde_json::Value]) -> Vec<String> {
    boards
        .iter()
        .filter_map(|board| board["id"].as_str().map(str::to_owned))
        .collect()
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
    crate::safety::Store::open(&config_path)
        .unwrap()
        .update_health("TL-TEST-3", false, |health| health.disable(None))
        .unwrap();
    let mut config = Config::default();
    config.service.status_listen = status;
    config.hardware.client_binary = Some(binary.clone());
    for (index, port) in ports.iter().enumerate() {
        config.hardware.boards.push(
            toml::from_str(&format!(
                "id='b{index}'\nname='B{index}'\nserial='TL-TEST-{}'\nprofile='manual'\nclient_port={port}\ndevice_location={}",
                index + 1,
                index + 1
            ))
            .unwrap(),
        );
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
    crate::inventory::fixture::set(binary, discovered_devices(2));
    for _ in 0..2 {
        tokio::time::advance(Duration::from_secs(6)).await;
        yield_paused(256).await;
    }
    let boards = status_boards(status)
        .await
        .expect("status after continuous discovery");
    let ids = board_ids(&boards);
    assert_eq!(
        ids.len(),
        3,
        "continuous discovery must keep the absent configured board"
    );
    assert!(
        boards
            .iter()
            .any(|board| board["id"] == "b2" && board["serial"] == "TL-TEST-3"),
        "the absent configured board must be restored"
    );
    let _ = signal.send(());
    yield_paused(64).await;
    tokio::time::advance(Duration::from_secs(3)).await;
    yield_paused(64).await;
    let result = tokio::time::timeout(Duration::from_secs(1), &mut task)
        .await
        .expect("shutdown after discovery");
    result.unwrap().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

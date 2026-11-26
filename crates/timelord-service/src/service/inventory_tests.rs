use super::test_support::{fixture_dir, run_from};
use super::*;
use timelord_core::Network;

fn board(profile: &str) -> timelord_core::BoardConfig {
    toml::from_str(&format!(
        "id='inv-0'\nname='Inventory 0'\nserial='inv-0'\nprofile='{profile}'\nclient_port=18110\ndevice_location=1"
    ))
    .unwrap()
}

#[tokio::test]
async fn replace_node_during_inventory_keeps_the_new_node() {
    let dir = fixture_dir("inventory-keeps-node");
    let (mut run, current) = run_from(&dir, Config::default()).await;
    let board = board("manual");
    let stale = crate::inventory::Topology {
        boards: vec![board.clone()],
        remembered_boards: vec![board],
    };
    let mut testnet = run.shared.config.clone();
    testnet.node_mut().network = Network::Testnet11;
    testnet.node_mut().port = 58444;
    run.replace_node(testnet, &current).await.unwrap();
    assert!(
        run.apply_inventory(Some(stale), &current)
            .unwrap()
            .is_none()
    );
    assert_eq!(run.shared.config.node().network, Network::Testnet11);
    assert_eq!(run.shared.config.node().port, 58444);
    assert_eq!(run.shared.config.hardware.boards.len(), 1);
    assert_eq!(run.shared.config.hardware.boards[0].id, "inv-0");
    assert_eq!(run.shared.config.hardware.remembered_boards.len(), 1);
    assert_eq!(run.shared.config.hardware.remembered_boards[0].id, "inv-0");
    run.stop().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn apply_inventory_returns_prepare_config_with_the_live_node() {
    let dir = fixture_dir("inventory-prepare-node");
    let (mut run, current) = run_from(&dir, Config::default()).await;
    let mut testnet = run.shared.config.clone();
    testnet.node_mut().network = Network::Testnet11;
    testnet.node_mut().port = 58444;
    run.replace_node(testnet, &current).await.unwrap();
    let topology = crate::inventory::Topology {
        boards: vec![board("automatic")],
        remembered_boards: vec![],
    };
    let prepared = run
        .apply_inventory(Some(topology), &current)
        .unwrap()
        .expect("Automatic board requires a prepare config");
    assert_eq!(prepared.node().network, Network::Testnet11);
    assert_eq!(prepared.node().port, 58444);
    assert_eq!(prepared.hardware.boards.len(), 1);
    assert_eq!(prepared.hardware.boards[0].id, "inv-0");
    assert!(run.shared.config.hardware.boards.is_empty());
    assert_eq!(run.shared.config.node().network, Network::Testnet11);
    run.stop().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn replace_node_keeps_the_live_hardware_profile() {
    let dir = fixture_dir("inventory-replace-keeps-hardware");
    let mut live = Config::default();
    let live_board = board("manual");
    live.hardware.boards = vec![live_board.clone()];
    live.hardware.remembered_boards = vec![live_board];
    let (mut run, current) = run_from(&dir, live).await;
    let hardware = serde_json::to_value(&run.shared.config.hardware).unwrap();
    let mut disk = Config::default();
    disk.node_mut().network = Network::Testnet11;
    disk.node_mut().port = 58444;
    let mut disk_board = board("manual");
    disk_board.device_location = None;
    disk.hardware.boards = vec![disk_board];
    run.replace_node(disk, &current).await.unwrap();
    assert_eq!(
        serde_json::to_value(&run.shared.config.hardware).unwrap(),
        hardware
    );
    assert_eq!(run.shared.config.node().network, Network::Testnet11);
    assert_eq!(run.shared.config.node().port, 58444);
    run.stop().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn replace_node_adopts_the_disk_connection_profile() {
    let dir = fixture_dir("inventory-replace-connection-profile");
    let mut live: Config = toml::from_str(
        r#"
schema_version = 1
[connections]
active_id = "mainnet"
[[connections.profiles]]
id = "mainnet"
name = "Mainnet"
node = { host = "127.0.0.1", port = 8444, network = "mainnet" }
[[connections.profiles]]
id = "testnet"
name = "Testnet11"
node = { host = "127.0.0.1", port = 58444, network = "testnet11" }
"#,
    )
    .unwrap();
    let live_board = board("manual");
    live.hardware.boards = vec![live_board.clone()];
    live.hardware.remembered_boards = vec![live_board];
    let (mut run, current) = run_from(&dir, live).await;
    let hardware = serde_json::to_value(&run.shared.config.hardware).unwrap();
    let mut disk: Config = toml::from_str(
        r#"
schema_version = 1
[connections]
active_id = "testnet"
[[connections.profiles]]
id = "mainnet"
name = "Mainnet"
node = { host = "127.0.0.1", port = 8444, network = "mainnet" }
[[connections.profiles]]
id = "testnet"
name = "Testnet11"
node = { host = "127.0.0.1", port = 58444, network = "testnet11" }
"#,
    )
    .unwrap();
    let mut disk_board = board("manual");
    disk_board.device_location = None;
    disk.hardware.boards = vec![disk_board];
    let expected_node = disk.node().clone();
    run.replace_node(disk, &current).await.unwrap();
    assert_eq!(run.shared.config.node(), &expected_node);
    assert_eq!(
        serde_json::to_value(&run.shared.config.hardware).unwrap(),
        hardware
    );
    run.stop().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

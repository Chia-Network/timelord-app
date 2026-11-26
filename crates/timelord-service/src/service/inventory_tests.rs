use super::*;
use std::time::{SystemTime, UNIX_EPOCH};
use timelord_core::Network;

fn fixture_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tl-service-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn run_from(dir: &std::path::Path, config: Config) -> (Run, watch::Sender<Arc<Shared>>) {
    let (node_requests, _node_reloads) = tokio::sync::mpsc::channel(1);
    let run = start_run(
        config,
        None,
        dir.join("history.json"),
        Arc::new(safety::Store::open(&dir.join("config.toml")).unwrap()),
        unix_now(),
        operational::State::fixture(),
        node_requests,
    )
    .await
    .unwrap();
    let (current, _state) = watch::channel(run.shared.clone());
    (run, current)
}

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

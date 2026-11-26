use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn fixture_dir(name: &str) -> PathBuf {
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

pub(super) async fn run_from(
    dir: &std::path::Path,
    config: Config,
) -> (Run, watch::Sender<Arc<Shared>>) {
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

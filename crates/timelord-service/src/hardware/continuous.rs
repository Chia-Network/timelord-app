//! Persistent per-board owners and live chain routing. Board availability never
//! replaces the node session or another board's native process.
use super::*;
use std::collections::HashMap;
use timelord_core::Assignment;
use tokio::task::JoinSet;

pub(super) struct Group {
    pub(super) board: BoardConfig,
    pub(super) stop: watch::Sender<bool>,
    pub(super) task: tokio::task::JoinHandle<Result<()>>,
}

pub(crate) async fn own_group(
    mut tasks: JoinSet<()>,
    owner_stop: watch::Sender<bool>,
) -> Result<()> {
    let first = tasks.join_next().await;
    let stopping = *owner_stop.borrow();
    let _ = owner_stop.send(true);
    while let Some(result) = tasks.join_next().await {
        result.context("Board group cleanup failed")?;
    }
    if let Some(result) = first {
        result.context("Board group failed")?;
    }
    anyhow::ensure!(stopping, "Board owner group exited unexpectedly");
    Ok(())
}

pub(crate) async fn join_retired_owner(task: tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    task.await.context("Board owner group failed")?
}

pub(super) async fn retire_stale_groups(
    groups: &mut HashMap<String, Group>,
    desired: &[BoardConfig],
    backend: &crate::asic::AsicBackend,
) -> Result<()> {
    let retiring: Vec<_> = groups
        .iter()
        .filter(|(_, group)| {
            !desired.iter().any(|b| {
                b.id == group.board.id
                    && b.serial == group.board.serial
                    && b.client_port == group.board.client_port
            })
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in retiring {
        if let Some(group) = groups.remove(&id) {
            let _ = group.stop.send(true);
            join_retired_owner(group.task).await?;
            backend.remove_board(&id).await;
        }
    }
    Ok(())
}

pub(crate) fn known_configuration(mut config: Config) -> Config {
    for board in config.hardware.remembered_boards.clone() {
        if config.hardware.boards.len() < 3
            && !config.hardware.boards.iter().any(|b| b.id == board.id)
        {
            config.hardware.boards.push(board);
        }
    }
    config
}

pub(crate) async fn run(
    mut shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    mut updates: watch::Receiver<Arc<Shared>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut rebalance = Rebalance::default();
    let result: Result<()> = async {
        loop {
            if *shutdown.borrow() {
                break Ok(());
            }
            let desired = &shared.config.hardware.boards;
            retire_stale_groups(&mut groups, desired, &backend).await?;
            for board in desired {
                if groups.contains_key(&board.id) {
                    continue;
                }
                let listener = match tokio::net::TcpListener::bind(("127.0.0.1", board.client_port))
                    .await
                {
                    Ok(listener) => listener,
                    Err(error) => {
                        let mut state = shared.runtime.lock().expect("runtime");
                        let h = state.boards.entry(board.id.clone()).or_default();
                        h.recovery_reason = Some(format!("Waiting for ASIC client port: {error}"));
                        h.retry_after_unix = Some(crate::unix_now() + 1);
                        continue;
                    }
                };
                let offer = backend.add_board(&board.id);
                let (stop, rx) = watch::channel(false);
                let owner_stop = stop.clone();
                let source = shared.clone();
                let prover = backend.clone();
                let board = board.clone();
                let configured = board.clone();
                let task = tokio::spawn(async move {
                    let mut tasks = JoinSet::new();
                    tasks.spawn(supervise_board(
                        source.clone(),
                        prover,
                        board.clone(),
                        rx.clone(),
                    ));
                    tasks.spawn(async move {
                        if let Err(error) =
                            crate::service::client_listener(listener, board.id, source, rx, offer)
                                .await
                        {
                            tracing::error!(error=%format!("{error:#}"), "ASIC listener failed");
                        }
                    });
                    own_group(tasks, owner_stop).await
                });
                groups.insert(
                    configured.id.clone(),
                    Group {
                        board: configured,
                        stop,
                        task,
                    },
                );
            }
            if groups.values().any(|g| g.task.is_finished()) {
                break Err(anyhow::anyhow!(
                    "A persistent board owner exited unexpectedly"
                ));
            }
            reconcile(&shared, &mut rebalance)?;
            tokio::select! {
                _ = shutdown.changed() => break Ok(()),
                result = updates.changed() => {
                    if result.is_err() { break Ok(()); }
                    shared = updates.borrow_and_update().clone();
                },
                _ = tick.tick() => {},
            }
        }
    }
    .await;
    for group in groups.values() {
        let _ = group.stop.send(true);
    }
    for (_, group) in groups {
        if let Err(error) = group.task.await {
            tracing::warn!(%error, "Board group cleanup failed");
        }
    }
    result
}

/// How long a route change holds Automatic tuning on every board.
pub(crate) const DISTURBANCE_SECONDS: u64 = 20;

#[derive(Default)]
pub(crate) struct Rebalance {
    peak_epoch: u64,
}

/// Stable board order: by serial, then identifier. Configuration order and
/// re-appended returning boards never decide which board carries a chain.
fn candidates(
    config: &Config,
    state: &crate::runtime::RuntimeState,
    now: u64,
) -> Vec<routing::Candidate> {
    let mut boards: Vec<_> = config.hardware.boards.iter().collect();
    boards.sort_by(|a, b| (&a.serial, &a.id).cmp(&(&b.serial, &b.id)));
    boards
        .into_iter()
        .map(|board| {
            let health = state
                .safety
                .as_ref()
                .map(|store| store.policy(board).health)
                .unwrap_or_default();
            routing::Candidate {
                id: board.id.clone(),
                tier: health.tier(now),
                serving: state
                    .boards
                    .get(&board.id)
                    .is_some_and(|h| h.lifecycle.accepts_work()),
                failures_last_hour: health.failures_within(now, 3600),
            }
        })
        .collect()
}

pub(crate) fn reconcile(shared: &Shared, rebalance: &mut Rebalance) -> Result<()> {
    let now = crate::unix_now();
    let mut state = shared.runtime.lock().expect("runtime");
    let next = if shared.config.hardware.chain_assignments.is_some() {
        // Administrator-pinned mappings stay strict even while a board is away.
        shared.config.assignments().map_err(anyhow::Error::msg)?
    } else {
        // Improvements wait for a new peak (or idle node work) so they never
        // cancel in-flight proofs; forced moves happen immediately.
        let allow = state.peak_epoch != rebalance.peak_epoch
            || state.routing.is_empty()
            || !state.scheduler_initialized;
        rebalance.peak_epoch = state.peak_epoch;
        routing::plan(
            &candidates(&shared.config, &state, now),
            &state.routing,
            allow,
        )
    };
    if state.routing == next {
        return Ok(());
    }
    let moved: Vec<_> = next
        .iter()
        .filter(|a| {
            !state
                .routing
                .iter()
                .any(|old| old.chain == a.chain && old.board_id == a.board_id)
        })
        .map(|a| a.chain)
        .chain(
            state
                .routing
                .iter()
                .filter(|old| !next.iter().any(|a| a.chain == old.chain))
                .map(|a| a.chain),
        )
        .collect();
    let touched: std::collections::BTreeSet<_> = next
        .iter()
        .chain(state.routing.iter())
        .filter(|a| moved.contains(&a.chain))
        .map(|a| a.board_id.clone())
        .collect();
    // A route change restarts chain sessions on the touched boards. Hold
    // tuning everywhere briefly: engine restarts it causes are not instability.
    state.disturb(now, DISTURBANCE_SECONDS);
    for board in &touched {
        state.event(Some(board.clone()), "route_change");
    }
    if let Some(metrics) = shared.metrics() {
        metrics.topology_changed();
        metrics.chains_moved(moved.len() as u64);
    }
    tracing::info!(
        routes=%describe(&next),
        moved=?moved,
        "Updated chain routes; native owners and node session retained"
    );
    state.routing = next;
    Ok(())
}

fn describe(routes: &[Assignment]) -> String {
    if routes.is_empty() {
        return "none".into();
    }
    routes
        .iter()
        .map(|a| format!("{:?}->{}#{}", a.chain, a.board_id, a.engine))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn routed_assignments<'a>(
    _shared: &'a Shared,
    state: &'a crate::runtime::RuntimeState,
) -> &'a [Assignment] {
    &state.routing
}

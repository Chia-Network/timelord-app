//! Persistent per-board owners and live chain routing. Board availability never
//! replaces the node session or another board's native process.
use super::*;
use std::collections::HashMap;
use timelord_core::Assignment;
#[cfg(test)]
use timelord_core::Chain;
use tokio::task::JoinSet;

struct Group {
    board: BoardConfig,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<()>>,
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
                    group.task.await.context("Board owner group failed")??;
                    backend.remove_board(&id).await;
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Shared, Arc<crate::safety::Store>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "tl-continuous-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(crate::safety::Store::open(&dir.join("config.toml")).unwrap());
        let mut config = Config::default();
        config.hardware.boards = (0..3)
            .map(|i| {
                toml::from_str(&format!(
                    "id='b{i}'\nname='B{i}'\nserial='S{i}'\nprofile='manual'\nclient_port={}",
                    18110 + i
                ))
                .unwrap()
            })
            .collect();
        let mut runtime = crate::runtime::RuntimeState::new(false);
        runtime.safety = Some(store.clone());
        runtime.scheduler_initialized = true;
        for board in &config.hardware.boards {
            let mut h = HardwareState::default();
            h.lifecycle.testing().serve(1);
            runtime.boards.insert(board.id.clone(), h);
        }
        let shared = Shared::new(
            None,
            config,
            Arc::new(std::sync::Mutex::new(runtime)),
            crate::operational::State::fixture(),
            1,
            1,
        );
        (shared, store, dir)
    }
    #[test]
    fn a_board_failure_moves_only_its_chain_without_erasing_history() {
        let (shared, _store, dir) = fixture();
        let mut rebalance = Rebalance::default();
        reconcile(&shared, &mut rebalance).unwrap();
        let initial = shared.assignments();
        assert_eq!(initial.len(), 3);
        shared
            .runtime
            .lock()
            .unwrap()
            .estimated(Chain::Reward, 1.0e6, 1, "fixture", 1);
        let lost = initial
            .iter()
            .find(|a| a.chain == Chain::Reward)
            .unwrap()
            .board_id
            .clone();
        shared
            .runtime
            .lock()
            .unwrap()
            .boards
            .get_mut(&lost)
            .unwrap()
            .lifecycle
            .testing()
            .fault(crate::board_lifecycle::Fault::Transport, "lost".into(), 2);
        reconcile(&shared, &mut rebalance).unwrap();
        let state = shared.runtime.lock().unwrap();
        assert!(state.routing.iter().all(|a| a.board_id != lost));
        for chain in [Chain::Challenge, Chain::InfusedChallenge] {
            assert_eq!(
                state.routing.iter().find(|a| a.chain == chain),
                initial.iter().find(|a| a.chain == chain)
            );
        }
        assert_eq!(state.estimated_observations.iter().count(), 1);
        assert!(state.disturbed_until_unix > crate::unix_now());
        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn a_failing_board_keeps_working_and_hands_its_critical_chain_over_at_a_peak() {
        let (shared, store, dir) = fixture();
        let mut rebalance = Rebalance::default();
        reconcile(&shared, &mut rebalance).unwrap();
        let holder = |chain: Chain| {
            shared
                .assignments()
                .into_iter()
                .find(|a| a.chain == chain)
                .unwrap()
                .board_id
        };
        let critical = holder(Chain::Challenge);
        let serial = match critical.as_str() {
            "b0" => "S0",
            "b1" => "S1",
            _ => "S2",
        };
        for _ in 0..3 {
            store
                .record_failure(serial, "Waiting for ASIC client timed out")
                .unwrap();
        }
        // Still serving: nothing moves between peaks and it is never idled.
        let before = shared.assignments();
        reconcile(&shared, &mut rebalance).unwrap();
        assert_eq!(shared.assignments(), before);
        shared.runtime.lock().unwrap().peak_epoch += 1;
        reconcile(&shared, &mut rebalance).unwrap();
        assert_eq!(holder(Chain::InfusedChallenge), critical);
        assert_ne!(holder(Chain::Challenge), critical);
        // The operator's retry clears the streak; it is preferred again.
        store
            .update_health(serial, true, |h| h.retry_now(crate::unix_now()))
            .unwrap();
        assert_eq!(store.health(serial).consecutive_failures, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

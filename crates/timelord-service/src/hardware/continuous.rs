//! Persistent per-board owners and live chain routing. Board availability never
//! replaces the node session or another board's native process.
use super::*;
use std::collections::HashMap;
use timelord_core::Assignment;
#[cfg(test)]
use timelord_core::Chain;

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
    mut offers: HashMap<String, tokio::sync::mpsc::Sender<crate::asic::Lease>>,
    mut updates: watch::Receiver<Arc<Shared>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
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
                    offers.remove(&id);
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
                let offer = offers
                    .remove(&board.id)
                    .unwrap_or_else(|| backend.add_board(&board.id));
                let (stop, rx) = watch::channel(false);
                let owner_stop = stop.clone();
                let source = shared.clone();
                let prover = backend.clone();
                let board = board.clone();
                let configured = board.clone();
                let task = tokio::spawn(async move {
                    let mut tasks = JoinSet::new();
                    let (ready, _initialized) = oneshot::channel();
                    tasks.spawn(supervise_board(
                        source.clone(),
                        prover,
                        board.clone(),
                        ready,
                        rx.clone(),
                        RestartBackoff::default(),
                    ));
                    tasks.spawn(async move {
                        if let Err(error) = crate::service::client_listener(
                            listener,
                            board.id,
                            3,
                            source,
                            rx,
                            Some(offer),
                        )
                        .await
                        {
                            tracing::error!(%error, "ASIC listener failed");
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
            reconcile(&shared)?;
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

fn reconcile(shared: &Shared) -> Result<()> {
    let mut state = shared.runtime.lock().expect("runtime");
    let unavailable: std::collections::HashSet<_> = shared
        .config
        .hardware
        .boards
        .iter()
        .filter(|board| {
            state
                .boards
                .get(&board.id)
                .is_none_or(|h| !h.lifecycle.capacity(crate::unix_now(), true).accepts_work)
        })
        .map(|b| b.id.clone())
        .collect();
    let mut candidate = shared.config.clone();
    if candidate.hardware.chain_assignments.is_none() {
        candidate
            .hardware
            .boards
            .retain(|board| !unavailable.contains(&board.id));
    }
    let mut next = candidate.assignments().map_err(anyhow::Error::msg)?;
    preserve_live_engines(&mut next, state.routing.as_deref().unwrap_or_default());
    if state.routing.as_ref() == Some(&next) {
        return Ok(());
    }
    // Chain observations are not board-labelled. Split their scope when a
    // chain moves, rather than attributing an old board's history to its peer.
    state.observations = Default::default();
    state.estimated_observations = Default::default();
    state.archive = None;
    state.event(None, "topology_change");
    state.routing = Some(next.clone());
    tracing::info!(plan=?next, "Updated chain routes; native owners and node session retained");
    Ok(())
}

// A capacity change can change the preferred engine order without changing a
// chain's board. Keep that chain's socket when its physical engine is still in
// the new mask; assign only the remaining chains to the remaining slots.
fn preserve_live_engines(next: &mut [Assignment], previous: &[Assignment]) {
    let slots: Vec<_> = next
        .iter()
        .map(|a| (a.board_id.clone(), a.engine))
        .collect();
    let mut used = std::collections::HashSet::new();
    let mut retained = Vec::new();
    for assignment in next.iter_mut() {
        if let Some(old) = previous
            .iter()
            .find(|old| old.chain == assignment.chain && old.board_id == assignment.board_id)
            && slots.contains(&(old.board_id.clone(), old.engine))
            && used.insert((old.board_id.clone(), old.engine))
        {
            assignment.engine = old.engine;
            retained.push(assignment.chain);
        }
    }
    for assignment in next.iter_mut().filter(|a| !retained.contains(&a.chain)) {
        let (_, engine) = slots
            .iter()
            .find(|(board, engine)| {
                board == &assignment.board_id && !used.contains(&(board.clone(), *engine))
            })
            .expect("available planned engine");
        assignment.engine = *engine;
        used.insert((assignment.board_id.clone(), *engine));
    }
}

pub(crate) fn routed_assignments<'a>(
    shared: &'a Shared,
    state: &'a crate::runtime::RuntimeState,
) -> &'a [Assignment] {
    state.routing.as_deref().unwrap_or(&shared.assignments)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expansion_keeps_a_healthy_chains_existing_physical_engine() {
        let previous = vec![Assignment {
            board_id: "a".into(),
            chain: Chain::Challenge,
            engine: 1,
        }];
        let mut next = vec![
            Assignment {
                board_id: "a".into(),
                chain: Chain::Challenge,
                engine: 0,
            },
            Assignment {
                board_id: "a".into(),
                chain: Chain::Reward,
                engine: 1,
            },
        ];
        preserve_live_engines(&mut next, &previous);
        assert_eq!(next[0].engine, 1);
        assert_eq!(next[1].engine, 0);
    }
    #[test]
    fn a_board_failure_changes_only_routes_and_keeps_native_generations() {
        let mut config = Config::default();
        config.hardware.boards = (0..3)
            .map(|i| {
                toml::from_str(&format!(
                    "id='b{i}'\nname='B{i}'\nprofile='manual'\nclient_port={}",
                    18110 + i
                ))
                .unwrap()
            })
            .collect();
        let assignments = config.assignments().unwrap();
        let mut runtime = crate::runtime::RuntimeState::new(false);
        runtime.routing = Some(assignments.clone());
        for board in &config.hardware.boards {
            let mut h = HardwareState::default();
            h.lifecycle.testing().start_native();
            h.lifecycle.recovered();
            runtime.boards.insert(board.id.clone(), h);
        }
        let shared = Shared {
            config,
            assignments,
            native: None,
            clients: Default::default(),
            runtime: Arc::new(std::sync::Mutex::new(runtime)),
            process: crate::operational::State::fixture(),
            scope: crate::runtime::RuntimeScope::Production,
            started_unix: 1,
            service_started_unix: 1,
        };
        shared
            .runtime
            .lock()
            .unwrap()
            .boards
            .get_mut("b0")
            .unwrap()
            .lifecycle
            .testing()
            .fault(crate::board_lifecycle::Fault::Transport, "lost".into(), 2);
        reconcile(&shared).unwrap();
        let state = shared.runtime.lock().unwrap();
        let routes = state.routing.as_ref().unwrap();
        assert_eq!(routes.len(), 3);
        assert!(routes.iter().all(|a| a.board_id != "b0"));
        assert!(state.boards["b1"].lifecycle.capacity(2, true).accepts_work);
        assert!(state.boards["b2"].lifecycle.capacity(2, true).accepts_work);
        assert_eq!(
            routes
                .iter()
                .filter(|a| a.chain == Chain::Challenge)
                .count(),
            1
        );
    }
}

//! Service generation ownership, listeners and topology replacement.
use crate::{
    Shared, asic, hardware, history_store, inventory, observability, operational, private_control,
    runtime, status_api, unix_now,
};
use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeSet, HashMap},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use timelord_core::Config;
use timelord_supervisor::AcceptBackoff;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinSet,
};
use vdf_client_protocol::{ClientMessage, acknowledge_stop, read_message};

struct Run {
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    tasks: JoinSet<Result<()>>,
    store: Arc<history_store::Store>,
    history_enabled: bool,
}

impl Run {
    async fn stop(mut self) -> Result<()> {
        let _ = self.stop.send(true);
        let mut failure = None;
        while let Some(task) = self.tasks.join_next().await {
            if let Err(error) = task
                .context("Client listener stopped unexpectedly")
                .and_then(|r| r)
            {
                failure = Some(error);
            }
        }
        if self.history_enabled {
            let saved = self.store.capture(
                &self.shared.runtime.lock().expect("runtime"),
                self.shared.started_unix,
                unix_now(),
            );
            let saved = saved.pruned(unix_now());
            let store = self.store.clone();
            if let Err(error) = tokio::task::spawn_blocking(move || store.save(&saved)).await? {
                tracing::warn!(%error, "Final chart history checkpoint could not be saved");
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

async fn start_run(
    config: Config,
    prepared: hardware::Prepared,
    history_path: PathBuf,
    faults: &HashMap<String, runtime::HardwareState>,
    service_started_unix: u64,
    process: Arc<operational::State>,
) -> Result<Run> {
    let hardware::Prepared {
        assignments,
        native,
    } = prepared;
    tracing::info!(plan=?assignments, "Resolved chain assignments for this run");
    let control = config
        .service
        .control_socket
        .as_ref()
        .map(|p| private_control::bind(p))
        .transpose()?;
    let mut listeners = vec![];
    for b in &config.hardware.boards {
        let capacity = assignments.iter().filter(|a| a.board_id == b.id).count() as u8;
        if capacity == 0 {
            continue;
        }
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], b.client_port)))
            .await
            .with_context(|| format!("Binding client listener for {}", b.id))?;
        listeners.push((b.id.clone(), capacity, listener));
    }
    let active = config.node.certificate.is_some() && !assignments.is_empty();
    let tuning_paused = config.service.tuning_paused;
    let started_unix = unix_now();
    let store = Arc::new(history_store::Store::new(
        history_path,
        &config,
        &assignments,
    )?);
    let mut runtime = runtime::RuntimeState::new(tuning_paused);
    // Leave unreadable/corrupt checkpoints untouched and keep proof production
    // available. A bad history file must not silently be replaced with empty data.
    let history_enabled = match store.restore(&mut runtime, started_unix) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "Saved history unavailable; persistence disabled to preserve checkpoint");
            false
        }
    };
    runtime.boards = faults.clone();
    process.metrics.register_boards(&config);
    if assignments.is_empty() {
        runtime.transition(
            "waiting_for_clients",
            "disconnected",
            "Connect an ASIC board to resume.",
        );
    }
    let runtime = Arc::new(Mutex::new(runtime));
    let (backend, offers) =
        asic::AsicBackend::new(&config, &assignments, runtime.clone(), process.clone())?;
    let backend = Arc::new(backend);
    let shared = Arc::new(Shared {
        config,
        native,
        assignments,
        clients: Arc::new(Mutex::new(BTreeSet::new())),
        process,
        runtime,
        started_unix,
        service_started_unix,
    });
    let (shutdown, receiver) = watch::channel(false);
    let mut tasks = JoinSet::new();
    if history_enabled {
        tasks.spawn(history_store::record(
            shared.clone(),
            store.clone(),
            receiver.clone(),
        ));
    }
    for (id, capacity, listener) in listeners {
        tasks.spawn(client_listener(
            listener,
            id.clone(),
            capacity,
            shared.clone(),
            receiver.clone(),
            if active {
                offers.get(&id).cloned()
            } else {
                None
            },
        ));
    }
    if active {
        let shared = shared.clone();
        let backend = backend.clone();
        let shutdown = receiver.clone();
        tasks.spawn(async move {
            runtime::run(shared, backend, shutdown).await;
            Ok(())
        });
    }
    if shared.config.hardware.client_binary.is_some() && active {
        let backend = backend.clone();
        let shared = shared.clone();
        let shutdown = receiver.clone();
        tasks.spawn(async move {
            hardware::run(shared, backend, shutdown)
                .await
                .context("ASIC supervision stopped")
        });
    }
    if let Some(control) = control {
        let shared = shared.clone();
        let shutdown = receiver.clone();
        tasks.spawn(async move { private_control::run(control, shared, shutdown).await });
    }
    Ok(Run {
        shared,
        stop: shutdown,
        tasks,
        store,
        history_enabled,
    })
}

async fn essential_exit(
    tasks: &mut JoinSet<Result<()>>,
    process: &operational::State,
) -> anyhow::Error {
    let task = tasks.join_next().await;
    process.essential_failure();
    tracing::error!(?task, "Essential service task exited unexpectedly");
    anyhow::anyhow!("Essential service task exited unexpectedly: {task:?}")
}

async fn observation_exit(
    server: &mut Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    process: &operational::State,
) {
    let result = server.as_mut().expect("observer").await;
    // Remove the completed handle so it cannot be polled again. A scrape
    // listener failure must not interrupt proof work or topology supervision.
    server.take();
    process.fail();
    tracing::error!(?result, "Observability server exited unexpectedly");
}

fn spawn_http(
    listener: TcpListener,
    app: axum::Router,
    mut stopped: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<std::io::Result<()>> {
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stopped.changed().await;
            })
            .await
    })
}

/// Replace the current run for a changed topology. A replacement that fails
/// validation keeps the current run.
async fn replace(
    run: Run,
    mut config: Config,
    faults: &mut HashMap<String, runtime::HardwareState>,
    history_path: &Path,
    service_started_unix: u64,
    process: &Arc<operational::State>,
    current: &watch::Sender<Arc<Shared>>,
) -> Result<Run> {
    // Validate the replacement before interrupting healthy work.
    let prepared = match hardware::prepare(&config).await {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::warn!(%error,"Topology replacement rejected; retaining current owners");
            return Ok(run);
        }
    };
    let _topology = process.progress.begin(
        timelord_supervisor::progress::Resource::Topology,
        0,
        0,
        timelord_supervisor::progress::Stage::TopologyChange,
    );
    {
        let mut runtime = run.shared.runtime.lock().expect("runtime");
        config.service.tuning_paused = runtime.tuning_paused;
        runtime.transition(
            "reconfiguring",
            "disconnected",
            "Updating engines for connected ASIC boards.",
        );
        runtime.event(None, "topology_change");
        for board in runtime.boards.values_mut() {
            board.raw_speeds.clear();
        }
    }
    process.reconfigure();
    process.metrics.topology_changed();
    tracing::info!(plan=?prepared.assignments, "USB topology changed; replacing chain owners");
    let previous = run.shared.clone();
    run.stop().await?;
    // Collect after shutdown too: verification can finish while tasks drain.
    faults.extend(
        previous
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .iter()
            .filter(|(_, b)| b.latched_fault)
            .map(|(id, b)| {
                let mut fault = b.clone();
                fault.raw_speeds.clear();
                fault.temperature_c = None;
                fault.temperature_unix = 0;
                (id.clone(), fault)
            }),
    );
    let run = start_run(
        config,
        prepared,
        history_path.to_path_buf(),
        faults,
        service_started_unix,
        process.clone(),
    )
    .await?;
    current.send_replace(run.shared.clone());
    process.resume();
    Ok(run)
}

pub(crate) async fn serve(
    config: Config,
    history_path: PathBuf,
    signal: impl std::future::Future<Output = ()>,
) -> Result<()> {
    tokio::pin!(signal);
    let service_started_unix = unix_now();
    // Start reporting before discovery or native initialization can wait.
    let process = operational::State::new(observability::Metrics::new(config.node.network)?);
    let initial = Arc::new(Shared {
        native: None,
        config: config.clone(),
        assignments: vec![],
        clients: Default::default(),
        process: process.clone(),
        runtime: Arc::new(Mutex::new(runtime::RuntimeState::new(
            config.service.tuning_paused,
        ))),
        started_unix: service_started_unix,
        service_started_unix,
    });
    let (current, state) = watch::channel(initial);
    let report_state = state.clone();
    let _reporter = timelord_supervisor::progress::Reporter::with_readiness(
        process.progress.clone(),
        move || observability::ready(&report_state.borrow()),
    )?;
    // The status listener outlives each topology. No UI or external supervisor
    // is required to restart the service after a USB change.
    let mut faults = HashMap::new();
    let startup = async {
        let http = TcpListener::bind(config.service.status_listen)
            .await
            .context("Binding status endpoint")?;
        let observer = if let Some(address) = config.service.observability_listen {
            Some(
                TcpListener::bind(address)
                    .await
                    .with_context(|| format!("Binding observability endpoint {address}"))?,
            )
        } else {
            None
        };
        let (config, prepared) = {
            let _discovery = process.progress.begin(
                timelord_supervisor::progress::Resource::Topology,
                0,
                0,
                timelord_supervisor::progress::Stage::Discovery,
            );
            let config = inventory::connected_configuration(config).await?;
            let prepared = hardware::prepare(&config).await?;
            (config, prepared)
        };
        let run = start_run(
            config,
            prepared,
            history_path.clone(),
            &faults,
            service_started_unix,
            process.clone(),
        )
        .await?;
        Ok::<_, anyhow::Error>((http, observer, run))
    };
    let (http, observer, mut run) = tokio::select! {
        biased;
        _ = &mut signal => return Ok(()),
        result = startup => result?,
    };
    current.send_replace(run.shared.clone());
    let (http_stop, stopped) = watch::channel(false);
    let app = status_api::router(observability::HttpState {
        current: state.clone(),
        process: process.clone(),
    });
    let mut server = spawn_http(http, app, stopped.clone());
    let mut observation_server = observer.map(|listener| {
        let app = observability::routes().with_state(observability::HttpState {
            current: state,
            process: process.clone(),
        });
        spawn_http(listener, app, stopped)
    });
    let mut poll = tokio::time::interval(Duration::from_secs(5));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut topology = inventory::TopologyChange::default();
    let result = loop {
        let candidate = tokio::select! {
            _ = &mut signal => break Ok(()),
            error = essential_exit(&mut run.tasks, &process), if !run.tasks.is_empty() => break Err(error),
            _ = observation_exit(&mut observation_server, &process), if observation_server.is_some() => continue,
            result = &mut server => break result.context("Status task failed")?.context("Status server failed"),
            _ = poll.tick() => {
                tokio::select! {
                    _ = &mut signal => break Ok(()),
                    result = inventory::live_configuration(&run.shared) => result,
                }
            }
        };
        let Some(config) = topology.observe(&run.shared.config, candidate) else {
            continue;
        };
        run = replace(
            run,
            config,
            &mut faults,
            &history_path,
            service_started_unix,
            &process,
            &current,
        )
        .await?;
    };
    process.stop();
    if result.is_err() {
        process.essential_failure();
    }
    run.stop().await?;
    let _ = http_stop.send(true);
    if !server.is_finished() {
        server.await.context("Status task failed")??;
    }
    if let Some(server) = observation_server
        && !server.is_finished()
    {
        server.await.context("Observability task failed")??;
    }
    result
}

pub(crate) async fn client_listener(
    listener: TcpListener,
    id: String,
    capacity: u8,
    shared: Arc<Shared>,
    shutdown: watch::Receiver<bool>,
    offers: Option<tokio::sync::mpsc::Sender<asic::Lease>>,
) -> Result<()> {
    client_listener_with(
        AcceptBackoff::new(),
        listener,
        id,
        capacity,
        shared,
        shutdown,
        offers,
    )
    .await
}

async fn client_listener_with(
    mut backoff: AcceptBackoff,
    listener: TcpListener,
    id: String,
    capacity: u8,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
    offers: Option<tokio::sync::mpsc::Sender<asic::Lease>>,
) -> Result<()> {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            Some(result)=clients.join_next(),if !clients.is_empty()=>{result.context("Client task failed")?;},
            accepted=backoff.accept(listener.accept(), |error, failures, delay| {
                tracing::warn!(%error,failures=failures,?delay,"VDF accept failed; retrying");
            })=>{
                let Some((socket,_))=accepted else { continue; };
                let slot={let mut connected=shared.clients.lock().expect("client state lock");let slot=(0..capacity).find(|slot|!connected.contains(&(id.clone(),*slot)));if let Some(slot)=slot {connected.insert((id.clone(),slot));}slot};
                if let Some(slot)=slot {
                    if let Some(offers) = &offers {
                        let generation = {
                            let mut runtime = shared.runtime.lock().expect("runtime");
                            let hardware = runtime.boards.entry(id.clone()).or_default();
                            // Reusing a slot must never reuse its predecessor's verified readiness.
                            hardware.verified_slots.remove(&slot);
                            hardware.generation
                        };
                        let lease = asic::Lease { socket, board: id.clone(), slot, generation, clients: shared.clients.clone() };
                        if offers.try_send(lease).is_err() { tracing::warn!(board=%id, "ASIC socket queue full"); }
                        continue;
                    }
                    let shared=shared.clone();let id=id.clone();let mut shutdown=shutdown.clone();
                    clients.spawn(async move {
                        tracing::info!(board=%id,slot,"VDF socket connected; configure the full node to begin work");
                        let result=tokio::select! {_=shutdown.changed()=>Ok(()),result=idle_client(socket)=>result};
                        shared.clients.lock().expect("client state lock").remove(&(id.clone(),slot));
                        if let Err(error)=result {tracing::debug!(board=%id,slot,%error,"VDF socket closed");}
                    });
                } else {tracing::warn!(board=%id,"Rejected excess VDF socket");}
            }
        }
    }
    while let Some(result) = clients.join_next().await {
        result.context("Client shutdown failed")?;
    }
    Ok(())
}

async fn idle_client(mut socket: TcpStream) -> Result<()> {
    match read_message(&mut socket).await? {
        ClientMessage::Stop => {
            acknowledge_stop(&mut socket).await?;
            Ok(())
        }
        ClientMessage::Proof(_) => {
            bail!("Unsolicited proof received without an active challenge; not accepted")
        }
    }
}

pub(crate) async fn shutdown_signal() {
    let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    shutdown_signal_from(term).await;
}

pub(crate) async fn shutdown_signal_from(mut term: tokio::signal::unix::Signal) {
    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
}

#[cfg(test)]
mod tests;

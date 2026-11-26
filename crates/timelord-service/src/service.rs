//! Service ownership: status listeners, the node session and persistent
//! per-board native owners. USB changes update routes, never the process.
use crate::{
    Shared, asic, config_checkpoint, hardware, history_store, inventory, observability,
    operational, private_control, runtime, safety, status_api, unix_now,
};
use anyhow::{Context, Result, bail};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use timelord_core::Config;
use timelord_supervisor::AcceptBackoff;
use tokio::io::AsyncReadExt;
use tokio::{net::TcpListener, sync::watch, task::JoinSet};

struct Run {
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    tasks: JoinSet<Result<()>>,
    store: Arc<history_store::Store>,
    history_enabled: bool,
    owner_updates: watch::Sender<Arc<Shared>>,
    node: NodeSession,
}

/// The node session has its own stop signal so a node-profile change can
/// replace it without touching native owners, routes or USB.
struct NodeSession {
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl NodeSession {
    fn start(shared: &Arc<Shared>, backend: &Arc<asic::AsicBackend>) -> Self {
        let (stop, receiver) = watch::channel(false);
        let task = shared.config.node().certificate.is_some().then(|| {
            let shared = shared.clone();
            let backend = backend.clone();
            tokio::spawn(async move { crate::runtime::run(shared, backend, receiver).await })
        });
        Self { stop, task }
    }
    async fn stop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            tracing::warn!(%error, "Node session task failed during shutdown");
        }
    }
}

impl Run {
    async fn stop(mut self) -> Result<()> {
        self.node.stop().await;
        let _ = self.stop.send(true);
        let mut failure = None;
        while let Some(task) = self.tasks.join_next().await {
            if let Err(error) = task
                .context("Service task stopped unexpectedly")
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
            let progress = self.shared.process.progress.clone();
            if let Err(error) = self.store.flush(saved, progress).await {
                tracing::warn!(%error, "Final chart history checkpoint unavailable; shutdown continues");
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Publish a configuration whose hardware section is unchanged in meaning
    /// (new locations, remembered boards) or whose node section changed.
    /// Automatic boards require already-prepared capabilities; discovery
    /// prepares those off the serve loop before calling this.
    fn publish(&mut self, config: Config, current: &watch::Sender<Arc<Shared>>) -> Result<()> {
        self.publish_native(config, self.shared.native.clone(), current)
    }

    fn publish_native(
        &mut self,
        config: Config,
        native: Option<Arc<hardware::Facts>>,
        current: &watch::Sender<Arc<Shared>>,
    ) -> Result<()> {
        if hardware::needs_prepare(&config, native.as_ref()) {
            bail!("Automatic capabilities were not prepared");
        }
        let previous = &self.shared;
        let updated = Arc::new(Shared {
            config,
            native,
            clients: previous.clients.clone(),
            runtime: previous.runtime.clone(),
            process: previous.process.clone(),
            scope: previous.scope,
            started_unix: previous.started_unix,
            service_started_unix: previous.service_started_unix,
        });
        self.owner_updates.send_replace(updated.clone());
        self.shared = updated.clone();
        current.send_replace(updated);
        Ok(())
    }

    /// Install Automatic capabilities on the live configuration. The config
    /// used to probe is never published here, so a node-profile change or a
    /// later inventory cannot be overwritten by a snapshot taken at spawn.
    fn adopt_prepared_native(
        &mut self,
        native: Option<Arc<hardware::Facts>>,
        current: &watch::Sender<Arc<Shared>>,
    ) -> Result<()> {
        let Some(native) = native else {
            bail!("Automatic capabilities were not prepared");
        };
        if self.shared.native.is_some() {
            forgive_prepare_journal(&self.shared);
            return Ok(());
        }
        let previous = &self.shared;
        let updated = Arc::new(Shared {
            config: previous.config.clone(),
            native: Some(native),
            clients: previous.clients.clone(),
            runtime: previous.runtime.clone(),
            process: previous.process.clone(),
            scope: previous.scope,
            started_unix: previous.started_unix,
            service_started_unix: previous.service_started_unix,
        });
        self.owner_updates.send_replace(updated.clone());
        self.shared = updated.clone();
        current.send_replace(updated);
        forgive_prepare_journal(&self.shared);
        Ok(())
    }

    /// Replace only the node session. Native owners, routes, learned tuning and
    /// board health stay in place: no board depends on the selected network.
    async fn replace_node(
        &mut self,
        config: Config,
        current: &watch::Sender<Arc<Shared>>,
    ) -> Result<()> {
        let backend = self
            .shared
            .runtime
            .lock()
            .expect("runtime")
            .prover
            .upgrade()
            .context("Proof backend unavailable")?;
        self.node.stop().await;
        backend.stop_all().await;
        let context = if self.store.retarget(&config)? {
            runtime::NodeContext::New
        } else {
            runtime::NodeContext::Same
        };
        self.shared
            .runtime
            .lock()
            .expect("runtime")
            .reset_node_context(context);
        self.shared
            .process
            .metrics
            .set_network(&config.node().network);
        self.publish(config, current)?;
        self.node = NodeSession::start(&self.shared, &backend);
        Ok(())
    }
}

async fn start_run(
    config: Config,
    native: Option<Arc<hardware::Facts>>,
    history_path: PathBuf,
    safety: Arc<safety::Store>,
    service_started_unix: u64,
    process: Arc<operational::State>,
    node_requests: tokio::sync::mpsc::Sender<private_control::NodeReload>,
) -> Result<Run> {
    let control = config
        .service
        .control_socket
        .as_ref()
        .map(|p| private_control::bind(p))
        .transpose()?;
    let tuning_paused = config.service.tuning_paused;
    let started_unix = unix_now();
    let store = Arc::new(history_store::Store::new(history_path, &config)?);
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
    hardware::restore_lifecycles(&config, &mut runtime.boards, &safety);
    if let Some(reset) = safety.journal_reset() {
        process.metrics.safety_journal_reset(reset);
    }
    if native.is_some()
        && let Err(error) = safety.forgive_prepare_state()
    {
        tracing::warn!(
            error = %format!("{error:#}"),
            "Could not clear stale Automatic prepare journal entries"
        );
    }
    runtime.safety = Some(safety);
    process.metrics.register_boards(&config);
    runtime.transition(
        "waiting_for_clients",
        "waiting_for_hardware",
        if config.hardware.boards.is_empty() {
            "Connect an ASIC board to resume."
        } else {
            "Starting ASIC boards before connecting to the full node."
        },
    );
    let runtime = Arc::new(Mutex::new(runtime));
    let backend = Arc::new(asic::AsicBackend::new(runtime.clone(), process.clone()));
    let shared = Arc::new(Shared::new(
        native,
        config,
        runtime,
        process,
        started_unix,
        service_started_unix,
    ));
    shared.runtime.lock().expect("runtime").prover = Arc::downgrade(&backend);
    let (shutdown, receiver) = watch::channel(false);
    let mut tasks = JoinSet::new();
    if history_enabled {
        tasks.spawn(history_store::record(
            shared.clone(),
            store.clone(),
            receiver.clone(),
        ));
    }
    let node = NodeSession::start(&shared, &backend);
    let (owner_updates, receiver_updates) = watch::channel(shared.clone());
    tasks.spawn(hardware::continuous::run(
        shared.clone(),
        backend,
        receiver_updates,
        receiver.clone(),
    ));
    if let Some(control) = control {
        let shared = shared.clone();
        let shutdown = receiver.clone();
        tasks.spawn(
            async move { private_control::run(control, shared, shutdown, node_requests).await },
        );
    }
    Ok(Run {
        shared,
        stop: shutdown,
        tasks,
        store,
        history_enabled,
        owner_updates,
        node,
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
        axum::serve(timelord_supervisor::http::Listener::new(listener), app)
            .with_graceful_shutdown(async move {
                let _ = stopped.changed().await;
            })
            .await
    })
}

/// After a failed capability probe for discovered Automatic boards, wait
/// before probing again so a persistent failure cannot occupy every
/// inventory cycle.
pub(crate) const DISCOVERY_PREPARE_RETRY: Duration = Duration::from_secs(30);

type DiscoveryPrepare = tokio::task::JoinHandle<(
    Option<std::path::PathBuf>,
    Result<Option<Arc<hardware::Facts>>>,
)>;

fn forgive_prepare_journal(shared: &Shared) {
    let Some(store) = shared
        .runtime
        .lock()
        .expect("runtime")
        .safety
        .as_ref()
        .cloned()
    else {
        return;
    };
    if let Err(error) = store.forgive_prepare_state() {
        tracing::warn!(
            error = %format!("{error:#}"),
            "Could not clear stale Automatic prepare journal entries"
        );
    }
}

fn spawn_discovery_prepare(
    config: Config,
    current: Option<Arc<hardware::Facts>>,
) -> DiscoveryPrepare {
    tokio::spawn(async move {
        let binary = config.hardware.client_binary.clone();
        let result = hardware::prepared_native(&config, current).await;
        (binary, result)
    })
}

pub(crate) async fn serve(
    config: Config,
    history_path: PathBuf,
    config_path: PathBuf,
    fallback: bool,
    signal: impl std::future::Future<Output = ()>,
) -> Result<()> {
    tokio::pin!(signal);
    let requested_config = config.clone();
    let service_started_unix = unix_now();
    let process =
        operational::State::new(observability::Metrics::new(config.node().network.clone())?);
    process.progress.configuration(fallback);
    // Start reporting before discovery or native initialization can wait.
    let initial = Arc::new(Shared::new(
        None,
        config.clone(),
        Arc::new(Mutex::new(runtime::RuntimeState::new(
            config.service.tuning_paused,
        ))),
        process.clone(),
        service_started_unix,
        service_started_unix,
    ));
    let (current, state) = watch::channel(initial);
    let report_state = state.clone();
    let _reporter = timelord_supervisor::progress::Reporter::with_readiness(
        process.progress.clone(),
        move || observability::ready(&report_state.borrow()),
    )?;
    let (node_requests, mut node_reloads) = tokio::sync::mpsc::channel(4);
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
        let safety =
            Arc::new(safety::Store::open(&config_path)?.monitored(process.progress.clone()));
        let (config, native) = {
            let _discovery = process.progress.begin(
                timelord_supervisor::progress::Resource::Topology,
                0,
                0,
                timelord_supervisor::progress::Stage::Discovery,
            );
            let config = hardware::continuous::known_configuration(config);
            let native = hardware::prepare(&config).await?;
            (config, native)
        };
        let run = start_run(
            config,
            native,
            history_path.clone(),
            safety,
            service_started_unix,
            process.clone(),
            node_requests.clone(),
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
    if let Ok(paths) = timelord_supervisor::Paths::for_config(&config_path) {
        process
            .board_history
            .attach(paths.directory.join("board-history.json"));
    }
    let app = status_api::router(observability::HttpState {
        current: state.clone(),
        process: process.clone(),
    });
    let (http_stop, stopped) = watch::channel(false);
    let mut checkpoint = Some(tokio::spawn(config_checkpoint::record(
        requested_config,
        config_path.clone(),
        state.clone(),
        stopped.clone(),
    )));
    let mut server = spawn_http(http, app, stopped.clone());
    let mut observation_server = observer.map(|listener| {
        let app = observability::routes().with_state(observability::HttpState {
            current: state,
            process: process.clone(),
        });
        spawn_http(
            listener,
            timelord_supervisor::http::protect(app, 16),
            stopped,
        )
    });
    let mut poll = tokio::time::interval(Duration::from_secs(5));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut inventory_task: Option<tokio::task::JoinHandle<Result<Config>>> = None;
    let mut prepare_task: Option<DiscoveryPrepare> = None;
    let mut prepare_retry_at: Option<tokio::time::Instant> = None;
    let result = loop {
        let candidate = tokio::select! {
            _ = &mut signal => break Ok(()),
            result=async {checkpoint.as_mut().expect("checkpoint").await},if checkpoint.is_some()=>{
                match result {Ok(Ok(()))=>{},Ok(Err(error))=>tracing::warn!(%error,"Configuration checkpoint stopped"),Err(error)=>tracing::warn!(%error,"Configuration checkpoint task failed")}
                checkpoint=None;continue;
            },
            error = essential_exit(&mut run.tasks, &process), if !run.tasks.is_empty() => break Err(error),
            _ = observation_exit(&mut observation_server, &process), if observation_server.is_some() => continue,
            result = &mut server => break result.context("Status task failed")?.context("Status server failed"),
            Some(request) = node_reloads.recv() => {
                let outcome = node_reload(&mut run, &config_path, &current).await;
                match &outcome {
                    Err(error) => tracing::warn!(error=%format!("{error:#}"), "Node profile change rejected; current session retained"),
                    // Promote the new configuration as last-good only once it
                    // proves itself, like a configuration loaded at startup.
                    Ok(Some(requested)) => {
                        if let Some(old) = checkpoint.take() {
                            old.abort();
                        }
                        checkpoint = Some(tokio::spawn(config_checkpoint::record(
                            requested.clone(),
                            config_path.clone(),
                            current.subscribe(),
                            http_stop.subscribe(),
                        )));
                    }
                    Ok(None) => {}
                }
                let _ = request.send(outcome.map(|_| ()).map_err(|error| format!("{error:#}")));
                continue;
            }
            result=async{inventory_task.as_mut().expect("inventory").await}, if inventory_task.is_some()=>{
                inventory_task.take();
                result.context("USB reconciliation task failed")?
            }
            result=async{prepare_task.as_mut().expect("prepare").await}, if prepare_task.is_some()=>{
                prepare_task.take();
                match result.context("Automatic capability task failed")? {
                    (binary, Ok(native)) => {
                        if binary != run.shared.config.hardware.client_binary {
                            tracing::warn!(
                                "Ignoring Automatic capabilities prepared for a superseded native client"
                            );
                        } else if let Err(error) = run.adopt_prepared_native(native, &current) {
                            tracing::warn!(
                                error = %format!("{error:#}"),
                                "Automatic capabilities could not be prepared for discovered boards; clients not started"
                            );
                            prepare_retry_at = Some(
                                tokio::time::Instant::now() + DISCOVERY_PREPARE_RETRY,
                            );
                        } else {
                            prepare_retry_at = None;
                            if inventory_task.is_none() {
                                let shared=run.shared.clone();
                                inventory_task=Some(tokio::spawn(async move{inventory::live_configuration(&shared).await}));
                            }
                        }
                    }
                    (_, Err(error)) => {
                        tracing::warn!(
                            error = %format!("{error:#}"),
                            "Automatic capabilities could not be prepared for discovered boards; clients not started"
                        );
                        prepare_retry_at = Some(
                            tokio::time::Instant::now() + DISCOVERY_PREPARE_RETRY,
                        );
                    }
                }
                continue;
            }
            _ = poll.tick() => {
                if inventory_task.is_none() {
                    let shared=run.shared.clone();
                    inventory_task=Some(tokio::spawn(async move{inventory::live_configuration(&shared).await}));
                }
                continue;
            }
        };
        // Inventory failures are never interpreted as unplugged boards.
        if let Ok(candidate) = candidate {
            let config = hardware::continuous::known_configuration(candidate);
            if serde_json::to_value(&config.hardware.boards)?
                != serde_json::to_value(&run.shared.config.hardware.boards)?
            {
                if hardware::needs_prepare(&config, run.shared.native.as_ref()) {
                    let idle = prepare_task.is_none();
                    let ready = prepare_retry_at
                        .is_none_or(|deadline| tokio::time::Instant::now() >= deadline);
                    if idle && ready {
                        prepare_task =
                            Some(spawn_discovery_prepare(config, run.shared.native.clone()));
                    }
                } else if let Err(error) = run.publish(config, &current) {
                    tracing::warn!(
                        error = %format!("{error:#}"),
                        "Automatic capabilities could not be prepared for discovered boards; clients not started"
                    );
                }
            }
        }
    };
    process.stop();
    if result.is_err() {
        process.essential_failure();
    }
    run.stop().await?;
    if let Some(inventory) = inventory_task {
        // Cancelling the isolated probe drops its kill-on-drop child. Do not
        // wait for a blocked driver enumeration when stopping the service.
        inventory.abort();
        let _ = inventory.await;
    }
    if let Some(prepare) = prepare_task {
        // Cancelling the capability probe drops its kill-on-drop child so a
        // stuck native client cannot delay shutdown.
        prepare.abort();
        let _ = prepare.await;
    }
    let _ = http_stop.send(true);
    if let Some(mut checkpoint) = checkpoint
        && tokio::time::timeout(Duration::from_secs(2), &mut checkpoint)
            .await
            .is_err()
    {
        checkpoint.abort();
    }
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

/// Apply a saved node-profile change without restarting USB owners. Any change
/// outside the node/connection sections still requires a supervised restart.
async fn node_reload(
    run: &mut Run,
    config_path: &std::path::Path,
    current: &watch::Sender<Arc<Shared>>,
) -> Result<Option<Config>> {
    let requested = crate::load(config_path)?;
    // Mirror serve's normalization so only real edits differ.
    let mut saved = requested.clone();
    saved.service.control_socket = Some(crate::automatic::socket_path(config_path, &saved));
    crate::set_default_automatic_state(&mut saved, config_path)?;
    saved.service.tuning_paused = run.shared.config.service.tuning_paused;
    // A legacy app-launch preference with no runtime effect (README); the app
    // also treats it as node-only, so it must not force a board restart.
    saved.service.start_automatically = run.shared.config.service.start_automatically;
    let candidate = hardware::continuous::known_configuration(saved);
    let running = &run.shared.config;
    // USB locations are resolved at runtime and remembered boards are a
    // registry, not operator edits. Every other board setting must match.
    let settings = |config: &Config| -> Result<serde_json::Value> {
        let mut hardware = config.hardware.clone();
        hardware.boards.sort_by(|a, b| a.id.cmp(&b.id));
        for board in &mut hardware.boards {
            board.device_location = None;
        }
        hardware.remembered_boards = vec![];
        Ok(serde_json::to_value((&hardware, &config.service))?)
    };
    anyhow::ensure!(
        settings(&candidate)? == settings(running)?,
        "Board or service settings changed; use Save & Restart"
    );
    crate::network::resolve(candidate.node())?;
    if serde_json::to_value(candidate.node())? == serde_json::to_value(running.node())? {
        return Ok(None);
    }
    tracing::info!(network=%candidate.node().network, host=%candidate.node().host, "Switching full-node profile; native owners retained");
    run.replace_node(candidate, current).await?;
    Ok(Some(requested))
}

/// Accept identified native engine sockets for one board. Each lease names its
/// physical engine; the backend routes work only to currently routed engines.
pub(crate) async fn client_listener(
    listener: TcpListener,
    id: String,
    shared: Arc<Shared>,
    shutdown: watch::Receiver<bool>,
    offers: asic::Offers,
) -> Result<()> {
    client_listener_with(AcceptBackoff::new(), listener, id, shared, shutdown, offers).await
}

async fn client_listener_with(
    mut backoff: AcceptBackoff,
    listener: TcpListener,
    id: String,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
    offers: asic::Offers,
) -> Result<()> {
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            accepted=backoff.accept(listener.accept(), |error, failures, delay| {
                tracing::warn!(%error,failures=failures,?delay,"VDF accept failed; retrying");
            })=>{
                let Some((mut socket,_))=accepted else { continue; };
                let mut identity = [0u8;13];
                if !matches!(tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut identity)).await, Ok(Ok(_)))
                    || &identity[..11] != b"TLENGINE 1 " || identity[12] != b'\n' || !(b'0'..=b'2').contains(&identity[11]) {
                    tracing::warn!(board=%id, "Rejected VDF socket without an engine identity");
                    continue;
                }
                let slot = identity[11] - b'0';
                let claimed = shared.clients.lock().expect("client state lock").insert((id.clone(), slot));
                if !claimed {
                    tracing::warn!(board=%id, slot, "Rejected duplicate VDF engine socket");
                    continue;
                }
                let generation = {
                    let mut runtime = shared.runtime.lock().expect("runtime");
                    let hardware = runtime.boards.entry(id.clone()).or_default();
                    // Reusing a slot must never reuse its predecessor's verified readiness.
                    hardware.lifecycle.forget_slot(slot);
                    hardware.lifecycle.status().generation
                };
                let lease = asic::Lease { socket, board: id.clone(), slot, generation, clients: shared.clients.clone() };
                offers.offer(lease);
            }
        }
    }
    Ok(())
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
mod operations_tests;
#[cfg(test)]
mod tests;

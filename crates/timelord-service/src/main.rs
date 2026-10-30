mod asic;
mod assignment;
mod automatic;
mod board_lifecycle;
mod bounded_lines;
mod config_checkpoint;
mod connection;
mod diagnostics;
mod experiments;
mod hardware;
mod history_store;
mod inventory;
mod node_status;
mod observability;
mod operational;
#[cfg(test)]
mod operations_tests;
mod private_control;
mod runtime;
mod safety;
mod status_api;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::{
    collections::BTreeSet,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use timelord_core::{Assignment, Config};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinSet,
};
use vdf_client_protocol::{ClientMessage, acknowledge_stop, read_message};

#[derive(Parser)]
#[command(
    version,
    about = "Timelord Mac background service and administrator tools"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create an incomplete-setup configuration; never overwrite an existing file.
    Init {
        #[arg(long)]
        config: PathBuf,
    },
    /// Check schema, credential completeness, and board/port assignments.
    Validate {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, hide = true)]
        supervised: bool,
    },
    /// Print the configured chain plan, without touching hardware.
    Plan {
        #[arg(long)]
        config: PathBuf,
    },
    /// Start the coordinator, status API and explicitly configured native clients.
    Serve {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, hide = true)]
        supervised: bool,
    },
    /// Read configuration as JSON for the native interface.
    Configuration {
        #[arg(long)]
        config: PathBuf,
    },
    /// Validate and atomically replace configuration from a JSON file.
    Configure {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        input: PathBuf,
    },
    /// Authenticate a Chia full-node connection without launching clients.
    TestNode {
        #[arg(long)]
        config: PathBuf,
    },
    /// Read full-node sync and peak height over its private, authenticated RPC.
    NodeState {
        #[arg(long)]
        config: PathBuf,
    },
    /// Import a dedicated identity from a connection.toml bundle.
    ImportNode {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        bundle: PathBuf,
    },
    /// Connect to the existing Chia installation in this root folder.
    LocalNode {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        chia_root: PathBuf,
    },
    /// Enumerate USB boards without setting operating points.
    Devices {
        #[arg(long)]
        config: PathBuf,
    },
    /// Internal disposable driver process; never loads D2XX into the service.
    #[command(hide = true)]
    UsbInventory {
        #[arg(long)]
        driver_library: PathBuf,
    },
    /// Run a bounded, isolated three-chain ASIC proof check without a full node.
    CheckHardware {
        #[arg(long)]
        config: PathBuf,
    },
    /// Hold or resume Automatic tuning without pausing proof production.
    Tuning {
        #[arg(long)]
        config: PathBuf,
        #[arg(long,action=clap::ArgAction::Set)]
        paused: bool,
    },
    /// Read the local service's versioned JSON status.
    Status {
        #[arg(long)]
        config: PathBuf,
    },
}

fn load(path: &Path) -> Result<Config> {
    let text = config_checkpoint::bounded_text(path, false)
        .with_context(|| format!("Reading {}", path.display()))?;
    parse_config(path, &text)
}
fn parse_config(path: &Path, text: &str) -> Result<Config> {
    let mut config: Config = toml::from_str(text).context("Parsing configuration")?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    for credential in [
        &mut config.node.certificate,
        &mut config.node.private_key,
        &mut config.node.ca_certificate,
        &mut config.hardware.client_binary,
        &mut config.hardware.driver_library,
        &mut config.hardware.automatic_state,
        &mut config.service.control_socket,
    ] {
        if let Some(relative) = credential
            && relative.is_relative()
        {
            *relative = parent.join(&relative);
        }
    }
    if let Some(rpc) = &mut config.node.rpc {
        for path in [
            &mut rpc.certificate,
            &mut rpc.private_key,
            &mut rpc.ca_certificate,
        ] {
            if path.is_relative() {
                *path = parent.join(&*path);
            }
        }
    }
    config.validate().map_err(anyhow::Error::msg)?;
    Ok(config)
}

fn set_default_automatic_state(config: &mut Config, path: &Path) -> Result<()> {
    if config.hardware.automatic_state.is_none() {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()?;
        config.hardware.automatic_state = Some(parent.join("automatic-profiles.json"));
    }
    Ok(())
}

fn save(path: &Path, config: &Config) -> Result<()> {
    config.validate().map_err(anyhow::Error::msg)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    timelord_supervisor::atomic::write(path, toml::to_string_pretty(config)?.as_bytes())
}

#[tokio::main]
async fn main() -> Result<()> {
    let diagnostic_log = timelord_supervisor::logging::Writer::stderr();
    let tracing_log = diagnostic_log.clone();
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(move || tracing_log.clone())
        .init();
    match Cli::parse().command {
        Command::Init { config } => {
            if let Some(parent) = config.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            use std::io::Write;
            options
                .open(&config)?
                .write_all(toml::to_string_pretty(&Config::default())?.as_bytes())?;
            println!("Created {}", config.display());
        }
        Command::Configuration { config } => {
            println!("{}", serde_json::to_string(&load(&config)?)?)
        }
        Command::Configure { config, input } => {
            let updated: Config = serde_json::from_slice(&std::fs::read(input)?)?;
            save(&config, &updated)?;
            println!(
                "{}",
                serde_json::json!({"saved":true,"restart_required":true})
            );
        }
        Command::TestNode { config } => {
            let config = load(&config)?;
            if TcpStream::connect(config.service.status_listen)
                .await
                .is_ok()
            {
                bail!(
                    "Pause Timelord before testing a connection; a duplicate peer identity can interrupt its node session"
                );
            }
            let mut socket = runtime::connect(&config).await?;
            socket.close(None).await?;
            println!(
                "{}",
                serde_json::json!({"authenticated":true,"message":"Authenticated full node on the selected network. Chain readiness is checked when Timelord starts."})
            );
        }
        Command::NodeState { config } => {
            let config = load(&config)?;
            let rpc = config
                .node
                .rpc
                .as_ref()
                .context("Full-node status RPC is not configured")?;
            let status = tokio::time::timeout(Duration::from_secs(5), node_status::fetch(rpc))
                .await
                .context("Full-node status timed out")??;
            println!("{}", serde_json::to_string(&status)?);
        }
        Command::ImportNode { config, bundle } => {
            connection::import(&config, &bundle)?;
            println!("{}", serde_json::json!({"saved":true}));
        }
        Command::LocalNode { config, chia_root } => {
            let mut updated = load(&config)?;
            connection::local(&mut updated, &chia_root)?;
            save(&config, &updated)?;
            println!("{}", serde_json::json!({"saved":true}));
        }
        Command::Devices { config } => println!(
            "{}",
            serde_json::to_string(&inventory::discover(&load(&config)?).await?)?
        ),
        Command::UsbInventory { driver_library } => {
            println!(
                "{}",
                serde_json::to_string(&inventory::native(&driver_library)?)?
            );
        }
        Command::CheckHardware { config } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&diagnostics::check(load(&config)?).await?)?
            );
        }
        Command::Validate { config, supervised } => {
            if supervised {
                timelord_supervisor::watch_parent()?;
            }
            load(&config)?;
            println!(
                "Configuration valid. This does not establish node authentication or hardware readiness."
            );
        }
        Command::Plan { config } => {
            let mut value = load(&config)?;
            set_default_automatic_state(&mut value, &config)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&assignment::resolve(&value)?)?
            );
        }
        Command::Serve { config, supervised } => {
            // Install the async termination handler before the lifetime thread
            // can signal us, including while configuration/USB startup awaits.
            let termination =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            if supervised {
                timelord_supervisor::watch_parent()?;
            }
            let paths = timelord_supervisor::Paths::for_config(&config)?;
            let _owner = timelord_supervisor::InstanceLock::acquire(&paths.worker_lock)?;
            let (mut value, fallback) = config_checkpoint::load_running(&config)?;
            value.service.control_socket = Some(automatic::socket_path(&config, &value));
            set_default_automatic_state(&mut value, &config)?;
            serve(
                value,
                config.with_extension("history.json"),
                config,
                fallback,
                shutdown_signal_from(termination),
            )
            .await?;
        }
        Command::Tuning { config, paused } => {
            let value = config_checkpoint::load_running(&config)?.0;
            let result =
                private_control::request(&automatic::socket_path(&config, &value), paused).await?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Command::Status { config } => {
            let address = config_checkpoint::load_running(&config)?
                .0
                .service
                .status_listen;
            let response = tokio::time::timeout(Duration::from_secs(3), async {
                let mut socket = TcpStream::connect(address).await?;
                socket
                    .write_all(
                        b"GET /v1/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .await?;
                let mut response = String::new();
                socket
                    .take(16 * 1024 * 1024)
                    .read_to_string(&mut response)
                    .await?;
                Ok::<_, std::io::Error>(response)
            })
            .await
            .context("Status request timed out")??;
            let (header, body) = response
                .split_once("\r\n\r\n")
                .context("Invalid status response")?;
            if !header.starts_with("HTTP/1.1 200") {
                bail!("Service rejected status request");
            }
            let value: serde_json::Value = serde_json::from_str(body)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
    }
    Ok(())
}

struct Shared {
    config: Config,
    assignments: Vec<Assignment>,
    // These are socket slots, not authenticated board/engine identities.
    clients: Arc<Mutex<BTreeSet<(String, u8)>>>,
    runtime: Arc<Mutex<runtime::RuntimeState>>,
    started_unix: u64,
    service_started_unix: u64,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

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
            let saved = self.store.snapshot(
                &self.shared.runtime.lock().expect("runtime"),
                self.shared.started_unix,
                unix_now(),
            );
            let store = self.store.clone();
            if let Err(error) = tokio::task::spawn_blocking(move || store.save(&saved)).await? {
                tracing::warn!(%error, "Final chart history checkpoint could not be saved");
            }
        }
        let runtime = self.shared.runtime.lock().expect("runtime");
        if let Some(metrics) = &runtime.metrics {
            metrics.disconnect(runtime.metric_epoch);
        }
        failure.map_or(Ok(()), Err)
    }
}

async fn start_run(
    config: Config,
    history_path: PathBuf,
    board_states: &std::collections::HashMap<String, runtime::HardwareState>,
    service_started_unix: u64,
    metrics: Arc<observability::Metrics>,
    process: Arc<operational::State>,
    safety: Option<Arc<safety::Store>>,
) -> Result<Run> {
    let unavailable = hardware::unavailable_for(&config, board_states, safety.as_deref());
    let assignments = assignment::resolve_usable(&config, &unavailable)?;
    hardware::validate_automatic(&config, &assignments).await?;
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
    let mut runtime = runtime::RuntimeState::with_tuning_paused(tuning_paused);
    // Leave unreadable/corrupt checkpoints untouched and keep proof production
    // available. A bad history file must not silently be replaced with empty data.
    let history_enabled = match store.restore(&mut runtime, started_unix) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "Saved history unavailable; persistence disabled to preserve checkpoint");
            false
        }
    };
    runtime.boards = board_states.clone();
    for board in &config.hardware.boards {
        runtime
            .boards
            .entry(board.id.clone())
            .or_default()
            .lifecycle
            .bind(board.serial.as_deref());
    }
    if let Some(store) = &safety {
        for board in &config.hardware.boards {
            if let Some(saved) = board
                .serial
                .as_deref()
                .and_then(|serial| store.board(serial))
                && let Some(reason) = saved.quarantine
            {
                let hardware = runtime.boards.entry(board.id.clone()).or_default();
                hardware.lifecycle.quarantined(
                    reason.into(),
                    format!("Board admission required after {reason:?}"),
                    unix_now(),
                );
            }
        }
    }
    runtime.safety = safety;
    runtime.process = process;
    runtime.metric_epoch = metrics.begin_session();
    metrics.register_boards(&config);
    runtime.metrics = Some(metrics);
    if assignments.is_empty() {
        runtime.transition(
            "waiting_for_clients",
            "disconnected",
            "Connect an ASIC board to resume.",
        );
    }
    let runtime = Arc::new(Mutex::new(runtime));
    let (backend, offers) = asic::AsicBackend::new(&config, &assignments, runtime.clone())?;
    let backend = Arc::new(backend);
    let shared = Arc::new(Shared {
        config,
        assignments,
        clients: Arc::new(Mutex::new(BTreeSet::new())),
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
    if shared.config.hardware.client_binary.is_some() {
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

async fn serve(
    config: Config,
    history_path: PathBuf,
    config_path: PathBuf,
    fallback: bool,
    signal: impl std::future::Future<Output = ()>,
) -> Result<()> {
    tokio::pin!(signal);
    let requested_config = config.clone();
    let service_started_unix = unix_now();
    let process = Arc::new(operational::State::default());
    process.progress.configuration(fallback);
    // Start reporting before discovery or native initialization can wait.
    let metrics = observability::Metrics::new(config.node.network)?;
    let initial = Arc::new(Shared {
        config: config.clone(),
        assignments: vec![],
        clients: Default::default(),
        runtime: Arc::new(Mutex::new(runtime::RuntimeState {
            metrics: Some(metrics.clone()),
            process: process.clone(),
            ..Default::default()
        })),
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
    let mut board_states = std::collections::HashMap::new();
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
        let safety = if config.hardware.client_binary.is_some() {
            Some(Arc::new(
                safety::Store::open(&config_path)?.monitored(process.progress.clone()),
            ))
        } else {
            None
        };
        let config = {
            let _discovery = process.progress.begin(
                timelord_supervisor::progress::Resource::Topology,
                0,
                0,
                timelord_supervisor::progress::Stage::Discovery,
            );
            inventory::connected_configuration(config).await?
        };
        let run = start_run(
            config,
            history_path.clone(),
            &board_states,
            service_started_unix,
            metrics.clone(),
            process.clone(),
            safety.clone(),
        )
        .await?;
        Ok::<_, anyhow::Error>((http, observer, safety, run))
    };
    let (http, observer, safety, mut run) = tokio::select! {
        biased;
        _ = &mut signal => return Ok(()),
        result = startup => result?,
    };
    current.send_replace(run.shared.clone());
    let app = status_api::router(observability::HttpState {
        current: state.clone(),
        metrics: metrics.clone(),
        process: process.clone(),
    });
    let (http_stop, mut stopped) = watch::channel(false);
    let mut checkpoint = Some(tokio::spawn(config_checkpoint::record(
        requested_config,
        config_path,
        state.clone(),
        stopped.clone(),
    )));
    let observation_stopped = stopped.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(
            timelord_supervisor::http::Listener::new(http),
            timelord_supervisor::http::protect(app),
        )
        .with_graceful_shutdown(async move {
            let _ = stopped.changed().await;
        })
        .await
    });
    let mut observation_server = observer.map(|listener| {
        let mut stopped = observation_stopped.clone();
        let app = observability::routes().with_state(observability::HttpState {
            current: state,
            metrics: metrics.clone(),
            process: process.clone(),
        });
        tokio::spawn(async move {
            axum::serve(
                timelord_supervisor::http::Listener::new(listener),
                timelord_supervisor::http::protect(app),
            )
            .with_graceful_shutdown(async move {
                let _ = stopped.changed().await;
            })
            .await
        })
    });
    let mut poll = tokio::time::interval(Duration::from_secs(5));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut topology = inventory::TopologyChange::default();
    let mut capacity = assignment::CapacityChange::default();
    let mut inventory_task: Option<tokio::task::JoinHandle<Result<Config>>> = None;
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
            result=async{inventory_task.as_mut().expect("inventory").await}, if inventory_task.is_some()=>{
                inventory_task.take();
                result.context("USB reconciliation task failed")?
            }
            _ = poll.tick() => {
                if inventory_task.is_none() {
                    let shared=run.shared.clone();
                    inventory_task=Some(tokio::spawn(async move{inventory::live_configuration(&shared).await}));

                }
                continue;
            }
        };
        let next = topology.observe(&run.shared.config, candidate);
        let unavailable = hardware::unavailable(&run.shared);
        let capacity_changed =
            capacity.observe(&run.shared.assignments, &run.shared.config, &unavailable);
        if next.is_none() && !capacity_changed {
            continue;
        }
        let mut config = next.unwrap_or_else(|| run.shared.config.clone());
        let _topology = process.progress.begin(
            timelord_supervisor::progress::Resource::Topology,
            0,
            0,
            timelord_supervisor::progress::Stage::TopologyChange,
        );
        // Validate the replacement before interrupting healthy work.
        let plan = assignment::resolve_usable(&config, &unavailable)?;
        if let Err(error) = hardware::validate_automatic(&config, &plan).await {
            tracing::warn!(%error, "Topology replacement rejected; retaining current owners");
            continue;
        }
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
        metrics.topology_changed();
        tracing::info!(plan=?plan, "USB topology changed; replacing chain owners");
        let previous = run.shared.clone();
        run.stop().await?;
        // Collect after shutdown too: verification can finish while tasks drain.
        board_states = previous.runtime.lock().expect("runtime").boards.clone();
        run = start_run(
            config,
            history_path.clone(),
            &board_states,
            service_started_unix,
            metrics.clone(),
            process.clone(),
            safety.clone(),
        )
        .await?;
        current.send_replace(run.shared.clone());
        process.resume();
    };
    process.stop();
    if result.is_err() {
        process.essential_failure();
    }
    run.stop().await?;
    if let Some(inventory) = inventory_task {
        let _ = inventory.await;
    }
    let _ = http_stop.send(true);
    if let Some(checkpoint) = checkpoint {
        let _ = checkpoint.await;
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

async fn client_listener(
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
            accepted=listener.accept()=>{
                let (socket,_)=accepted?;
                let slot={let mut connected=shared.clients.lock().expect("client state lock");let slot=(0..capacity).find(|slot|!connected.contains(&(id.clone(),*slot)));if let Some(slot)=slot {connected.insert((id.clone(),slot));}slot};
                if let Some(slot)=slot {
                    if let Some(offers) = &offers {
                        let generation = {
                            let mut runtime = shared.runtime.lock().expect("runtime");
                            let hardware = runtime.boards.entry(id.clone()).or_default();
                            // Reusing a slot must never reuse its predecessor's verified readiness.
                            hardware.lifecycle.forget_slot(slot);
                            hardware.lifecycle.status().generation
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

async fn shutdown_signal() {
    let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    shutdown_signal_from(term).await;
}

async fn shutdown_signal_from(mut term: tokio::signal::unix::Signal) {
    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
}

#[cfg(test)]
mod tests {
    use super::*;
    use timelord_core::Network;
    #[test]
    fn admitted_board_remains_unavailable_across_reconfiguration_until_fresh_inventory() {
        let board: timelord_core::BoardConfig =
            toml::from_str("id='a'\nname='A'\nserial='A'\nclient_port=18110").unwrap();
        let mut config = Config::default();
        config.hardware.boards.push(board);
        let hardware = runtime::HardwareState {
            lifecycle: {
                let mut owner = crate::board_lifecycle::BoardLifecycle::default();
                owner.admitted(1);
                owner
            },

            ..Default::default()
        };
        let mut hardware = hardware.clone(); // The entire drained state transfers to a replacement run.
        let mut boards = std::collections::HashMap::from([("a".into(), hardware.clone())]);
        assert!(hardware::unavailable_for(&config, &boards, None).contains("a"));
        hardware
            .lifecycle
            .inventory(Some(1), crate::board_lifecycle::Inventory::Present);
        boards.insert("a".into(), hardware);
        assert!(!hardware::unavailable_for(&config, &boards, None).contains("a"));
    }

    #[tokio::test]
    async fn requested_shutdown_prevents_startup_from_binding_or_opening_hardware() {
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config::default();
        config.service.status_listen = occupied.local_addr().unwrap();
        serve(config, PathBuf::new(), PathBuf::new(), false, async {})
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn startup_sigterm_is_retained_until_shutdown_is_polled() {
        let term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        assert_eq!(
            unsafe { libc::kill(std::process::id() as i32, libc::SIGTERM) },
            0
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
        tokio::time::timeout(Duration::from_secs(1), shutdown_signal_from(term))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn topology_generations_release_owners_keep_status_and_preserve_faults() {
        let metrics = observability::Metrics::new(Network::Mainnet).unwrap();
        let process = Arc::new(operational::State::default());
        metrics.topology_changed();
        // All ports and files belong to this fixture; no node, native binary or USB.
        let dir = std::env::temp_dir().join(format!(
            "topology-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = Config::default();
        config.service.control_socket = Some(dir.join("control.sock"));
        let mut reserved = vec![];
        for i in 0..3 {
            let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            config.hardware.boards.push(toml::from_str(&format!("id='b{i}'\nname='B{i}'\nserial='s{i}'\nprofile='manual'\ndevice_location={}\nclient_port={}", i+1, port.local_addr().unwrap().port())).unwrap());
            reserved.push(port);
        }
        let mut board_states = std::collections::HashMap::new();
        board_states.insert(
            "b2".into(),
            runtime::HardwareState {
                lifecycle: crate::board_lifecycle::BoardLifecycle::fixture(
                    crate::board_lifecycle::Phase::ThermalStop,
                    false,
                    Some(crate::board_lifecycle::Fault::Thermal),
                    None,
                    0,
                ),

                ..Default::default()
            },
        );
        // Fixture restrictions are attached to the same physical serial as production snapshots.
        let mut preserved = board_states.remove("b2").unwrap();
        preserved.lifecycle.bind(Some("s2"));
        preserved.lifecycle.fault(
            crate::board_lifecycle::Fault::Thermal,
            "fixture".into(),
            unix_now(),
        );
        board_states.insert("b2".into(), preserved);
        drop(reserved);
        let mut run = start_run(
            config,
            dir.join("history.json"),
            &board_states,
            unix_now() - 3600,
            metrics.clone(),
            process.clone(),
            None,
        )
        .await
        .unwrap();
        let service_started_unix = run.shared.service_started_unix;
        let (current, state) = watch::channel(run.shared.clone());
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = http.local_addr().unwrap();
        let http_metrics = metrics.clone();
        let http_process = process.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                http,
                status_api::router(observability::HttpState {
                    current: state,
                    metrics: http_metrics,
                    process: http_process,
                }),
            )
            .await
            .unwrap();
        });
        for indices in [vec![0], vec![], vec![0, 1], vec![0, 1, 2]] {
            let mut socket = if let Some(board) = run.shared.config.hardware.boards.first() {
                let socket = TcpStream::connect(("127.0.0.1", board.client_port))
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(2), async {
                    while run.shared.clients.lock().unwrap().is_empty() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                Some(socket)
            } else {
                None
            };
            let devices = indices
                .iter()
                .map(|i| inventory::Device {
                    name: "Chia VDF A".into(),
                    location: i + 1,
                    serial: Some(format!("s{i}")),
                })
                .collect();
            let next = inventory::connected_plan(run.shared.config.clone(), devices).unwrap();
            run.stop().await.unwrap();
            if let Some(ref mut socket) = socket {
                let mut byte = [0];
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
                        .await
                        .unwrap()
                        .unwrap(),
                    0
                );
            }
            assert!(!dir.join("control.sock").exists());
            run = start_run(
                next,
                dir.join("history.json"),
                &board_states,
                service_started_unix,
                metrics.clone(),
                process.clone(),
                None,
            )
            .await
            .unwrap();
            current.send_replace(run.shared.clone());
            assert_eq!(metrics.counter("topology_changes_total", &[]), 1);
            assert!(
                run.shared.runtime.lock().unwrap().boards["b2"]
                    .lifecycle
                    .status()
                    .latched_fault
            );
            assert_eq!(
                run.shared.assignments.len(),
                if indices.is_empty() { 0 } else { 3 }
            );
            let mut status_socket = TcpStream::connect(address).await.unwrap();
            status_socket
                .write_all(
                    b"GET /v1/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let mut response = String::new();
            status_socket.read_to_string(&mut response).await.unwrap();
            let (_, body) = response.split_once("\r\n\r\n").unwrap();
            let value: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(value["boards"].as_array().unwrap().len(), indices.len());
            assert_eq!(value["connected_client_count"], 0);
            assert_eq!(value["proofs_delivered"], 0);
            assert_eq!(value["service_started_unix"], service_started_unix);
            assert!(value["started_unix"].as_u64().unwrap() >= service_started_unix + 3600);
        }
        run.stop().await.unwrap();
        server.abort();
        let _ = server.await;
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn unexpected_supervisor_exit_is_latched_as_unhealthy() {
        let metrics = observability::Metrics::new(Network::Mainnet).unwrap();
        let process = Arc::new(operational::State::default());
        let mut tasks = JoinSet::new();
        tasks.spawn(async { Ok(()) });
        essential_exit(&mut tasks, &process).await;
        assert!(!process.snapshot().healthy());
        // Creating a replacement runtime cannot clear the process health latch.
        metrics.begin_session();
        assert!(!process.snapshot().healthy());
    }
    #[tokio::test]
    async fn observability_failures_leave_client_service_running() {
        for failure in ["io", "panic", "early_success"] {
            let metrics = observability::Metrics::new(Network::Mainnet).unwrap();
            let process = Arc::new(operational::State::default());
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let shared = Arc::new(Shared {
                config: Config::default(),
                assignments: vec![],
                clients: Arc::new(Mutex::new(BTreeSet::new())),
                runtime: Arc::new(Mutex::new(runtime::RuntimeState {
                    process: process.clone(),
                    ..Default::default()
                })),
                started_unix: unix_now(),
                service_started_unix: unix_now(),
            });
            let (stop, rx) = watch::channel(false);
            let mut client =
                tokio::spawn(client_listener(listener, "a".into(), 1, shared, rx, None));
            let mut observer = Some(tokio::spawn(async move {
                match failure {
                    "io" => Err(std::io::Error::other("fixture listener failure")),
                    "panic" => panic!("fixture listener panic"),
                    _ => Ok(()),
                }
            }));
            tokio::select! {
                _ = observation_exit(&mut observer, &process) => {},
                result = &mut client => panic!("Client service stopped: {result:?}"),
            }
            assert!(observer.is_none());
            assert!(!process.snapshot().healthy());
            metrics.begin_session();
            assert!(!process.snapshot().healthy());
            // The independent client listener still handles protocol traffic.
            let mut socket = TcpStream::connect(address).await.unwrap();
            socket.write_all(b"STOP").await.unwrap();
            let mut ack = [0; 3];
            tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut ack))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&ack, b"ACK");
            stop.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(2), client)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }
    #[tokio::test]
    async fn idle_clients_stop_cleanly_and_release_their_slot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shared = Arc::new(Shared {
            config: Config::default(),
            assignments: vec![],
            clients: Arc::new(Mutex::new(BTreeSet::new())),
            runtime: Default::default(),
            started_unix: unix_now(),
            service_started_unix: crate::unix_now(),
        });
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(client_listener(
            listener,
            "a".into(),
            1,
            shared.clone(),
            rx,
            None,
        ));
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket.write_all(b"STOP").await.unwrap();
        let mut ack = [0; 3];
        tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut ack))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&ack, b"ACK");
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(shared.clients.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod configuration_tests {
    use super::*;
    #[test]
    fn invalid_edit_preserves_last_good_config_and_relative_paths_resolve_from_its_folder() {
        let folder =
            std::env::temp_dir().join(format!("timelord-config-fixture-{}", std::process::id()));
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("config.toml");
        let mut valid = Config::default();
        valid.node.certificate = Some("identity/client.crt".into());
        valid.node.private_key = Some("identity/client.key".into());
        valid.node.ca_certificate = Some("identity/ca.crt".into());
        save(&path, &valid).unwrap();
        let original = std::fs::read(&path).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.node.private_key,
            Some(folder.canonicalize().unwrap().join("identity/client.key"))
        );
        valid.node.private_key = None;
        assert!(save(&path, &valid).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(folder).unwrap();
    }
}

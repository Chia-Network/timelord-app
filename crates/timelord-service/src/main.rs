mod asic;
mod assignment;
mod automatic;
mod connection;
mod diagnostics;
mod hardware;
mod history_buffer;
mod history_store;
mod inventory;
mod node_status;
mod private_control;
mod runtime;
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
    let text =
        std::fs::read_to_string(path).with_context(|| format!("Reading {}", path.display()))?;
    let mut config: Config = toml::from_str(&text).context("Parsing configuration")?;
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

fn save(path: &Path, config: &Config) -> Result<()> {
    config.validate().map_err(anyhow::Error::msg)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".timelord-config-{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        use std::io::Write;
        let mut file = options.open(&temporary)?;
        file.write_all(toml::to_string_pretty(config)?.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
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
        Command::Validate { config } => {
            load(&config)?;
            println!(
                "Configuration valid. This does not establish node authentication or hardware readiness."
            );
        }
        Command::Plan { config } => {
            let mut value = load(&config)?;
            value.hardware.automatic_state.get_or_insert(
                config
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join("automatic-profiles.json"),
            );
            println!(
                "{}",
                serde_json::to_string_pretty(&assignment::resolve(&value)?)?
            );
        }
        Command::Serve { config } => {
            let mut value = load(&config)?;
            value.service.control_socket = Some(automatic::socket_path(&config, &value));
            value.hardware.automatic_state.get_or_insert(
                config
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join("automatic-profiles.json"),
            );
            serve(value, config.with_extension("history.json")).await?;
        }
        Command::Tuning { config, paused } => {
            let value = load(&config)?;
            let result =
                private_control::request(&automatic::socket_path(&config, &value), paused).await?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Command::Status { config } => {
            let address = load(&config)?.service.status_listen;
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
    native: Option<Arc<hardware::Facts>>,
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
    faults: &std::collections::HashMap<String, runtime::HardwareState>,
    service_started_unix: u64,
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
    runtime.boards = faults.clone();
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
        native,
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
    if shared.config.hardware.client_binary.is_some() && active {
        let backend = backend.clone();
        let shared = shared.clone();
        let shutdown = receiver.clone();
        tasks.spawn(async move {
            if let Err(error) = hardware::run(shared, backend, shutdown).await {
                tracing::warn!(%error, "ASIC supervision stopped");
            }
            Ok(())
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

async fn serve(config: Config, history_path: PathBuf) -> Result<()> {
    let service_started_unix = unix_now();
    // The status listener outlives each topology. No UI or external supervisor
    // is required to restart the service after a USB change.
    let http = TcpListener::bind(config.service.status_listen)
        .await
        .context("Binding status endpoint")?;
    let config = inventory::connected_configuration(config).await?;
    let mut faults = std::collections::HashMap::new();
    let prepared = hardware::prepare(&config).await?;
    let mut run = start_run(
        config,
        prepared,
        history_path.clone(),
        &faults,
        service_started_unix,
    )
    .await?;
    let (current, state) = watch::channel(run.shared.clone());
    let app = status_api::router(state);
    let (http_stop, mut stopped) = watch::channel(false);
    let mut server = tokio::spawn(async move {
        axum::serve(http, app)
            .with_graceful_shutdown(async move {
                let _ = stopped.changed().await;
            })
            .await
    });
    let signal = shutdown_signal();
    tokio::pin!(signal);
    let mut poll = tokio::time::interval(Duration::from_secs(5));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut topology = inventory::TopologyChange::default();
    let result = loop {
        let candidate = tokio::select! {
            _ = &mut signal => break Ok(()),
            result = &mut server => break result.context("Status task failed")?.context("Status server failed"),
            _ = poll.tick() => {
                tokio::select! {
                    _ = &mut signal => break Ok(()),
                    result = inventory::live_configuration(&run.shared) => result,
                }
            }
        };
        let Some(mut config) = topology.observe(&run.shared.config, candidate) else {
            continue;
        };
        // Validate the replacement before interrupting healthy work.
        let prepared = match hardware::prepare(&config).await {
            Ok(prepared) => prepared,
            Err(error) => {
                tracing::warn!(%error, "Topology replacement rejected; retaining current owners");
                continue;
            }
        };
        let plan = &prepared.assignments;
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
        tracing::info!(plan=?plan, "USB topology changed; replacing chain owners");
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
        run = start_run(
            config,
            prepared,
            history_path.clone(),
            &faults,
            service_started_unix,
        )
        .await?;
        current.send_replace(run.shared.clone());
    };
    run.stop().await?;
    let _ = http_stop.send(true);
    if !server.is_finished() {
        server.await.context("Status task failed")??;
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
                        let generation = shared.runtime.lock().expect("runtime").boards.get(&id).map(|b| b.generation).unwrap_or(0);
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
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn topology_generations_release_owners_keep_status_and_preserve_faults() {
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
        let mut faults = std::collections::HashMap::new();
        faults.insert(
            "b2".into(),
            runtime::HardwareState {
                latched_fault: true,
                state: "thermal_stop".into(),
                ..Default::default()
            },
        );
        drop(reserved);
        let mut run = start_run(
            config.clone(),
            hardware::prepare(&config).await.unwrap(),
            dir.join("history.json"),
            &faults,
            unix_now() - 3600,
        )
        .await
        .unwrap();
        let service_started_unix = run.shared.service_started_unix;
        let (current, state) = watch::channel(run.shared.clone());
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = http.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(http, status_api::router(state)).await.unwrap();
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
                next.clone(),
                hardware::prepare(&next).await.unwrap(),
                dir.join("history.json"),
                &faults,
                service_started_unix,
            )
            .await
            .unwrap();
            current.send_replace(run.shared.clone());
            assert!(run.shared.runtime.lock().unwrap().boards["b2"].latched_fault);
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
    async fn idle_clients_stop_cleanly_and_release_their_slot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shared = Arc::new(Shared {
            native: None,
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

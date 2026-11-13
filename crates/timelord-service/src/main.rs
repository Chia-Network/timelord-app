mod asic;
mod assignment;
mod automatic;
use hardware::lifecycle as board_lifecycle;
mod bounded_lines;
mod config_checkpoint;
mod connection;
mod diagnostics;
mod experiments;
mod hardware;
mod history_buffer;
mod history_store;
mod inventory;
mod network;
mod node_status;
mod observability;
mod operational;
mod optional_io;
mod private_control;
mod runtime;
mod safety;
mod service;
mod stable_change;
mod status_api;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use service::{serve, shutdown_signal_from};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use timelord_core::{Assignment, Config};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

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
    network::resolve(&config.node)?;
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
    network::resolve(&config.node)?;
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
    native: Option<Arc<hardware::Facts>>,
    config: Config,
    assignments: Vec<Assignment>,
    // These are socket slots, not authenticated board/engine identities.
    clients: Arc<Mutex<BTreeSet<(String, u8)>>>,
    process: Arc<operational::State>,
    scope: runtime::RuntimeScope,
    runtime: Arc<Mutex<runtime::RuntimeState>>,
    started_unix: u64,
    service_started_unix: u64,
}

impl Shared {
    pub(crate) fn metrics(&self) -> Option<&Arc<observability::Metrics>> {
        (self.scope == runtime::RuntimeScope::Production).then_some(&self.process.metrics)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

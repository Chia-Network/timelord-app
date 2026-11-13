//! Small control plane: no USB driver, node protocol, or proof arithmetic.
pub mod accept_backoff;
pub mod atomic;
pub mod http;
pub mod logging;
pub mod power;
pub mod progress;

pub use accept_backoff::AcceptBackoff;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Running,
    Paused,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub schema_version: u32,
    pub revision: u64,
    pub mode: Mode,
}
impl Default for Intent {
    fn default() -> Self {
        Self {
            schema_version: 1,
            revision: 0,
            mode: Mode::Running,
        }
    }
}

/// A stable inode is locked for the entire lifetime; never unlink the lock file.
pub struct InstanceLock(File);
impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Concurrent fork/exec may temporarily inherit the open description
        // even with CLOEXEC. Releasing ownership must not depend on that child
        // reaching exec and closing its inherited descriptor.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
impl InstanceLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = private_open(path, true)?;
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        ensure!(locked == 0, "Another owner holds {}", path.display());
        Ok(Self(file))
    }
    pub fn descriptor(&self) -> libc::c_int {
        self.0.as_raw_fd()
    }
}

fn private_open(path: &Path, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("Opening {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.mode() & 0o077 == 0,
        "{} must be a private, owned regular file",
        path.display()
    );
    Ok(file)
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub directory: PathBuf,
    pub socket: PathBuf,
    pub intent: PathBuf,
    pub lock: PathBuf,
    pub worker_lock: PathBuf,
}
impl Paths {
    pub fn for_config(config: &Path) -> Result<Self> {
        // Each configuration has independent control state and locks. The
        // configuration itself may be unavailable; the control plane still runs.
        let directory = config
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()?;
        let stem = config
            .file_name()
            .context("Configuration needs a filename")?;
        let state = directory.join(format!(".{}-supervisor", stem.to_string_lossy()));
        match fs::DirBuilder::new().mode(0o700).create(&state) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let metadata = fs::symlink_metadata(&state)?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "Supervisor state directory must be private and owned"
        );
        Ok(Self {
            config: directory.join(stem),
            socket: state.join("control.sock"),
            intent: state.join("intent.json"),
            lock: state.join("owner.lock"),
            worker_lock: state.join("worker.lock"),
            directory: state,
        })
    }
    pub fn load_intent(&self) -> Result<Intent> {
        match fs::symlink_metadata(&self.intent) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Intent::default());
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let file = private_open(&self.intent, false)?;
        use std::io::Read;
        let mut bytes = vec![];
        file.take(4097).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 4096, "Saved intent is too large");
        let value: Intent =
            serde_json::from_slice(&bytes).context("Reading saved Run/Pause intent")?;
        ensure!(
            value.schema_version == 1,
            "Unsupported Run/Pause state version"
        );
        Ok(value)
    }
    pub fn save_intent(&self, value: &Intent) -> Result<()> {
        atomic::write(&self.intent, &serde_json::to_vec(value)?)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub command: Control,
    pub expected_revision: Option<u64>,
}
#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    Status,
    Pause,
    Resume,
    Reload,
}
/// Wire lifecycle phase; durable operating intent remains a separate Mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorPhase {
    Paused,
    StateError,
    Starting,
    WorkerStarted,
    Stopping,
    Recovering,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recovery {
    pub timestamp_unix: u64,
    pub worker_pid: Option<u32>,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    #[serde(default)]
    pub logging: logging::Stats,
    #[serde(default)]
    pub sleep_inhibited: bool,
    #[serde(default)]
    pub sleep_error: Option<String>,
    pub api_version: u32,
    pub supervisor_pid: u32,
    pub worker_pid: Option<u32>,
    pub mode: Mode,
    pub revision: u64,
    pub worker_generation: u64,
    pub restarts: u64,
    pub phase: SupervisorPhase,
    pub error: Option<String>,
    pub progress: Option<progress::Snapshot>,
    pub progress_age_seconds: Option<u64>,
    pub recoveries: Vec<Recovery>,
}
#[derive(Serialize, Deserialize)]
pub struct Response {
    pub status: Status,
    pub error: Option<String>,
}

pub async fn request(
    config: &Path,
    command: Control,
    expected_revision: Option<u64>,
) -> Result<Status> {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixStream,
    };
    let paths = Paths::for_config(config)?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(35), async {
        let mut socket = UnixStream::connect(&paths.socket)
            .await
            .context("Connecting to installed supervisor")?;
        peer_is_owner(&socket)?;
        socket
            .write_all(&serde_json::to_vec(&Request {
                command,
                expected_revision,
            })?)
            .await?;
        socket.write_all(b"\n").await?;
        use tokio::io::AsyncReadExt;
        let mut line = String::new();
        BufReader::new(socket.take(65537))
            .read_line(&mut line)
            .await?;
        ensure!(
            line.len() <= 65536 && line.ends_with('\n'),
            "Invalid supervisor response"
        );
        let response: Response = serde_json::from_str(&line)?;
        if let Some(error) = response.error {
            bail!("{error}");
        }
        Ok(response.status)
    })
    .await;
    result.context("Supervisor control timed out")?
}

pub fn peer_is_owner(socket: &tokio::net::UnixStream) -> Result<()> {
    #[cfg(target_os = "macos")]
    let uid = {
        let (mut uid, mut gid) = (0, 0);
        ensure!(
            unsafe { libc::getpeereid(socket.as_raw_fd(), &mut uid, &mut gid) } == 0,
            "Cannot inspect control peer"
        );
        uid
    };
    #[cfg(not(target_os = "macos"))]
    let uid = socket.peer_cred()?.uid();
    ensure!(
        uid == unsafe { libc::geteuid() },
        "Supervisor control requires the service account"
    );
    Ok(())
}

/// A supervisor crash closes stdin. A dedicated OS thread still enforces the
/// deadline if the worker's async runtime or blocking proof pool is wedged.
pub fn watch_parent() -> Result<()> {
    ensure!(
        unsafe { libc::getpgrp() == libc::getpid() },
        "Supervised worker must own its process group"
    );
    std::thread::Builder::new()
        .name("supervisor-lifetime".into())
        .spawn(|| {
            use std::io::Read;
            let mut byte = [0u8; 1];
            loop {
                match std::io::stdin().read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            unsafe {
                libc::kill(libc::getpid(), libc::SIGTERM);
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
            unsafe {
                libc::kill(-libc::getpgrp(), libc::SIGKILL);
            }
        })?;
    Ok(())
}

pub use timelord_core::Network;

/// Generate a system-domain job without installing or loading it. The installer
/// validates the selected account's numeric UID before performing privileged work.
pub fn launchd_plist(app: &Path, config: &Path, user: &str) -> Result<String> {
    launchd_plist_with_monitor(app, config, user, None, Network::Mainnet)
}
pub fn launchd_plist_with_monitor(
    app: &Path,
    config: &Path,
    user: &str,
    monitor: Option<std::net::SocketAddr>,
    network: Network,
) -> Result<String> {
    let monitor_args = if let Some(address) = monitor {
        ensure!(address.port() > 0, "Monitoring port must be nonzero");
        format!(
            "<string>--monitor-listen</string><string>{address}</string><string>--network</string><string>{}</string>",
            network.label()
        )
    } else {
        String::new()
    };
    ensure!(
        app.is_absolute() && config.is_absolute(),
        "App and configuration paths must be absolute"
    );
    ensure!(
        !user.is_empty()
            && user != "root"
            && user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            && !user.starts_with('-'),
        "Select a non-root deployment account"
    );
    fn xml(value: &str) -> Result<String> {
        ensure!(
            value.chars().all(|c| !c.is_control()),
            "Control characters are not allowed in launchd paths"
        );
        Ok(value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;"))
    }
    let helper = xml(app
        .join("Contents/Helpers/timelord-supervisor")
        .to_str()
        .context("App path must be UTF-8")?)?;
    let config_text = xml(config
        .to_str()
        .context("Configuration path must be UTF-8")?)?;
    // Worker diagnostics have their own bounded rotating sink. Do not retain
    // unlimited launchd stdout/stderr across repeated supervisor failures.
    let log = "/dev/null";
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>net.chia.timelord.supervisor</string>
<key>AssociatedBundleIdentifiers</key><string>net.chia.timelord.mac</string>
<key>UserName</key><string>{user}</string>
<key>ProgramArguments</key><array><string>{helper}</string><string>run</string><string>--config</string><string>{config_text}</string>{monitor_args}</array>
<key>WorkingDirectory</key><string>/</string>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ThrottleInterval</key><integer>10</integer>
<key>ExitTimeOut</key><integer>20</integer>
<key>AbandonProcessGroup</key><false/>
<key>ProcessType</key><string>Interactive</string>
<key>StandardOutPath</key><string>{log}</string>
<key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#
    ))
}

#[cfg(test)]
mod phase_tests {
    use super::SupervisorPhase;
    #[test]
    fn phase_contract_round_trips_the_shared_swift_fixture() {
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/supervisor-phases.json")).unwrap();
        let phases: Vec<SupervisorPhase> = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            phases,
            vec![
                SupervisorPhase::Paused,
                SupervisorPhase::StateError,
                SupervisorPhase::Starting,
                SupervisorPhase::WorkerStarted,
                SupervisorPhase::Stopping,
                SupervisorPhase::Recovering
            ]
        );
        assert_eq!(serde_json::to_value(phases).unwrap(), wire);
        assert!(serde_json::from_str::<SupervisorPhase>("\"unknown_phase\"").is_err());
    }
}

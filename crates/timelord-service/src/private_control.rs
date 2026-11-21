//! Same-user, owner-only Unix IPC. It can hold/resume tuning, apply a saved
//! node-profile change and retry or disable a board. It cannot set hardware
//! operating points or control arbitrary processes.
use crate::Shared;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use timelord_supervisor::AcceptBackoff;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Semaphore, watch},
};
/// Private-control transactions allowed at once; a local client must not queue
/// unbounded handlers ahead of the service.
const MAX_IN_FLIGHT: usize = 4;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TuningRequest {
    paused: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum BoardAction {
    /// End probation now and retry a waiting owner immediately.
    Retry,
    /// Keep the board out of rotation (optionally for a bounded time).
    Disable,
    Enable,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    ReloadNode,
    Board {
        serial: String,
        action: BoardAction,
        #[serde(default)]
        seconds: Option<u64>,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Request {
    Tuning(TuningRequest),
    Command(Command),
}
#[derive(Serialize, Deserialize)]
pub struct Response {
    pub paused: bool,
    pub service_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
/// A node-profile change request handed to the service owner loop.
pub type NodeReload = tokio::sync::oneshot::Sender<std::result::Result<(), String>>;
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    inode: u64,
}
impl Drop for Listener {
    fn drop(&mut self) {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.file_type().is_socket() && m.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
pub fn bind(path: &Path) -> Result<Listener> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            bail!("Private control path is not an owned socket");
        }
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => bail!("Another service owns the private control socket"),
            Err(e)
                if [
                    std::io::ErrorKind::ConnectionRefused,
                    std::io::ErrorKind::NotFound,
                ]
                .contains(&e.kind()) =>
            {
                std::fs::remove_file(path)?
            }
            Err(e) => return Err(e).context("Cannot inspect the previous private control socket"),
        }
    }
    let listener = UnixListener::bind(path).context("Binding private control socket")?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(Listener {
        listener,
        path: path.to_owned(),
        inode: std::fs::symlink_metadata(path)?.ino(),
    })
}
async fn handle(
    mut socket: UnixStream,
    shared: Arc<Shared>,
    node: tokio::sync::mpsc::Sender<NodeReload>,
) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let (mut uid, mut gid) = (0, 0);
        if unsafe { libc::getpeereid(socket.as_raw_fd(), &mut uid, &mut gid) } != 0
            || uid != unsafe { libc::geteuid() }
        {
            bail!("Private control peer is not the service user");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if socket.peer_cred()?.uid() != unsafe { libc::geteuid() } {
            bail!("Private control peer is not the service user");
        }
    }
    let mut line = String::new();
    use tokio::io::AsyncReadExt;
    BufReader::new((&mut socket).take(257))
        .read_line(&mut line)
        .await?;
    if line.len() > 256 || !line.ends_with('\n') {
        bail!("Invalid private control request");
    }
    let request: Request = serde_json::from_str(&line)?;
    let error = match request {
        Request::Tuning(request) => {
            shared.runtime.lock().expect("runtime").tuning_paused = request.paused;
            None
        }
        Request::Command(Command::ReloadNode) => {
            let (reply, outcome) = tokio::sync::oneshot::channel();
            node.send(reply)
                .await
                .map_err(|_| anyhow::anyhow!("Service is stopping"))?;
            outcome.await.context("Service is stopping")?.err()
        }
        Request::Command(Command::Board {
            serial,
            action,
            seconds,
        }) => board_action(&shared, &serial, action, seconds)
            .err()
            .map(|e| format!("{e:#}")),
    };
    let response = Response {
        paused: shared.runtime.lock().expect("runtime").tuning_paused,
        service_pid: std::process::id(),
        error,
    };
    socket.write_all(&serde_json::to_vec(&response)?).await?;
    socket.write_all(b"\n").await?;
    Ok(())
}
fn board_action(
    shared: &Shared,
    serial: &str,
    action: BoardAction,
    seconds: Option<u64>,
) -> Result<()> {
    let id = shared
        .config
        .hardware
        .boards
        .iter()
        .find(|b| b.serial.as_deref() == Some(serial))
        .map(|b| b.id.clone())
        .context("No configured board has that serial")?;
    let store = shared
        .runtime
        .lock()
        .expect("runtime")
        .safety
        .clone()
        .context("Board journal unavailable")?;
    let now = crate::unix_now();
    match action {
        BoardAction::Retry => {
            store.update_health(serial, true, |h| h.retry_now(now))?;
            if let Some(hardware) = shared.runtime.lock().expect("runtime").boards.get_mut(&id) {
                hardware.retry_requested = true;
            }
        }
        BoardAction::Disable => {
            store.update_health(serial, false, |h| {
                h.disable(seconds.map(|s| now.saturating_add(s)))
            })?;
        }
        BoardAction::Enable => {
            store.update_health(serial, true, |h| h.enable())?;
            if let Some(hardware) = shared.runtime.lock().expect("runtime").boards.get_mut(&id) {
                hardware.retry_requested = true;
            }
        }
    }
    tracing::info!(serial, ?action, ?seconds, "Operator board action applied");
    Ok(())
}
pub async fn run(
    control: Listener,
    shared: Arc<Shared>,
    shutdown: watch::Receiver<bool>,
    node: tokio::sync::mpsc::Sender<NodeReload>,
) -> Result<()> {
    run_with(AcceptBackoff::new(), control, shared, shutdown, node).await
}

async fn run_with(
    mut backoff: AcceptBackoff,
    control: Listener,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
    node: tokio::sync::mpsc::Sender<NodeReload>,
) -> Result<()> {
    let in_flight = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            accepted=backoff.accept(control.listener.accept(), |error, failures, delay| {
                tracing::warn!(%error,failures=failures,?delay,"Private control accept failed; retrying");
            })=> {
                let Some((socket,_))=accepted else { continue; };
                // Reserve a slot before spawning so waiting tasks stay bounded.
                let permit = tokio::select! {
                    _=shutdown.changed()=>break,
                    permit=in_flight.clone().acquire_owned()=>match permit {
                        Ok(permit)=>permit,
                        Err(_)=>break,
                    },
                };
                // Bounded same-user transaction; no unauthenticated public HTTP mutation.
                // A node-profile switch drains the old session; allow it longer.
                let shared = shared.clone();
                let node = node.clone();
                tokio::spawn(async move {
                    let _permit=permit;
                    if let Err(error)=tokio::time::timeout(Duration::from_secs(30),handle(socket,shared,node)).await {tracing::debug!(%error,"Private control timed out");}
                });
            }
        }
    }
    Ok(())
}
pub async fn request(path: &Path, paused: bool) -> Result<Response> {
    send(path, Request::Tuning(TuningRequest { paused }), 3).await
}
pub async fn command(path: &Path, command: Command) -> Result<Response> {
    let response = send(path, Request::Command(command), 35).await?;
    if let Some(error) = &response.error {
        bail!("{error}");
    }
    Ok(response)
}
async fn send(path: &Path, request: Request, seconds: u64) -> Result<Response> {
    tokio::time::timeout(Duration::from_secs(seconds), async {
        let mut socket = UnixStream::connect(path)
            .await
            .context("The running service does not support live control")?;
        socket.write_all(&serde_json::to_vec(&request)?).await?;
        socket.write_all(b"\n").await?;
        let mut line = String::new();
        use tokio::io::AsyncReadExt;
        BufReader::new(socket.take(1025))
            .read_line(&mut line)
            .await?;
        if line.len() > 1024 || !line.ends_with('\n') {
            bail!("Invalid private control response");
        }
        Ok(serde_json::from_str(&line)?)
    })
    .await
    .context("Private control request timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn private_controls_hold_tuning_without_changing_production_or_exposing_a_port() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("tl-control-{}.sock", std::process::id()));
        let control = bind(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(bind(&path).is_err());
        let shared = Arc::new(crate::Shared::new(
            None,
            timelord_core::Config::default(),
            std::sync::Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState::new(
                false,
            ))),
            crate::operational::State::fixture(),
            crate::unix_now(),
            crate::unix_now(),
        ));
        shared.runtime.lock().unwrap().phase = "running".into();
        let (stop, shutdown) = watch::channel(false);
        let (node, _node_requests) = tokio::sync::mpsc::channel(1);
        let server = tokio::spawn(run(control, shared.clone(), shutdown, node));
        let response = request(&path, true).await.unwrap();
        assert!(response.paused);
        assert_eq!(response.service_pid, std::process::id());
        assert_eq!(shared.runtime.lock().unwrap().phase, "running");
        assert!(!request(&path, false).await.unwrap().paused);
        let mut invalid = UnixStream::connect(&path).await.unwrap();
        invalid
            .write_all(b"{\"paused\":true,\"frequency\":1700}\n")
            .await
            .unwrap();
        invalid.shutdown().await.unwrap();
        // Wait for the server to consume the invalid request, then read the flag.
        assert!(!request(&path, false).await.unwrap().paused);
        assert!(!shared.runtime.lock().unwrap().tuning_paused);
        stop.send(true).unwrap();
        server.await.unwrap().unwrap();
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn transient_accept_errors_back_off_then_serve_real_request() {
        let path =
            std::env::temp_dir().join(format!("tl-control-backoff-{}.sock", std::process::id()));
        let control = bind(&path).unwrap();
        let shared = Arc::new(crate::Shared::new(
            None,
            timelord_core::Config::default(),
            std::sync::Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState::new(
                false,
            ))),
            crate::operational::State::fixture(),
            crate::unix_now(),
            crate::unix_now(),
        ));
        let (stop, shutdown) = watch::channel(false);
        let started = std::time::Instant::now();
        let (node, _node_requests) = tokio::sync::mpsc::channel(1);
        let server = tokio::spawn(run_with(
            AcceptBackoff::with_injected([
                std::io::Error::from_raw_os_error(libc::ECONNABORTED),
                std::io::Error::from_raw_os_error(libc::EMFILE),
            ]),
            control,
            shared,
            shutdown,
            node,
        ));
        assert!(request(&path, true).await.unwrap().paused);
        assert!(started.elapsed() >= Duration::from_millis(140));
        stop.send(true).unwrap();
        server.await.unwrap().unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn cleanup_never_removes_a_replacement_file() {
        let path = std::env::temp_dir().join(format!("tl-replacement-{}.sock", std::process::id()));
        let listener = bind(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"keep").unwrap();
        drop(listener);
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
        std::fs::remove_file(path).unwrap();
    }
}

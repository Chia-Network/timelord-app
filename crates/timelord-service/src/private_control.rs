//! Same-user, owner-only Unix IPC. It can only pause/resume tuning, not set hardware.
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
    sync::watch,
};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    paused: bool,
}
#[derive(Serialize, Deserialize)]
pub struct Response {
    pub paused: bool,
    pub service_pid: u32,
}
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
async fn handle(mut socket: UnixStream, shared: Arc<Shared>) -> Result<()> {
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
    shared.runtime.lock().expect("runtime").tuning_paused = request.paused;
    let response = Response {
        paused: request.paused,
        service_pid: std::process::id(),
    };
    socket.write_all(&serde_json::to_vec(&response)?).await?;
    socket.write_all(b"\n").await?;
    Ok(())
}
pub async fn run(
    control: Listener,
    shared: Arc<Shared>,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    run_with(AcceptBackoff::new(), control, shared, shutdown).await
}

async fn run_with(
    mut backoff: AcceptBackoff,
    control: Listener,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        tokio::select! {
            _=shutdown.changed()=>break,
            accepted=backoff.accept(control.listener.accept(), |error, failures, delay| {
                tracing::warn!(%error,failures=failures,?delay,"Private control accept failed; retrying");
            })=> {
                let Some((socket,_))=accepted else { continue; };
                // Bounded same-user transaction; no unauthenticated public HTTP mutation.
                if let Err(error)=tokio::time::timeout(Duration::from_secs(2),handle(socket,shared.clone())).await {tracing::debug!(%error,"Private control timed out");}
            }
        }
    }
    Ok(())
}
pub async fn request(path: &Path, paused: bool) -> Result<Response> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut socket = UnixStream::connect(path)
            .await
            .context("The running service does not support live tuning control")?;
        socket
            .write_all(&serde_json::to_vec(&Request { paused })?)
            .await?;
        socket.write_all(b"\n").await?;
        let mut line = String::new();
        use tokio::io::AsyncReadExt;
        BufReader::new(socket.take(257))
            .read_line(&mut line)
            .await?;
        if line.len() > 256 || !line.ends_with('\n') {
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
        let shared = Arc::new(Shared {
            native: None,
            config: timelord_core::Config::default(),
            assignments: vec![],
            clients: Default::default(),
            process: crate::operational::State::fixture(),
            scope: crate::runtime::RuntimeScope::Production,
            runtime: Default::default(),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        });
        shared.runtime.lock().unwrap().phase = "running".into();
        let (stop, shutdown) = watch::channel(false);
        let server = tokio::spawn(run(control, shared.clone(), shutdown));
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
        let shared = Arc::new(Shared {
            native: None,
            config: timelord_core::Config::default(),
            assignments: vec![],
            clients: Default::default(),
            process: crate::operational::State::fixture(),
            runtime: Default::default(),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
            scope: crate::runtime::RuntimeScope::Production,
        });
        let (stop, shutdown) = watch::channel(false);
        let started = std::time::Instant::now();
        let server = tokio::spawn(run_with(
            AcceptBackoff::with_injected([
                std::io::Error::from_raw_os_error(libc::ECONNABORTED),
                std::io::Error::from_raw_os_error(libc::EMFILE),
            ]),
            control,
            shared,
            shutdown,
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

//! Start a same-user session supervisor without changing saved Run/Pause intent.
use anyhow::{Context, Result, bail};
use std::{
    net::SocketAddr, os::unix::process::CommandExt, path::PathBuf, process::Stdio, time::Duration,
};
use timelord_supervisor::{Control, InstanceLock, Network, Paths, Status, request};

/// Bytes of a failed child's stderr kept for the startup error. A startup
/// failure explains itself in a few lines; a noisy child must not grow the
/// short-lived CLI's memory.
const STARTUP_DIAGNOSTICS: u64 = 8 * 1024;

pub async fn ensure_started(
    config: PathBuf,
    worker: Option<PathBuf>,
    monitor: Option<SocketAddr>,
    status_listen: Option<SocketAddr>,
    network: Network,
) -> Result<Status> {
    let paths = Paths::for_config(&config)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(current) = status(&paths.config).await {
            return Ok(current);
        }
        // A starting/stopping owner may not serve requests yet. Wait for it;
        // never replace an unresponsive owner or an old foreground worker.
        let owner = match InstanceLock::acquire(&paths.lock) {
            Ok(owner) => owner,
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(80)).await;
                continue;
            }
        };
        let worker_owner = InstanceLock::acquire(&paths.worker_lock)?;
        if let Some(address) = status_listen
            && tokio::time::timeout(
                Duration::from_secs(1),
                tokio::net::TcpStream::connect(address),
            )
            .await
            .is_ok_and(|r| r.is_ok())
        {
            bail!(
                "An existing worker is listening; close the old foreground Timelord before starting supervision"
            );
        }
        drop(worker_owner);
        drop(owner);
        break;
    }
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("run")
        .arg("--config")
        .arg(&paths.config)
        .arg("--network")
        .arg(network.label());
    supervisor_stdio(&mut command);
    if let Some(worker) = worker {
        command.arg("--worker").arg(worker);
    }
    if let Some(address) = monitor {
        command.arg("--monitor-listen").arg(address.to_string());
    }
    // The supervisor outlives both this short-lived CLI and the GUI. Only the
    // supervisor owns worker process groups; quitting the GUI never means Pause.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .context("Starting user-session supervisor")?;
    wait_for_status(&paths.config, child, Duration::from_secs(8)).await
}

/// Run the supervisor without a terminal and keep its stderr on a pipe: `run`
/// reports a startup failure only there, so `wait_for_status` can surface the
/// one artifact that explains why supervision never became available.
fn supervisor_stdio(command: &mut std::process::Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
}

async fn status(config: &std::path::Path) -> Result<Status> {
    tokio::time::timeout(
        Duration::from_secs(1),
        request(config, Control::Status, None),
    )
    .await
    .context("Supervisor did not respond")?
}

async fn wait_for_status(
    config: &std::path::Path,
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<Status> {
    let mut diagnostics = child.stderr.take();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(current) = status(config).await {
            return Ok(current);
        }
        let exit = child.try_wait()?;
        if tokio::time::Instant::now() >= deadline {
            // Only signal the child we spawned, never a concurrent lock winner.
            // SIGTERM uses normal supervisor cleanup; do not force-kill a worker
            // that may already own hardware or change the saved Run/Pause intent.
            if exit.is_none() {
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGTERM);
                }
                let cleanup = tokio::time::Instant::now() + Duration::from_secs(8);
                while child.try_wait()?.is_none() {
                    if tokio::time::Instant::now() >= cleanup {
                        bail!(
                            "User-session supervisor startup timed out; shutdown is still pending (pid: {})",
                            child.id()
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            // The child has exited and every child the supervisor starts gets
            // its own stdio, so no grandchild holds this pipe: it yields what
            // the child wrote followed by EOF and cannot block.
            let exit = child.try_wait()?;
            bail!(
                "User-session supervisor did not become available (exit: {exit:?}); startup child has exited{}",
                startup_diagnostics(diagnostics.take())
            );
        }
        // A concurrent starter may win the lock; wait for that owner rather
        // than confusing the losing child's exit with a failed startup.
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

/// Quote a reaped child's stderr for the startup error. Only the first
/// STARTUP_DIAGNOSTICS bytes are kept so a child that writes without bound
/// cannot make the CLI's error unbounded too.
fn startup_diagnostics(pipe: Option<std::process::ChildStderr>) -> String {
    use std::io::Read;
    let Some(pipe) = pipe else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = pipe.take(STARTUP_DIAGNOSTICS).read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    if bytes.len() as u64 == STARTUP_DIAGNOSTICS {
        return format!(":\n{text}\n[stderr truncated]");
    }
    format!(":\n{text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_failure_reports_the_child_stderr() {
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("echo 'native client refused the configured network' >&2; exit 7");
        supervisor_stdio(&mut command);
        let mut child = command.spawn().unwrap();
        // Reap first: the error path must need neither a signal nor a read that
        // waits for the child.
        while child.try_wait().unwrap().is_none() {}
        let pid = child.id();
        let config = std::env::temp_dir().join(format!("tl-startup-stderr-{pid}/config.toml"));
        let error = wait_for_status(&config, child, Duration::ZERO)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("startup child has exited"), "{error}");
        assert!(
            error.contains("native client refused the configured network"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn startup_timeout_terminates_and_reaps_only_its_child() {
        let child = std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = child.id();
        let config = std::env::temp_dir().join(format!("tl-startup-timeout-{pid}/config.toml"));
        let result = wait_for_status(&config, child, Duration::ZERO).await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("startup child has exited")
        );
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }
}

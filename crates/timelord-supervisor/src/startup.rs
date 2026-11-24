//! Start a same-user session supervisor without changing saved Run/Pause intent.
use anyhow::{Context, Result, bail};
use std::{
    net::SocketAddr, os::unix::process::CommandExt, path::PathBuf, process::Stdio, time::Duration,
};
use timelord_supervisor::{Control, InstanceLock, Network, Paths, Status, request};

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
        .arg(network.label())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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
            bail!(
                "User-session supervisor did not become available (exit: {exit:?}); startup child has exited"
            );
        }
        // A concurrent starter may win the lock; wait for that owner rather
        // than confusing the losing child's exit with a failed startup.
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

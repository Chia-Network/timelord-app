//! Start a same-user session supervisor without changing saved Run/Pause intent.
use anyhow::{Context, Result, bail};
use std::{
    net::SocketAddr, os::unix::process::CommandExt, path::PathBuf, process::Stdio, time::Duration,
};
use timelord_supervisor::{Control, InstanceLock, Network, Paths, Status, request};

/// Bytes of a failed child's stderr kept for the startup error. A startup
/// failure explains itself in a few lines; a noisy child must not grow the
/// short-lived CLI's memory.
const STARTUP_DIAGNOSTICS: usize = 8 * 1024;
/// Bound on waiting for a reaped child's output. The child is already gone, so
/// this only guards against a stray descendant that inherited its stderr.
const STARTUP_DIAGNOSTICS_WAIT: Duration = Duration::from_secs(1);

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
        // This CLI owns the child's stderr pipe to capture a startup failure, so
        // the supervisor releases fd 2 once it serves status.
        .arg("--own-stderr");
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
    let mut diagnostics = child.stderr.take().map(Diagnostics::read);
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
            // Report the status the child actually exited with; the loop's
            // earlier observation may predate a timeout SIGTERM. A failed
            // re-read must not replace the startup error with a bare io error.
            let exit = child.try_wait().unwrap_or(exit);
            let captured = match diagnostics.take() {
                Some(diagnostics) => diagnostics.finish().await,
                None => None,
            };
            bail!(
                "User-session supervisor did not become available (exit: {exit:?}); startup child has exited{}",
                describe(captured)
            );
        }
        // A concurrent starter may win the lock; wait for that owner rather
        // than confusing the losing child's exit with a failed startup.
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

/// Bounded capture of the child's stderr. A reader thread drains the pipe from
/// the moment the child starts, so a chatty child cannot block on a full pipe.
/// Only the most recent bytes are kept: `main` writes its decisive `Error: …`
/// line last, after any noise.
struct Diagnostics {
    reading: std::sync::Arc<std::sync::Mutex<Reading>>,
    done: Option<tokio::sync::oneshot::Receiver<()>>,
}

#[derive(Default)]
struct Reading {
    kept: Vec<u8>,
    truncated: bool,
}

struct Captured {
    text: String,
    truncated: bool,
}

impl Reading {
    fn push(&mut self, chunk: &[u8]) {
        self.kept.extend_from_slice(chunk);
        if self.kept.len() > STARTUP_DIAGNOSTICS {
            let excess = self.kept.len() - STARTUP_DIAGNOSTICS;
            self.kept.drain(..excess);
            self.truncated = true;
        }
    }
    /// Decode the kept bytes. A truncated window can begin inside a multi-byte
    /// character, so its leading continuation bytes are skipped rather than
    /// showing up as a replacement character.
    fn text(&self) -> String {
        let mut bytes = &self.kept[..];
        while let [first, rest @ ..] = bytes
            && *first & 0xC0 == 0x80
        {
            bytes = rest;
        }
        String::from_utf8_lossy(bytes).trim().to_string()
    }
}

impl Diagnostics {
    fn read(mut pipe: std::process::ChildStderr) -> Self {
        use std::io::Read;
        let reading = std::sync::Arc::new(std::sync::Mutex::new(Reading::default()));
        let sink = reading.clone();
        let (sender, done) = tokio::sync::oneshot::channel();
        let reader = std::thread::Builder::new()
            .name("supervisor-startup-stderr".into())
            .spawn(move || {
                let mut chunk = [0u8; 1024];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(size) => sink.lock().expect("stderr capture").push(&chunk[..size]),
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                let _ = sender.send(());
            });
        // A reader that cannot start only costs diagnostics; the startup error
        // is still reported.
        Self {
            reading,
            done: reader.ok().map(|_| done),
        }
    }
    /// The child has exited, so its output is buffered and the reader is about
    /// to see EOF. Wait briefly, then report what was captured even if a stray
    /// descendant kept the pipe open.
    async fn finish(self) -> Option<Captured> {
        if let Some(done) = self.done {
            let _ = tokio::time::timeout(STARTUP_DIAGNOSTICS_WAIT, done).await;
        }
        let reading = self.reading.lock().expect("stderr capture");
        let text = reading.text();
        if text.is_empty() {
            return None;
        }
        Some(Captured {
            text,
            truncated: reading.truncated,
        })
    }
}

/// Quote a reaped child's output for the startup error.
fn describe(captured: Option<Captured>) -> String {
    let Some(Captured { text, truncated }) = captured else {
        return String::new();
    };
    if text.is_empty() {
        return String::new();
    }
    match truncated {
        true => format!(":\n{text}\n[stderr truncated]"),
        false => format!(":\n{text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Supervise a script that is expected to fail, and return the startup
    /// error it produced. The script must fail quickly enough that the child
    /// has exited before the zero timeout, so the error path only reads the
    /// status it already has.
    async fn startup_error(script: &str) -> String {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(script);
        supervisor_stdio(&mut command);
        let mut child = command.spawn().unwrap();
        assert!(!child.wait().unwrap().success());
        let config = std::env::temp_dir()
            .join(format!("tl-startup-{}-{}", std::process::id(), child.id()))
            .join("config.toml");
        wait_for_status(&config, child, Duration::ZERO)
            .await
            .unwrap_err()
            .to_string()
    }

    /// Drain the stderr of a failing script the way wait_for_status does: the
    /// reader starts with the child, so it also drains a full pipe.
    async fn captured(script: &str) -> Captured {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(script);
        supervisor_stdio(&mut command);
        let mut child = command.spawn().unwrap();
        let diagnostics = Diagnostics::read(child.stderr.take().unwrap());
        assert!(!child.wait().unwrap().success());
        diagnostics.finish().await.expect("stderr was captured")
    }

    #[tokio::test]
    async fn startup_failure_reports_the_child_stderr() {
        let error =
            startup_error("echo 'native client refused the configured network' >&2; exit 7").await;
        assert!(error.contains("startup child has exited"), "{error}");
        assert!(
            error.contains("native client refused the configured network"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn startup_diagnostics_keep_the_tail_after_a_full_pipe() {
        // More than a pipe buffer, so the reader has to drain while the child
        // runs; the last line is the one that explains the failure.
        let captured = captured(
            "{ printf 'HEAD-MARKER'; yes a | head -c 1048576; printf 'TAIL-MARKER'; } >&2; exit 7",
        )
        .await;
        assert!(captured.truncated);
        assert!(captured.text.contains("TAIL-MARKER"), "{}", captured.text);
        assert!(!captured.text.contains("HEAD-MARKER"), "{}", captured.text);
    }

    #[tokio::test]
    async fn startup_diagnostics_do_not_mark_output_at_the_limit() {
        // Exactly STARTUP_DIAGNOSTICS bytes must not be reported as truncated.
        let captured =
            captured("{ printf 'HEAD-MARKER'; printf '%8181s' '' | tr ' ' a; } >&2; exit 7").await;
        assert!(!captured.truncated);
        assert!(captured.text.contains("HEAD-MARKER"), "{}", captured.text);
    }

    #[tokio::test]
    async fn startup_diagnostics_do_not_garble_a_split_character() {
        // Two bytes, then 10000 bytes of two-byte characters: the kept window
        // begins inside a character, so its continuation bytes are skipped.
        let captured =
            captured("{ printf 'XY'; yes 'é' | tr -d '\\n' | head -c 10000; } >&2; exit 7").await;
        assert!(captured.truncated);
        assert!(
            captured.text.chars().all(|character| character == 'é'),
            "{:?}",
            captured.text.chars().take(4).collect::<String>()
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

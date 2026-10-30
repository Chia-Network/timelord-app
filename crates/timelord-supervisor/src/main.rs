use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    fs,
    io::Write,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use timelord_supervisor::{
    Control, InstanceLock, Intent, Mode, Paths, Request, Response, Status, peer_is_owner,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
    time::Instant,
};

#[derive(Parser)]
#[command(
    version,
    about = "Persistent Timelord supervision and same-user control"
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Print the system LaunchDaemon registration without installing anything.
    LaunchdPlist {
        #[arg(long)]
        app: PathBuf,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        user: String,
    },
    /// Refuse migration while an existing coordinator holds its ownership lock.
    CheckOwner {
        #[arg(long)]
        config: PathBuf,
    },
    Run {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        worker: Option<PathBuf>,
    },
    Status {
        #[arg(long)]
        config: PathBuf,
    },
    Pause {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        expected_revision: Option<u64>,
    },
    Resume {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        expected_revision: Option<u64>,
    },
    Reload {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        expected_revision: Option<u64>,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Action::LaunchdPlist { app, config, user } => {
            print!(
                "{}",
                timelord_supervisor::launchd_plist(&app, &config, &user)?
            );
            Ok(())
        }
        Action::CheckOwner { config } => {
            let paths = Paths::for_config(&config)?;
            let _supervisor = InstanceLock::acquire(&paths.lock)?;
            let _worker = InstanceLock::acquire(&paths.worker_lock)?;
            println!("{}", serde_json::json!({"available":true}));
            Ok(())
        }
        Action::Run { config, worker } => {
            let helper =
                worker.unwrap_or(std::env::current_exe()?.with_file_name("timelord-service"));
            run(Paths::for_config(&config)?, helper).await
        }
        action => {
            let (config, command, revision) = match action {
                Action::Status { config } => (config, Control::Status, None),
                Action::Pause {
                    config,
                    expected_revision,
                } => (config, Control::Pause, expected_revision),
                Action::Resume {
                    config,
                    expected_revision,
                } => (config, Control::Resume, expected_revision),
                Action::Reload {
                    config,
                    expected_revision,
                } => (config, Control::Reload, expected_revision),
                Action::Run { .. } | Action::LaunchdPlist { .. } | Action::CheckOwner { .. } => {
                    unreachable!()
                }
            };
            println!(
                "{}",
                serde_json::to_string(
                    &timelord_supervisor::request(&config, command, revision).await?
                )?
            );
            Ok(())
        }
    }
}

struct SocketGuard {
    path: PathBuf,
    inode: u64,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.file_type().is_socket() && m.ino() == self.inode)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}
fn retry_delay(failures: &mut usize, elapsed: Option<Duration>) -> u64 {
    if elapsed.is_some_and(|elapsed| elapsed >= Duration::from_secs(60)) {
        *failures = 0;
    }
    let delay = [1, 5, 30, 60, 120][(*failures).min(4)];
    *failures = failures.saturating_add(1);
    delay
}

struct Worker {
    child: Child,
    group: i32,
    started: Instant,
    #[cfg(test)]
    stop_failures: usize,
    #[cfg(test)]
    wait_errors: std::cell::RefCell<std::collections::VecDeque<i32>>,
}
impl Worker {
    fn spawn(helper: &Path, config: &Path) -> Result<Self> {
        let child = Command::new(helper)
            .arg("serve")
            .arg("--supervised")
            .arg("--config")
            .arg(config)
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .context("Starting timelord worker")?;
        let group = child.id().context("Worker has no process id")? as i32;
        Ok(Self {
            child,
            group,
            started: Instant::now(),
            #[cfg(test)]
            stop_failures: 0,
            #[cfg(test)]
            wait_errors: Default::default(),
        })
    }
    fn signal(&self, signal: i32) {
        unsafe {
            libc::kill(-self.group, signal);
        };
    }
    fn exited(&self) -> Result<bool> {
        // Observe exit without reaping. Keeping the leader's PID reserved until
        // descendants are terminated prevents signaling a reused process group.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if let Err(error) = self.waitid(&mut info) {
            // EINTR is not evidence of exit. The existing tick/stop polling
            // intervals retry without spinning or relinquishing child identity.
            if error.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error).context("Observing timelord worker exit without reaping");
        }
        #[cfg(target_os = "macos")]
        let pid = info.si_pid;
        #[cfg(not(target_os = "macos"))]
        let pid = unsafe { info.si_pid() };
        // Darwin can report a stop notification even with WEXITED. A stopped
        // process is not an exited owner; retain its process-group identity.
        Ok(pid != 0
            && matches!(
                info.si_code,
                libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
            ))
    }
    fn waitid(&self, info: &mut libc::siginfo_t) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(error) = self.wait_errors.borrow_mut().pop_front() {
            return Err(std::io::Error::from_raw_os_error(error));
        }
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.group as libc::id_t,
                info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    async fn stop(&mut self) -> Result<std::process::ExitStatus> {
        #[cfg(test)]
        if self.stop_failures > 0 {
            self.stop_failures -= 1;
            anyhow::bail!("Injected worker cleanup failure");
        }
        self.signal(libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.exited()? && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.signal(libc::SIGKILL);
        let result = tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .context("Worker did not exit after SIGKILL")??;
        self.group = 0;
        Ok(result)
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if self.group > 0 {
            self.signal(libc::SIGKILL);
        }
    }
}
struct Envelope {
    request: Request,
    reply: oneshot::Sender<Response>,
}
type Snapshot = watch::Receiver<Status>;
async fn handle(mut socket: UnixStream, state: Snapshot, tx: mpsc::Sender<Envelope>) -> Result<()> {
    peer_is_owner(&socket)?;
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(3),
        BufReader::new((&mut socket).take(1025)).read_line(&mut line),
    )
    .await??;
    ensure!(
        line.len() <= 1024 && line.ends_with('\n'),
        "Invalid supervisor control frame"
    );
    let request: Request = serde_json::from_str(&line)?;
    let response = if matches!(request.command, Control::Status) {
        Response {
            status: state.borrow().clone(),
            error: None,
        }
    } else {
        let (reply, rx) = oneshot::channel();
        tx.try_send(Envelope { request, reply })
            .context("Supervisor control queue is full")?;
        tokio::time::timeout(Duration::from_secs(30), rx).await??
    };
    socket.write_all(&serde_json::to_vec(&response)?).await?;
    socket.write_all(b"\n").await?;
    Ok(())
}
async fn accept_controls(
    listener: UnixListener,
    current: Snapshot,
    tx: mpsc::Sender<Envelope>,
    #[cfg(test)] mut accept_errors: std::collections::VecDeque<std::io::Error>,
) -> Result<()> {
    let mut handlers = JoinSet::new();
    let mut failures: u32 = 0;
    let mut retry_at = Instant::now();
    loop {
        tokio::select! {
            Some(_) = handlers.join_next(), if !handlers.is_empty() => {},
            accepted = async {
                tokio::time::sleep_until(retry_at).await;
                #[cfg(test)]
                if let Some(error) = accept_errors.pop_front() { return Err(error); }
                listener.accept().await
            } => {
                let (socket, _) = match accepted {
                    Ok(accepted) => { failures = 0; accepted }
                    Err(error) => {
                        // An aborted peer or descriptor pressure does not revoke
                        // this listener or the healthy worker it controls.
                        failures = failures.saturating_add(1);
                        let delay = Duration::from_millis(50 * (1u64 << failures.saturating_sub(1).min(5))).min(Duration::from_secs(1));
                        let _ = writeln!(std::io::stderr(), "Supervisor control accept failed ({failures} consecutive): {error}; retry in {delay:?}");
                        retry_at = Instant::now() + delay;
                        continue;
                    }
                };
                if handlers.len() < 16 {
                    let (current, tx) = (current.clone(), tx.clone());
                    handlers.spawn(async move { let _ = handle(socket, current, tx).await; });
                }
            }
        }
    }
}
async fn signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
}
/// The loop owns every lifecycle transition. Readers receive immutable projections
/// rather than independently mutable mode, PID and phase fields.
enum Lifecycle {
    Paused,
    StateError(String),
    Starting,
    Running(Worker),
    Stopping {
        pid: u32,
    },
    CleanupFailed {
        worker: Worker,
        next: Instant,
        error: String,
    },
    Backoff {
        next: Instant,
        error: String,
    },
}
struct SupervisorState {
    intent: Intent,
    lifecycle: Lifecycle,
    generation: u64,
    restarts: u64,
    failures: usize,
    power: timelord_supervisor::power::Assertion,
}
impl SupervisorState {
    fn new(loaded: Result<Intent>) -> Self {
        let (intent, lifecycle) = match loaded {
            Ok(intent) => {
                let lifecycle = if intent.mode == Mode::Paused {
                    Lifecycle::Paused
                } else {
                    Lifecycle::Starting
                };
                (intent, lifecycle)
            }
            Err(error) => (
                Intent {
                    mode: Mode::Paused,
                    ..Default::default()
                },
                Lifecycle::StateError(format!("{error:#}")),
            ),
        };
        Self {
            intent,
            lifecycle,
            generation: 0,
            restarts: 0,
            failures: 0,
            power: Default::default(),
        }
    }
    fn snapshot(&self) -> Status {
        let (phase, worker_pid, error) = match &self.lifecycle {
            Lifecycle::Paused => ("paused", None, None),
            Lifecycle::StateError(error) => ("state_error", None, Some(error.clone())),
            Lifecycle::Starting => ("starting", None, None),
            Lifecycle::Running(worker) => ("worker_started", Some(worker.group as u32), None),
            Lifecycle::Stopping { pid } => ("stopping", Some(*pid), None),
            Lifecycle::CleanupFailed { worker, error, .. } => {
                ("stopping", Some(worker.group as u32), Some(error.clone()))
            }
            Lifecycle::Backoff { error, .. } => ("recovering", None, Some(error.clone())),
        };
        Status {
            api_version: 1,
            supervisor_pid: std::process::id(),
            worker_pid,
            mode: self.intent.mode,
            revision: self.intent.revision,
            worker_generation: self.generation,
            restarts: self.restarts,
            phase: phase.into(),
            error,
        }
    }
    fn needs_power(&self) -> bool {
        self.intent.mode == Mode::Running
            || matches!(
                self.lifecycle,
                Lifecycle::Running(_)
                    | Lifecycle::Stopping { .. }
                    | Lifecycle::CleanupFailed { .. }
            )
    }
    fn publish(&mut self, snapshots: &watch::Sender<Status>) {
        // Keep the machine awake through backoff and until a paused worker has
        // actually stopped. Interface lifetime has no part in this ownership.
        let previous = self.power.error.clone();
        self.power.set(self.needs_power());
        if self.power.error != previous
            && let Some(error) = &self.power.error
        {
            eprintln!("{error}");
        }
        snapshots.send_replace(self.snapshot());
    }
    async fn retire(
        &mut self,
        snapshots: &watch::Sender<Status>,
    ) -> Result<Option<(Duration, std::process::ExitStatus)>> {
        if !matches!(
            self.lifecycle,
            Lifecycle::Running(_) | Lifecycle::CleanupFailed { .. }
        ) {
            return Ok(None);
        }
        let mut worker = match std::mem::replace(&mut self.lifecycle, Lifecycle::Starting) {
            Lifecycle::Running(worker) | Lifecycle::CleanupFailed { worker, .. } => worker,
            _ => unreachable!(),
        };
        let elapsed = worker.started.elapsed();
        self.lifecycle = Lifecycle::Stopping {
            pid: worker.group as u32,
        };
        self.publish(snapshots);
        let exit = match worker.stop().await {
            Ok(exit) => exit,
            Err(error) => {
                // Keep the child handle and reserved process-group identity:
                // neither durable Pause nor a retry grants retirement evidence.
                self.lifecycle = Lifecycle::CleanupFailed {
                    worker,
                    next: Instant::now() + Duration::from_secs(120),
                    error: format!("Worker cleanup failed: {error:#}"),
                };
                self.publish(snapshots);
                return Err(error);
            }
        };
        self.lifecycle = if self.intent.mode == Mode::Paused {
            Lifecycle::Paused
        } else {
            Lifecycle::Starting
        };
        self.publish(snapshots);
        Ok(Some((elapsed, exit)))
    }
    async fn control(
        &mut self,
        paths: &Paths,
        request: Request,
        snapshots: &watch::Sender<Status>,
    ) -> Result<()> {
        if let Some(revision) = request.expected_revision {
            ensure!(
                revision == self.intent.revision,
                "Run/Pause state changed; read the current revision"
            );
        }
        match request.command {
            Control::Pause | Control::Resume => {
                let mode = if matches!(request.command, Control::Pause) {
                    Mode::Paused
                } else {
                    Mode::Running
                };
                let resuming = mode == Mode::Running && self.intent.mode == Mode::Paused;
                if mode != self.intent.mode || matches!(self.lifecycle, Lifecycle::StateError(_)) {
                    let updated = Intent {
                        mode,
                        revision: self
                            .intent
                            .revision
                            .checked_add(1)
                            .context("Intent revision exhausted")?,
                        schema_version: 1,
                    };
                    paths.save_intent(&updated)?;
                    self.intent = updated;
                }
                if mode == Mode::Paused {
                    self.retire(snapshots).await?;
                    self.lifecycle = Lifecycle::Paused;
                } else {
                    if matches!(self.lifecycle, Lifecycle::CleanupFailed { .. }) {
                        self.retire(snapshots).await?;
                    }
                    if resuming
                        || matches!(self.lifecycle, Lifecycle::Paused | Lifecycle::StateError(_))
                    {
                        self.lifecycle = Lifecycle::Starting;
                    }
                }
            }
            Control::Reload => {
                self.retire(snapshots).await?;
                if !matches!(self.lifecycle, Lifecycle::StateError(_)) {
                    self.lifecycle = if self.intent.mode == Mode::Paused {
                        Lifecycle::Paused
                    } else {
                        Lifecycle::Starting
                    };
                }
            }
            Control::Status => {}
        }
        self.publish(snapshots);
        Ok(())
    }
    async fn tick(
        &mut self,
        helper: &Path,
        config: &Path,
        snapshots: &watch::Sender<Status>,
    ) -> Result<()> {
        if matches!(self.lifecycle, Lifecycle::CleanupFailed { next, .. } if Instant::now() >= next)
        {
            // Cleanup failure is recoverable without relinquishing ownership or
            // exiting the supervisor (which would release its sleep assertion).
            let _ = self.retire(snapshots).await;
            return Ok(());
        }
        if matches!(&self.lifecycle, Lifecycle::Running(worker) if worker.exited()?) {
            let (elapsed, exit) = match self.retire(snapshots).await {
                Ok(retired) => retired.expect("exited worker"),
                Err(_) => return Ok(()), // CleanupFailed retains ownership and retries.
            };
            let delay = retry_delay(&mut self.failures, Some(elapsed));
            self.restarts += 1;
            self.lifecycle = Lifecycle::Backoff {
                next: Instant::now() + Duration::from_secs(delay),
                error: format!("Worker exited: {exit}; retry in {delay}s"),
            };
        }
        let ready = matches!(self.lifecycle, Lifecycle::Starting)
            || matches!(self.lifecycle, Lifecycle::Backoff { next, .. } if Instant::now() >= next);
        if self.intent.mode == Mode::Running && ready {
            self.lifecycle = match Worker::spawn(helper, config) {
                Ok(worker) => {
                    self.generation += 1;
                    Lifecycle::Running(worker)
                }
                Err(error) => {
                    let delay = retry_delay(&mut self.failures, None);
                    Lifecycle::Backoff {
                        next: Instant::now() + Duration::from_secs(delay),
                        error: format!("{error:#}"),
                    }
                }
            };
        }
        self.publish(snapshots);
        Ok(())
    }
}
async fn run(paths: Paths, helper: PathBuf) -> Result<()> {
    let _owner = InstanceLock::acquire(&paths.lock)?;
    if let Ok(meta) = fs::symlink_metadata(&paths.socket) {
        ensure!(
            meta.file_type().is_socket() && meta.uid() == unsafe { libc::geteuid() },
            "Control path is not an owned socket"
        );
        fs::remove_file(&paths.socket)?;
    }
    let listener = UnixListener::bind(&paths.socket)?;
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
    let _socket = SocketGuard {
        path: paths.socket.clone(),
        inode: fs::symlink_metadata(&paths.socket)?.ino(),
    };
    let mut state = SupervisorState::new(paths.load_intent());
    let (snapshots, current) = watch::channel(state.snapshot());
    state.publish(&snapshots);
    let (tx, mut rx) = mpsc::channel::<Envelope>(16);
    // Accept status readers independently of graceful worker cleanup.
    let mut controls = tokio::spawn(accept_controls(
        listener,
        current,
        tx,
        #[cfg(test)]
        Default::default(),
    ));
    let mut timer = tokio::time::interval(Duration::from_millis(100));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let stop = signal();
    tokio::pin!(stop);
    let result: Result<()> = async {
        loop {
            tokio::select! {
                _ = &mut stop => break,
                result = &mut controls => { result??; break; },
                Some(envelope) = rx.recv() => {
                    let error = state.control(&paths, envelope.request, &snapshots).await
                        .err().map(|error| format!("{error:#}"));
                    let _ = envelope.reply.send(Response { status: state.snapshot(), error });
                },
                _ = timer.tick() => state.tick(&helper, &paths.config, &snapshots).await?,
            }
        }
        Ok(())
    }
    .await;
    // Shutdown never changes durable intent. Retire the family while retaining
    // its sleep assertion, then RAII releases power protection on loop exit.
    let cleanup = state.retire(&snapshots).await;
    controls.abort();
    // A completed accept task has already been consumed by select above.
    if !controls.is_finished() {
        let _ = controls.await;
    }
    cleanup?;
    result
}

#[cfg(test)]
mod tests;

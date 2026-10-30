mod monitor;
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    fs,
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
    net::{UnixDatagram, UnixListener, UnixStream},
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
        #[arg(long)]
        monitor_listen: Option<std::net::SocketAddr>,
        #[arg(long, default_value="mainnet",value_parser=["mainnet","testnet11"])]
        network: String,
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
        #[arg(long)]
        monitor_listen: Option<std::net::SocketAddr>,
        #[arg(long, default_value="mainnet",value_parser=["mainnet","testnet11"])]
        network: String,
    },
    Status {
        #[arg(long)]
        config: PathBuf,
    },
    /// Bounded status and file metadata; excludes configuration, keys and log contents.
    Diagnostics {
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
        Action::LaunchdPlist {
            app,
            config,
            user,
            monitor_listen,
            network,
        } => {
            print!(
                "{}",
                timelord_supervisor::launchd_plist_with_monitor(
                    &app,
                    &config,
                    &user,
                    monitor_listen,
                    &network
                )?
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
        Action::Run {
            config,
            worker,
            monitor_listen,
            network,
        } => {
            let helper =
                worker.unwrap_or(std::env::current_exe()?.with_file_name("timelord-service"));
            run(Paths::for_config(&config)?, helper, monitor_listen, network).await
        }
        Action::Diagnostics { config } => {
            let paths = Paths::for_config(&config)?;
            let result = timelord_supervisor::request(&config, Control::Status, None).await;
            let files=[("configuration",paths.config.clone()),("last_good",paths.directory.join("last-good.toml")),("worker_log",paths.directory.join("worker.log")),("safety",paths.directory.join("board-safety.json"))].map(|(name,path)|{
                let meta=fs::symlink_metadata(path).ok();
                serde_json::json!({"name":name,"present":meta.is_some(),"regular_file":meta.as_ref().is_some_and(|m|m.is_file()),"bytes":meta.map(|m|m.len())})
            });
            println!(
                "{}",
                serde_json::json!({"schema_version":1,"version":env!("CARGO_PKG_VERSION"),"supervisor_available":result.is_ok(),"status":result.ok(),"files":files})
            );
            Ok(())
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
                Action::Run { .. }
                | Action::LaunchdPlist { .. }
                | Action::CheckOwner { .. }
                | Action::Diagnostics { .. } => {
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
    log_tasks: Vec<tokio::task::JoinHandle<()>>,
    child: Child,
    group: i32,
    watchdog: timelord_supervisor::progress::Watchdog,
    started: Instant,
    progress: Option<(timelord_supervisor::progress::Snapshot, std::time::Instant)>,
    #[cfg(test)]
    stop_failures: usize,
}
impl Worker {
    fn spawn(
        helper: &Path,
        config: &Path,
        progress: &Path,
        generation: u64,
        log: &timelord_supervisor::logging::Writer,
    ) -> Result<Self> {
        let mut child = Command::new(helper)
            .arg("serve")
            .arg("--supervised")
            .arg("--config")
            .arg(config)
            .env("TIMELORD_SUPERVISOR_PROGRESS", progress)
            .env("TIMELORD_SUPERVISOR_GENERATION", generation.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .context("Starting timelord worker")?;
        let group = child.id().context("Worker has no process id")? as i32;
        async fn pump(
            mut source: impl tokio::io::AsyncRead + Unpin,
            mut target: timelord_supervisor::logging::Writer,
        ) {
            use std::io::Write;
            let mut bytes = [0u8; 4096];
            while let Ok(size) = source.read(&mut bytes).await {
                if size == 0 {
                    break;
                }
                let _ = target.write_all(&bytes[..size]);
            }
        }
        let log_tasks = vec![
            tokio::spawn(pump(child.stdout.take().expect("stdout"), log.clone())),
            tokio::spawn(pump(child.stderr.take().expect("stderr"), log.clone())),
        ];
        Ok(Self {
            log_tasks,
            child,
            group,
            watchdog: timelord_supervisor::progress::Watchdog::new(std::time::Instant::now()),
            started: Instant::now(),
            progress: None,
            #[cfg(test)]
            stop_failures: 0,
        })
    }
    fn signal(&self, signal: i32) {
        unsafe {
            libc::kill(-self.group, signal);
        };
    }
    fn exited(&self) -> Result<bool> {
        Self::group_exited(self.group)
    }
    fn group_exited(group: i32) -> Result<bool> {
        // Observe exit without reaping. Keeping the leader's PID reserved until
        // descendants are terminated prevents signaling a reused process group.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                group as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
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
    async fn stop(&mut self) -> Result<std::process::ExitStatus> {
        #[cfg(test)]
        if self.stop_failures > 0 {
            self.stop_failures -= 1;
            anyhow::bail!("Injected worker cleanup failure");
        }
        // Let the coordinator close native control channels and drain proof
        // work first. The group-wide hard stop remains the bounded fallback.
        unsafe {
            libc::kill(self.group, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.exited()? && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.signal(libc::SIGKILL);
        let result = tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .context("Worker did not exit after SIGKILL")??;
        self.group = 0;
        for task in &mut self.log_tasks {
            if tokio::time::timeout(Duration::from_secs(1), &mut *task)
                .await
                .is_err()
            {
                task.abort();
            }
        }
        Ok(result)
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        for task in &self.log_tasks {
            task.abort();
        }
        if self.group > 0 {
            self.signal(libc::SIGKILL);
        }
    }
}
async fn validate_before_reload(helper: &Path, config: &Path) -> Result<()> {
    let child = Command::new(helper)
        .arg("validate")
        .arg("--supervised")
        .arg("--config")
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .context("Starting configuration validation")?;
    let group = child.id().context("Validator has no PID")? as i32;
    let mut owner = Worker {
        child,
        group,
        log_tasks: Vec::new(),
        progress: None,
        started: Instant::now(),
        watchdog: timelord_supervisor::progress::Watchdog::new(std::time::Instant::now()),
    };
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        while !owner.exited()? {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await;
    owner.signal(libc::SIGKILL);
    let status = tokio::time::timeout(Duration::from_secs(3), owner.child.wait())
        .await
        .context("Configuration validator did not retire")??;
    owner.group = 0;
    completed.context("Configuration validation timed out; running worker retained")??;
    ensure!(
        status.success(),
        "Configuration validation failed; running worker retained. Run timelord-service validate --config to inspect the requested file."
    );
    Ok(())
}
struct Envelope {
    request: Request,
    reply: oneshot::Sender<Response>,
}
/// Monotonic receipt/publication times stay internal; public JSON remains stable.
#[derive(Clone)]
struct Published {
    status: Status,
    published_at: std::time::Instant,
    progress_received_at: Option<std::time::Instant>,
}
impl Published {
    fn status_at(&self, now: std::time::Instant) -> Status {
        let mut status = self.status.clone();
        status.progress_age_seconds = self
            .progress_received_at
            .map(|received| now.saturating_duration_since(received).as_secs());
        status
    }
    fn healthy(&self, now: std::time::Instant) -> bool {
        now.saturating_duration_since(self.published_at) <= Duration::from_secs(30)
    }
    fn ready(&self, now: std::time::Instant) -> bool {
        self.healthy(now)
            && self.progress_received_at.is_some_and(|received| {
                now.saturating_duration_since(received) <= Duration::from_secs(3)
            })
            && self.status.mode == Mode::Running
            && self.status.phase == "worker_started"
            && self.status.progress.as_ref().is_some_and(|progress| {
                Some(progress.worker_pid) == self.status.worker_pid
                    && progress.generation == self.status.worker_generation
                    && progress.ready
                    && !progress.failed
            })
    }
}
type Snapshot = watch::Receiver<Published>;
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
            status: state.borrow().status_at(std::time::Instant::now()),
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
) -> Result<()> {
    let mut handlers = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = handlers.join_next(), if !handlers.is_empty() => {},
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
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
    Running(Box<Worker>),
    Stopping {
        pid: u32,
    },
    CleanupFailed {
        worker: Box<Worker>,
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
    recoveries: Vec<timelord_supervisor::Recovery>,
    log: timelord_supervisor::logging::Writer,
    monitor_error: Option<String>,
}
impl SupervisorState {
    fn new(loaded: Result<Intent>, log: timelord_supervisor::logging::Writer) -> Self {
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
            recoveries: Vec::new(),
            log,
            monitor_error: None,
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
        let progress = match &self.lifecycle {
            Lifecycle::Running(worker) | Lifecycle::CleanupFailed { worker, .. } => {
                worker.progress.as_ref()
            }
            _ => None,
        };
        Status {
            logging: self.log.stats(),
            sleep_inhibited: self.power.active(),
            sleep_error: self.power.error.clone(),
            api_version: 1,
            supervisor_pid: std::process::id(),
            worker_pid,
            mode: self.intent.mode,
            revision: self.intent.revision,
            worker_generation: self.generation,
            restarts: self.restarts,
            phase: phase.into(),
            error: error.or_else(|| self.monitor_error.clone()),
            progress: progress.map(|(snapshot, _)| snapshot.clone()),
            progress_age_seconds: progress.map(|(_, received)| received.elapsed().as_secs()),
            recoveries: self.recoveries.clone(),
        }
    }
    fn published(&self) -> Published {
        Published {
            status: self.snapshot(),
            published_at: std::time::Instant::now(),
            progress_received_at: match &self.lifecycle {
                Lifecycle::Running(worker) | Lifecycle::CleanupFailed { worker, .. } => {
                    worker.progress.as_ref().map(|(_, received)| *received)
                }
                _ => None,
            },
        }
    }
    fn observe(&mut self, snapshot: timelord_supervisor::progress::Snapshot) {
        if let Lifecycle::Running(worker) | Lifecycle::CleanupFailed { worker, .. } =
            &mut self.lifecycle
            && snapshot.worker_pid == worker.group as u32
            && snapshot.generation == self.generation
        {
            let now = std::time::Instant::now();
            if worker.watchdog.observe(snapshot.clone(), now) {
                worker.progress = Some((snapshot, now));
            }
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
    fn publish(&mut self, snapshots: &watch::Sender<Published>) {
        // Keep the machine awake through backoff and until a paused worker has
        // actually stopped. Interface lifetime has no part in this ownership.
        let previous = self.power.error.clone();
        self.power.set(self.needs_power());
        if self.power.error != previous
            && let Some(error) = &self.power.error
        {
            use std::io::Write;
            let _ = writeln!(&mut self.log, "{error}");
        }
        snapshots.send_replace(self.published());
    }
    async fn retire(
        &mut self,
        snapshots: &watch::Sender<Published>,
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
        let exit = match (*worker).stop().await {
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
        helper: &Path,
        request: Request,
        snapshots: &watch::Sender<Published>,
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
                validate_before_reload(helper, &paths.config).await?;
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
    fn record_recovery(&mut self, worker_pid: Option<u32>, reason: &str) {
        self.restarts += 1;
        self.recoveries.push(timelord_supervisor::Recovery {
            timestamp_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            worker_pid,
            reason: reason.chars().take(512).collect(),
        });
        if self.recoveries.len() > 32 {
            self.recoveries.remove(0);
        }
    }
    async fn tick(
        &mut self,
        helper: &Path,
        config: &Path,
        progress_path: &Path,
        snapshots: &watch::Sender<Published>,
    ) -> Result<()> {
        if matches!(self.lifecycle, Lifecycle::CleanupFailed { next, .. } if Instant::now() >= next)
        {
            // Keep failed cleanup owned and monitored until retirement succeeds.
            let _ = self.retire(snapshots).await;
            return Ok(());
        }
        let watchdog_failure = match &self.lifecycle {
            Lifecycle::Running(worker) => worker.watchdog.failure(std::time::Instant::now()),
            _ => None,
        };
        if watchdog_failure.is_some()
            || matches!(&self.lifecycle, Lifecycle::Running(worker) if worker.exited()?)
        {
            let previous_pid = self.snapshot().worker_pid;
            let (elapsed, exit) = match self.retire(snapshots).await {
                Ok(retired) => retired.expect("failed worker"),
                Err(error) => {
                    let reason = watchdog_failure
                        .unwrap_or_else(|| format!("Worker exited; cleanup failed: {error:#}"));
                    self.record_recovery(previous_pid, &reason);
                    self.publish(snapshots);
                    return Ok(()); // CleanupFailed retains ownership and retries.
                }
            };
            let delay = retry_delay(&mut self.failures, Some(elapsed));
            let reason = watchdog_failure.unwrap_or_else(|| format!("Worker exited: {exit}"));
            self.record_recovery(previous_pid, &reason);
            self.lifecycle = Lifecycle::Backoff {
                next: Instant::now() + Duration::from_secs(delay),
                error: format!("{reason}; retry in {delay}s"),
            };
        }
        let ready = matches!(self.lifecycle, Lifecycle::Starting)
            || matches!(self.lifecycle, Lifecycle::Backoff { next, .. } if Instant::now() >= next);
        if self.intent.mode == Mode::Running && ready {
            self.lifecycle = match Worker::spawn(
                helper,
                config,
                progress_path,
                self.generation + 1,
                &self.log,
            ) {
                Ok(worker) => {
                    self.generation += 1;
                    Lifecycle::Running(Box::new(worker))
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
async fn monitoring_exit(
    monitor: &mut Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    state: &mut SupervisorState,
    snapshots: &watch::Sender<Published>,
) {
    let result = monitor.as_mut().expect("monitor").await;
    monitor.take();
    state.monitor_error = Some(
        format!("Supervisor monitoring exited unexpectedly: {result:?}")
            .chars()
            .take(512)
            .collect(),
    );
    eprintln!("{}", state.monitor_error.as_deref().expect("monitor error"));
    state.publish(snapshots);
}

async fn run(
    paths: Paths,
    helper: PathBuf,
    monitor_listen: Option<std::net::SocketAddr>,
    network: String,
) -> Result<()> {
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
    let progress_path = paths.directory.join("progress.sock");
    if let Ok(meta) = fs::symlink_metadata(&progress_path) {
        ensure!(
            meta.file_type().is_socket() && meta.uid() == unsafe { libc::geteuid() },
            "Invalid progress socket owner"
        );
        fs::remove_file(&progress_path)?;
    }
    let progress = UnixDatagram::bind(&progress_path)?;
    fs::set_permissions(&progress_path, fs::Permissions::from_mode(0o600))?;
    let _progress_guard = SocketGuard {
        path: progress_path.clone(),
        inode: fs::symlink_metadata(&progress_path)?.ino(),
    };
    let mut buffer = [0u8; 8192];
    let log = timelord_supervisor::logging::Writer::rotating(paths.directory.join("worker.log"));
    let mut state = SupervisorState::new(paths.load_intent(), log);
    let (snapshots, current) = watch::channel(state.published());
    state.publish(&snapshots);
    let mut monitor = if let Some(address) = monitor_listen {
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .context("Binding supervisor monitoring")?;
        let app = monitor::routes(current.clone(), network);
        Some(tokio::spawn(async move {
            axum::serve(
                timelord_supervisor::http::Listener::new(listener),
                timelord_supervisor::http::protect(app),
            )
            .await
        }))
    } else {
        None
    };
    let (tx, mut rx) = mpsc::channel::<Envelope>(16);
    // Accept status readers independently of graceful worker cleanup.
    let mut controls = tokio::spawn(accept_controls(listener, current, tx));
    let mut timer = tokio::time::interval(Duration::from_millis(100));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let stop = signal();
    tokio::pin!(stop);
    let result: Result<()> = async {
        loop {
            tokio::select! {
                _ = &mut stop => break,
                _ = monitoring_exit(&mut monitor, &mut state, &snapshots), if monitor.is_some() => {},
                received = progress.recv(&mut buffer) => {
                    let size = received.context("Reading worker progress")?;
                    if size < buffer.len()
                        && let Ok(report) = serde_json::from_slice::<timelord_supervisor::progress::Snapshot>(&buffer[..size]) {
                        state.observe(report);
                        state.publish(&snapshots);
                    }
                },
                result = &mut controls => { result??; break; },
                Some(envelope) = rx.recv() => {
                    let error = state.control(&paths, &helper, envelope.request, &snapshots).await
                        .err().map(|error| format!("{error:#}"));
                    let _ = envelope.reply.send(Response { status: state.snapshot(), error });
                },
                _ = timer.tick() => state.tick(&helper, &paths.config, &progress_path, &snapshots).await?,
            }
        }
        Ok(())
    }
    .await;
    // Shutdown never changes durable intent. Retire the family while retaining
    // its sleep assertion, then RAII releases power protection on loop exit.
    let cleanup = state.retire(&snapshots).await;
    if let Some(monitor) = monitor {
        monitor.abort();
    }
    controls.abort();
    // A completed accept task has already been consumed by select above.
    if !controls.is_finished() {
        let _ = controls.await;
    }
    cleanup?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn failed_pause_retains_owner_and_power_until_confirmed_cleanup() {
        let dir = std::env::temp_dir().join(format!("tl-pause-owner-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let helper = dir.join("worker");
        fs::write(&helper, "#!/bin/sh\ncat >/dev/null\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let paths = Paths::for_config(&dir.join("config.toml")).unwrap();
        for resume in [false, true] {
            let mut worker =
                Worker::spawn(&helper, &paths.config, &dir.join("progress.sock"), 1).unwrap();
            let pid = worker.group;
            worker.stop_failures = if resume { 2 } else { 1 };
            let mut state = SupervisorState::new(Ok(Intent::default()));
            state.lifecycle = Lifecycle::Running(Box::new(worker));
            let (snapshots, _) = watch::channel(state.published());
            let request = |command| Request {
                command,
                expected_revision: None,
            };
            assert!(
                state
                    .control(&paths, request(Control::Pause), &snapshots)
                    .await
                    .is_err()
            );
            assert_eq!(paths.load_intent().unwrap().mode, Mode::Paused);
            assert!(state.needs_power());
            assert_eq!(state.snapshot().phase, "stopping");
            assert_eq!(state.snapshot().worker_pid, Some(pid as u32));
            assert!(state.snapshot().error.unwrap().contains("cleanup failed"));
            assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
            // No replacement is admitted while the cleanup retry is pending.
            state
                .tick(
                    &helper,
                    &paths.config,
                    &dir.join("progress.sock"),
                    &snapshots,
                )
                .await
                .unwrap();
            assert_eq!(state.snapshot().worker_pid, Some(pid as u32));
            assert_eq!(state.generation, 0);
            if resume {
                assert!(
                    state
                        .control(&paths, request(Control::Resume), &snapshots)
                        .await
                        .is_err()
                );
                assert_eq!(state.intent.mode, Mode::Running);
                assert_eq!(state.snapshot().worker_pid, Some(pid as u32));
                assert!(matches!(state.lifecycle, Lifecycle::CleanupFailed { .. }));
                state
                    .control(&paths, request(Control::Pause), &snapshots)
                    .await
                    .unwrap();
            } else {
                let Lifecycle::CleanupFailed { next, .. } = &mut state.lifecycle else {
                    panic!("cleanup ownership lost")
                };
                *next = Instant::now();
                state
                    .tick(
                        &helper,
                        &paths.config,
                        &dir.join("progress.sock"),
                        &snapshots,
                    )
                    .await
                    .unwrap();
            }
            assert!(matches!(state.lifecycle, Lifecycle::Paused));
            assert!(!state.needs_power());
            assert_eq!(state.snapshot().worker_pid, None);
            assert_eq!(state.generation, 0);
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn explicit_resume_retries_paused_start_backoff_but_repeated_resume_does_not() {
        let dir = std::env::temp_dir().join(format!("tl-resume-cleanup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths::for_config(&dir.join("config.toml")).unwrap();
        let mut state = SupervisorState::new(
            Ok(Intent {
                mode: Mode::Paused,
                ..Default::default()
            }),
            timelord_supervisor::logging::Writer::stderr(),
        );
        state.lifecycle = Lifecycle::Backoff {
            next: Instant::now() + Duration::from_secs(120),
            error: "Worker start failed".into(),
        };
        let (snapshots, _) = watch::channel(state.published());
        let resume = || Request {
            command: Control::Resume,
            expected_revision: None,
        };
        state
            .control(&paths, Path::new("/unused/worker"), resume(), &snapshots)
            .await
            .unwrap();
        assert_eq!(state.intent.mode, Mode::Running);
        assert!(matches!(state.lifecycle, Lifecycle::Starting));
        let revision = state.intent.revision;
        state.lifecycle = Lifecycle::Backoff {
            next: Instant::now() + Duration::from_secs(120),
            error: "Worker crashed".into(),
        };
        state
            .control(&paths, Path::new("/unused/worker"), resume(), &snapshots)
            .await
            .unwrap();
        assert!(matches!(state.lifecycle, Lifecycle::Backoff { .. }));
        assert_eq!(state.intent.revision, revision);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn initial_status_projects_durable_pause_and_invalid_state() {
        let paused = SupervisorState::new(
            Ok(Intent {
                mode: Mode::Paused,
                revision: 17,
                ..Default::default()
            }),
            timelord_supervisor::logging::Writer::stderr(),
        );
        let status = paused.snapshot();
        assert_eq!(status.mode, Mode::Paused);
        assert_eq!(status.phase, "paused");
        assert_eq!(status.revision, 17);
        assert_eq!(status.worker_pid, None);
        assert!(!paused.needs_power());
        let invalid = SupervisorState::new(
            Err(anyhow::anyhow!("corrupt fixture")),
            timelord_supervisor::logging::Writer::stderr(),
        );
        assert_eq!(invalid.snapshot().phase, "state_error");
        assert!(invalid.snapshot().error.is_some());
        assert!(!invalid.needs_power());
    }
    #[test]
    fn power_follows_requested_run_through_recovery_and_actual_pause_cleanup() {
        let mut state = SupervisorState::new(
            Ok(Intent::default()),
            timelord_supervisor::logging::Writer::stderr(),
        );
        assert!(state.needs_power());
        state.lifecycle = Lifecycle::Backoff {
            next: Instant::now(),
            error: "fixture outage".into(),
        };
        assert!(state.needs_power());
        assert_eq!(state.snapshot().phase, "recovering");
        state.intent.mode = Mode::Paused;
        state.lifecycle = Lifecycle::Stopping { pid: 123 };
        assert!(state.needs_power());
        assert_eq!(state.snapshot().worker_pid, Some(123));
        assert_eq!(state.snapshot().phase, "stopping");
        state.lifecycle = Lifecycle::Paused;
        assert!(!state.needs_power());
        assert_eq!(state.snapshot().worker_pid, None);
    }
    #[test]
    fn stable_worker_resets_crash_streak_but_short_crashes_keep_backoff() {
        let mut failures = 0;
        for expected in [1, 5, 30, 60, 120, 120] {
            assert_eq!(
                retry_delay(&mut failures, Some(Duration::from_secs(1))),
                expected
            );
        }
        assert_eq!(retry_delay(&mut failures, Some(Duration::from_secs(60))), 1);
        assert_eq!(retry_delay(&mut failures, None), 5);
    }
}

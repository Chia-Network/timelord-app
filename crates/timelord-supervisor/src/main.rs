mod monitor;
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use timelord_supervisor::{
    Control, InstanceLock, Intent, Mode, Paths, Request, Response, Status, peer_is_owner,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixDatagram, UnixListener, UnixStream},
    process::{Child, Command},
    sync::{mpsc, oneshot},
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
            let _owner = InstanceLock::acquire(&paths.worker_lock)?;
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
struct Worker {
    child: Child,
    group: i32,
    watchdog: timelord_supervisor::progress::Watchdog,
}
impl Worker {
    fn spawn(helper: &Path, config: &Path, progress: &Path, generation: u64) -> Result<Self> {
        let child = Command::new(helper)
            .arg("serve")
            .arg("--supervised")
            .arg("--config")
            .arg(config)
            .env("TIMELORD_SUPERVISOR_PROGRESS", progress)
            .env("TIMELORD_SUPERVISOR_GENERATION", generation.to_string())
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
            watchdog: timelord_supervisor::progress::Watchdog::new(std::time::Instant::now()),
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
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.group as libc::id_t,
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
    async fn stop(mut self) -> Result<std::process::ExitStatus> {
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
type Snapshot = Arc<Mutex<Status>>;
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
            status: state.lock().expect("status").clone(),
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
async fn signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}}
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
    let mut buffer = [0u8; 4096];
    let loaded = paths.load_intent();
    let (mut intent, error) = match loaded {
        Ok(intent) => (intent, None),
        Err(error) => (
            Intent {
                mode: Mode::Paused,
                ..Default::default()
            },
            Some(format!("{error:#}")),
        ),
    };
    let state = Arc::new(Mutex::new(Status {
        api_version: 1,
        supervisor_pid: std::process::id(),
        worker_pid: None,
        mode: intent.mode,
        revision: intent.revision,
        worker_generation: 0,
        restarts: 0,
        phase: if error.is_some() {
            "state_error"
        } else {
            "starting"
        }
        .into(),
        error,
        progress: None,
        progress_age_seconds: None,
        recoveries: Vec::new(),
    }));
    let tick = Arc::new(Mutex::new(std::time::Instant::now()));
    let mut monitor = if let Some(address) = monitor_listen {
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .context("Binding supervisor monitoring")?;
        let app = monitor::routes(state.clone(), tick.clone(), network);
        Some(tokio::spawn(
            async move { axum::serve(listener, app).await },
        ))
    } else {
        None
    };
    let (tx, mut rx) = mpsc::channel::<Envelope>(16);
    let mut handlers = JoinSet::new();
    let mut worker: Option<Worker> = None;
    let mut next = Instant::now();
    let mut failures = 0usize;
    let mut timer = tokio::time::interval(Duration::from_millis(100));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let stop = signal();
    tokio::pin!(stop);
    let result: Result<()>=async {loop {
        tokio::select! {
            _=&mut stop=>break,
            result=async {monitor.as_mut().expect("monitor").await}, if monitor.is_some()=>{
                result.context("Supervisor monitoring task failed")??;
                anyhow::bail!("Supervisor monitoring exited unexpectedly");
            },
            Some(_) = handlers.join_next(), if !handlers.is_empty()=>{},
            accepted=listener.accept()=>{
                let(socket,_)=accepted?;
                if handlers.len()<16 {
                    let(state,tx)=(state.clone(),tx.clone());
                    handlers.spawn(async move { let _=handle(socket,state,tx).await; });
                }
            },
            received=progress.recv(&mut buffer)=>{
                let size=received.context("Reading worker progress")?;
                let generation=state.lock().expect("status").worker_generation;
                if size < buffer.len()
                    && let Ok(snapshot)=serde_json::from_slice::<timelord_supervisor::progress::Snapshot>(&buffer[..size])
                    && let Some(child)=&mut worker
                    && Some(snapshot.worker_pid)==child.child.id()
                    && snapshot.generation==generation
                    && child.watchdog.observe(snapshot.clone(),std::time::Instant::now()) {
                    state.lock().expect("status").progress=Some(snapshot);
                }
            },
            Some(envelope)=rx.recv()=>{
                let update: Result<()>=async {
                    if let Some(revision)=envelope.request.expected_revision {
                        ensure!(revision==intent.revision,"Run/Pause state changed; read the current revision");
                    }
                    match envelope.request.command {
                        Control::Pause|Control::Resume=>{
                            let mode=if matches!(envelope.request.command,Control::Pause) {Mode::Paused} else {Mode::Running};
                            if mode!=intent.mode || state.lock().expect("status").error.is_some() {
                                let updated=Intent {mode,revision:intent.revision.checked_add(1).context("Intent revision exhausted")?,schema_version:1};
                                paths.save_intent(&updated)?;
                                intent=updated;
                            }
                            let mut status=state.lock().expect("status");
                            status.mode=intent.mode;status.revision=intent.revision;status.error=None;
                        },
                        Control::Reload|Control::Status=>{},
                    }
                    if intent.mode==Mode::Paused || matches!(envelope.request.command,Control::Reload) {
                        state.lock().expect("status").phase="stopping".into();
                        if let Some(child)=worker.take() {child.stop().await?;}
                        let mut status=state.lock().expect("status");status.worker_pid=None;status.progress=None;status.progress_age_seconds=None;
                        status.phase=if intent.mode==Mode::Paused {"paused"} else {"starting"}.into();
                        next=Instant::now();
                    }
                    Ok(())
                }.await;
                let error=update.err().map(|e|format!("{e:#}"));
                let _=envelope.reply.send(Response {status:state.lock().expect("status").clone(),error});
            },
            _=timer.tick()=>{
                *tick.lock().expect("supervisor tick")=std::time::Instant::now();
                let watchdog_failure=worker.as_ref().and_then(|w|w.watchdog.failure(std::time::Instant::now()));
                state.lock().expect("status").progress_age_seconds=worker.as_ref().and_then(|w|w.watchdog.age(std::time::Instant::now()));
                if watchdog_failure.is_some() || worker.as_ref().map(Worker::exited).transpose()?.unwrap_or(false) {
                    let previous_pid=worker.as_ref().and_then(|w|w.child.id());
                    let exit=worker.take().expect("worker").stop().await?;
                    let delay=[1,5,30,60,120][failures.min(4)]; failures=failures.saturating_add(1);
                    next=Instant::now()+Duration::from_secs(delay);
                    let mut status=state.lock().expect("status");status.worker_pid=None;status.progress=None;status.progress_age_seconds=None;status.restarts+=1;
                    let reason=watchdog_failure.unwrap_or_else(||format!("Worker exited: {exit}"));
                    status.recoveries.push(timelord_supervisor::Recovery {timestamp_unix:std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),worker_pid:previous_pid,reason:reason.chars().take(512).collect()});
                    if status.recoveries.len()>32 {status.recoveries.remove(0);}
                    status.phase="recovering".into();status.error=Some(format!("{reason}; retry in {delay}s"));
                }
                if worker.is_none() && intent.mode==Mode::Running && Instant::now()>=next {
                    let generation = state.lock().expect("status").worker_generation+1;
                    match Worker::spawn(&helper,&paths.config,&progress_path,generation) {
                        Ok(child)=>{
                            let mut status=state.lock().expect("status");status.worker_pid=child.child.id();status.worker_generation+=1;status.progress=None;status.progress_age_seconds=None;
                            // Process presence is not proof of productive work.
                            status.phase="worker_started".into();status.error=None;worker=Some(child);
                        },
                        Err(error)=>{
                            let delay=[1,5,30,60,120][failures.min(4)];failures=failures.saturating_add(1);
                            next=Instant::now()+Duration::from_secs(delay);
                            let mut status=state.lock().expect("status");status.phase="recovering".into();status.error=Some(format!("{error:#}"));
                        },
                    }
                }
            },
        }
    } Ok(())}.await;
    // Shutdown never changes durable intent. launchd/reboot resumes a requested
    // Run, and never overrides an explicit Pause.
    if let Some(child) = worker {
        child.stop().await?;
    }
    if let Some(monitor) = monitor {
        monitor.abort();
    }
    handlers.abort_all();
    while handlers.join_next().await.is_some() {}
    result
}

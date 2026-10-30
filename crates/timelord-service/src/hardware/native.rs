//! Native process, control transport and telemetry. Board policy lives in board.rs.
use super::*;

// Versioned native telemetry distinguishes a new rate baseline from real idle.
pub(super) fn raw_speed(line: &str) -> Option<(usize, f64, RawState)> {
    let (_, payload) = line.split_once("TLRAW 2 ")?;
    let mut parts = payload.split_whitespace();
    let engine = parts.next()?.parse::<usize>().ok()?;
    let ips = parts.next()?.parse::<f64>().ok()?;
    let state = match parts.next()? {
        "running" if ips >= 0. => RawState::Running,
        "sampling" if ips == -1. => RawState::Sampling,
        "idle" if ips == -1. => RawState::Idle,
        _ => return None,
    };
    (engine < 3 && ips.is_finite() && parts.next().is_none()).then_some((engine, ips, state))
}

pub(super) fn temperature(line: &str) -> Option<(f64, f64)> {
    let (_, value) = line.split_once("ASIC Temp = ")?;
    let (temp, frequency) = value.split_once(" C; Frequency = ")?;
    let (frequency, _) = frequency.split_once(" MHz")?;
    let temperature: f64 = temp.trim().parse().ok()?;
    let frequency: f64 = frequency.trim().parse().ok()?;
    (temperature.is_finite() && frequency.is_finite()).then_some((temperature, frequency))
}
pub(super) async fn terminate(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    if let Some(pid) = child.id() {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .await;
    }
    if tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}
// Discard bounded chunks while retaining just enough tail for a split final
// shutdown record. No subprocess output is collected into an unbounded buffer.
pub(super) async fn drain_shutdown(
    source: &mut (impl tokio::io::AsyncRead + Unpin),
) -> Result<bool> {
    let mut buffer = [0u8; 1024];
    let mut tail = String::new();
    let mut confirmed = false;
    loop {
        let count = source.read(&mut buffer).await?;
        if count == 0 {
            return Ok(confirmed);
        }
        tail.push_str(&String::from_utf8_lossy(&buffer[..count]));
        confirmed |= tail.contains("Hardware shutdown: engines_paused=1 safe_point_restored=1\n");
        if tail.len() > 128 {
            let mut start = tail.len() - 128;
            while !tail.is_char_boundary(start) {
                start += 1;
            }
            tail.drain(..start);
        }
    }
}

// Drain stderr even while control is awaiting its acknowledgement. A recovery
// flood must not fill the pipe and block the USB owner before it reads TLSET.
pub(crate) static INITIALIZATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) struct ClientLog {
    pub(super) lines: tokio::io::Lines<BufReader<tokio::process::ChildStderr>>,
    pub(super) ready: Option<oneshot::Sender<()>>,
    pub(super) initialization: Option<tokio::sync::MutexGuard<'static, ()>>,
    pub(super) preparing: Option<timelord_supervisor::progress::Guard>,
    pub(super) last_temperature: tokio::time::Instant,
    pub(super) recent: std::collections::VecDeque<String>,
}
impl ClientLog {
    pub(super) fn new(stderr: tokio::process::ChildStderr, ready: oneshot::Sender<()>) -> Self {
        Self {
            lines: BufReader::new(stderr).lines(),
            ready: Some(ready),
            initialization: None,
            preparing: None,
            last_temperature: tokio::time::Instant::now(),
            recent: Default::default(),
        }
    }
    pub(super) async fn read(&mut self, shared: &Shared, board: &BoardConfig) -> Result<bool> {
        let Some(line) = self.lines.next_line().await? else {
            bail!(
                "ASIC client log closed; recent log: {}",
                self.recent.iter().cloned().collect::<Vec<_>>().join(" | ")
            );
        };
        self.recent.push_back(line.clone());
        if self.recent.len() > 8 {
            self.recent.pop_front();
        }
        tracing::info!(board=%board.id, "{line}");
        let mut state = shared.runtime.lock().expect("runtime");
        if let Some((engine, ips, activity)) = raw_speed(&line) {
            state
                .boards
                .entry(board.id.clone())
                .or_default()
                .raw_sample(engine, ips, activity, crate::unix_now());
        }
        if let Some(value) = line
            .split_once("Board voltage is now ")
            .and_then(|(_, s)| s.split_whitespace().next())
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
        {
            state.boards.entry(board.id.clone()).or_default().voltage_v = Some(value);
        }
        if line.contains("ASIC Temp = ") && temperature(&line).is_none() {
            bail!("ASIC temperature telemetry unavailable");
        }
        if let Some((temp, freq)) = temperature(&line) {
            if temp <= 0. {
                bail!("ASIC temperature telemetry unavailable");
            }
            self.last_temperature = tokio::time::Instant::now();
            let hardware = state.boards.entry(board.id.clone()).or_default();
            hardware.temperature_unix = crate::unix_now();
            hardware.temperature_samples = hardware.temperature_samples.saturating_add(1);
            hardware.temperature_c = Some(temp);
            hardware.frequency_mhz = Some(freq);
            if temp < board.temperature_limit_c {
                hardware.lifecycle.cool_telemetry();
            }
            if temp >= board.temperature_limit_c {
                let first = !hardware.lifecycle.status().latched_fault;
                hardware.lifecycle.fault(
                    crate::board_lifecycle::Fault::Thermal,
                    "ASIC reached its configured temperature limit".into(),
                    crate::unix_now(),
                );
                state.event(Some(board.id.clone()), "thermal_stop");
                if first && let Some(metrics) = &state.metrics {
                    metrics
                        .thermal_stopped(&crate::observability::board(&shared.config, &board.id));
                }
                // Retire the native owner before attempting fault persistence.
                drop(state);
                bail!("ASIC reached its configured temperature limit");
            }
        }
        let recovery = line.contains("Restarting VDF at ");
        if recovery {
            state.boards.entry(board.id.clone()).or_default().recoveries += 1;
            state.event(Some(board.id.clone()), "recovery");
            if let Some(metrics) = &state.metrics {
                let engine = line
                    .split_once("VDF ")
                    .and_then(|(_, s)| s.split_once(':'))
                    .and_then(|(n, _)| n.parse::<u8>().ok());
                metrics.asic_recovered(
                    &crate::observability::board(&shared.config, &board.id),
                    engine,
                );
            }
        }
        if line.contains("Connected to timelord, waiting for challenge") {
            state
                .boards
                .entry(board.id.clone())
                .or_default()
                .lifecycle
                .initialized();
            self.initialization.take();
            self.preparing.take();
            if let Some(ready) = self.ready.take() {
                let _ = ready.send(());
            }
        }
        Ok(recovery)
    }
}
pub(super) struct NativeOwner {
    pub(super) control: NativeControl,
    pub(super) tuning: Option<Tuning>,
}

pub(super) struct NativeControl {
    pub(super) input: tokio::process::ChildStdin,
    pub(super) output: BufReader<tokio::process::ChildStdout>,
    pub(super) sequence: u64,
    minimum_frequency: f64,
    maximum_frequency: f64,
    minimum_voltage: f64,
}
impl NativeControl {
    fn accepts(&self, point: Point) -> bool {
        point.frequency_mhz.is_finite()
            && point.voltage_v.is_finite()
            && (self.minimum_frequency..=self.maximum_frequency).contains(&point.frequency_mhz)
            && (self.minimum_voltage..=1.).contains(&point.voltage_v)
    }
}

pub(super) fn force_safe(shared: &Shared, board: &BoardConfig) -> bool {
    let safety = shared.runtime.lock().expect("runtime").safety.clone();
    safety
        .zip(board.serial.as_ref())
        .and_then(|(store, serial)| store.board(serial))
        .is_some_and(|b| b.force_safe)
}
pub(super) fn startup_point(
    shared: &Shared,
    board: &BoardConfig,
    engines: usize,
    hash: &str,
    host: &str,
) -> Result<Point> {
    if force_safe(shared, board) {
        return Ok(admission::qualification(shared, board, engines)?.point);
    }
    automatic::initial(&shared.config, board, engines, hash, host)
}
impl NativeOwner {
    pub(super) fn attach(board: &BoardConfig, child: &mut Child) -> Result<Self> {
        let automatic = board.profile == ProfileMode::Automatic;
        let owner = Self {
            control: NativeControl {
                input: child.stdin.take().context("Native control input missing")?,
                output: BufReader::new(
                    child
                        .stdout
                        .take()
                        .context("Native control output missing")?,
                ),
                sequence: 0,
                minimum_frequency: if automatic {
                    board.automatic.min_frequency_mhz
                } else {
                    200.
                },
                maximum_frequency: if automatic {
                    board.automatic.max_frequency_mhz
                } else {
                    2200.
                },
                minimum_voltage: if automatic {
                    board.automatic.min_voltage_v
                } else {
                    0.7
                },
            },
            tuning: None,
        };
        Ok(owner)
    }
    pub(super) async fn initialize(&mut self, shared: &Shared, board: &BoardConfig) -> Result<()> {
        let engines = shared
            .assignments
            .iter()
            .filter(|a| a.board_id == board.id)
            .count();
        let automatic = board.profile == ProfileMode::Automatic;
        let (point, tuning) = if automatic {
            let hash = automatic::fingerprint(&shared.config)?;
            let host = automatic::host();
            let point = startup_point(shared, board, engines, &hash, &host)?;
            let mut policy = Policy::new(
                board.automatic.clone(),
                point,
                crate::unix_now(),
                supported_frequencies(&shared.config).await?,
            );
            if !force_safe(shared, board)
                && let Some(entry) =
                    automatic::learned(&shared.config, board, engines, &hash, &host)?
            {
                policy.restore(&entry);
            }
            let serial = board
                .serial
                .clone()
                .context("Automatic tuning needs a persistent board serial")?;
            let safety = shared.runtime.lock().expect("runtime").safety.clone();
            let journal = match safety {
                Some(store) => {
                    let token = store
                        .owner_token(&serial)
                        .context("Managed native session has no journal owner token")?;
                    Some(Journal { store, token })
                }
                None => None,
            };
            (
                point,
                Some(Tuning {
                    experiment: None,
                    policy,
                    serial,
                    engines,
                    hash,
                    host,
                    raw_window: Default::default(),
                    last_record_unix: crate::unix_now(),
                    journal,
                }),
            )
        } else {
            let (frequency_mhz, voltage_v) = board.operating_point();
            (
                Point {
                    frequency_mhz,
                    voltage_v,
                },
                None,
            )
        };
        self.tuning = tuning;
        let actual = self.control.change(point, 30).await?;
        if let Some(tuning) = &mut self.tuning {
            tuning.policy.applied(
                actual,
                crate::unix_now(),
                "Observing live proofs at the initial setting".into(),
            );
        }
        Ok(())
    }
    pub(super) async fn stop(self, child: &mut Child, log: ClientLog) -> bool {
        self.control.stop(child, log).await
    }
    pub(super) async fn tick(
        &mut self,
        shared: &Shared,
        board: &BoardConfig,
        log: &mut ClientLog,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        match &mut self.tuning {
            Some(tuning) => {
                tuning
                    .tick(&mut self.control, shared, board, log, shutdown)
                    .await
            }
            None => Ok(()),
        }
    }
}
impl NativeControl {
    pub(super) async fn stop(self, child: &mut Child, log: ClientLog) -> bool {
        let Self {
            input, mut output, ..
        } = self;
        drop(input); // EOF asks the USB owner to quiesce before proof threads drain.
        let ClientLog {
            lines,
            initialization,
            ..
        } = log;
        let _initialization = initialization;
        let mut stderr = lines.into_inner();
        let retired = tokio::time::timeout(Duration::from_secs(5), async {
            let mut sink = tokio::io::sink();
            let (exit, _, confirmed) = tokio::join!(
                child.wait(),
                tokio::io::copy(&mut output, &mut sink),
                drain_shutdown(&mut stderr)
            );
            exit.is_ok() && confirmed.unwrap_or(false)
        })
        .await;
        if let Ok(confirmed) = retired {
            return confirmed;
        }
        terminate(child).await;
        false
    }
    pub(super) async fn change(&mut self, target: Point, seconds: u64) -> Result<Point> {
        if !self.accepts(target) {
            bail!("Native request exceeds configured limits");
        }
        self.sequence += 1;
        let sequence = self.sequence;
        tokio::time::timeout(Duration::from_secs(seconds), async {
            self.input
                .write_all(
                    format!(
                        "TLSET 1 {sequence} {:.6} {:.6}\n",
                        target.frequency_mhz, target.voltage_v
                    )
                    .as_bytes(),
                )
                .await?;
            self.input.flush().await?;
            let mut line = String::new();
            for _ in 0..16 {
                line.clear();
                if (&mut self.output).take(513).read_line(&mut line).await? == 0 {
                    bail!("Native control closed before acknowledgement");
                }
                if line.len() > 512 {
                    bail!("Native control response too large");
                }
                if line.starts_with("TLERR ") {
                    let parts: Vec<_> = line.trim_end().splitn(4, ' ').collect();
                    if parts.len() != 4 || parts[1] != "1" || parts[2].parse::<u64>()? != sequence {
                        bail!("Native control rejection does not match request");
                    }
                    bail!("Native control rejected request {sequence}: {}", parts[3]);
                }
                if !line.starts_with("TLACK ") {
                    continue;
                }
                let parts: Vec<_> = line.split_whitespace().collect();
                if parts.len() != 5 || parts[1] != "1" || parts[2].parse::<u64>()? != sequence {
                    bail!("Native control acknowledgement does not match request");
                }
                let actual = Point {
                    frequency_mhz: parts[3].parse()?,
                    voltage_v: parts[4].parse()?,
                };
                if !self.accepts(actual)
                    || (actual.frequency_mhz - target.frequency_mhz).abs() > 1.
                    || (actual.voltage_v - target.voltage_v).abs() > 0.0063
                {
                    bail!("Native hardware readback does not match the requested setting");
                }
                return Ok(actual);
            }
            bail!("Native client did not acknowledge live control")
        })
        .await
        .context("Native live control timed out")?
    }
    pub(super) async fn change_with_logs(
        &mut self,
        target: Point,
        seconds: u64,
        log: &mut ClientLog,
        shared: &Shared,
        board: &BoardConfig,
    ) -> Result<Point> {
        let response = self.change(target, seconds);
        tokio::pin!(response);
        loop {
            tokio::select! {
                result = &mut response => return result,
                result = log.read(shared, board) => { result?; }
            }
        }
    }
}

pub(super) async fn supervise(
    shared: Arc<Shared>,
    board: BoardConfig,
    mut child: Child,
    ready: oneshot::Sender<()>,
    mut shutdown: watch::Receiver<bool>,
    initialization: tokio::sync::MutexGuard<'static, ()>,
    preparing: Option<timelord_supervisor::progress::Guard>,
) -> Result<()> {
    // Attach the pipes before polling initialization: cancellation must leave EOF
    // shutdown and its confirmation available to the same owner as normal exit.
    let mut owner = NativeOwner::attach(&board, &mut child)?;
    let output = child.stderr.take().context("ASIC client log unavailable")?;
    let mut log = ClientLog::new(output, ready);
    log.initialization = Some(initialization);
    log.preparing = preparing;
    let initialized = if *shutdown.borrow() {
        Ok(false)
    } else {
        tokio::select! {
            result = owner.initialize(&shared, &board) => result.map(|()| true),
            _ = shutdown.changed() => Ok(false),
        }
    };
    if let Err(error) = &initialized {
        shared
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .entry(board.id.clone())
            .or_default()
            .lifecycle
            .fault(
                if thermal_rejection(error) {
                    crate::board_lifecycle::Fault::Thermal
                } else {
                    crate::board_lifecycle::Fault::Transport
                },
                error.to_string(),
                crate::unix_now(),
            );
    }
    let mut watchdog = tokio::time::interval(Duration::from_secs(5));
    let result: Result<()> = match initialized {
        Err(error) => Err(error),
        Ok(false) => Ok(()),
        Ok(true) => loop {
            if *shutdown.borrow() {
                break Ok(());
            }
            tokio::select! {
                _ = shutdown.changed() => break Ok(()),
                _ = watchdog.tick() => {
                    if let Some(hardware) = shared.runtime.lock().expect("runtime").boards.get(&board.id)
                        && (hardware.lifecycle.status().latched_fault || hardware.lifecycle.status().phase == crate::board_lifecycle::Phase::Fault) {
                        break Err(anyhow::anyhow!(hardware.lifecycle.status().fault_reason.unwrap_or("ASIC needs attention").to_owned()));
                    }
                    if log.last_temperature.elapsed() > Duration::from_secs(TEMPERATURE_TIMEOUT_SECONDS) { break Err(anyhow::anyhow!("ASIC temperature telemetry stopped")); }
                    match child.try_wait() {
                        Ok(Some(exit)) => break Err(anyhow::anyhow!("ASIC client exited: {exit}")),
                        Err(error) => break Err(error.into()),
                        Ok(None) => {}
                    }
                    if let Err(error)=owner.tick(&shared,&board,&mut log,&mut shutdown).await {break Err(error);}
                }
                result = log.read(&shared, &board) => {
                    let recovery = match result { Ok(recovery) => recovery, Err(error) => break Err(error) };
                    if recovery && let Err(error)=owner.tick(&shared,&board,&mut log,&mut shutdown).await {break Err(error);}
                }
            }
        },
    };
    let quiesced = owner.stop(&mut child, log).await;
    if !quiesced {
        tracing::warn!(board=%board.name,"Native shutdown lacked confirmed quiescence; owner is unclean");
    }
    let mut state = shared.runtime.lock().expect("runtime");
    let hardware = state.boards.entry(board.id.clone()).or_default();
    hardware.lifecycle.retired(
        quiesced,
        result.as_ref().err().map(ToString::to_string),
        crate::unix_now(),
    );
    hardware.raw_speeds.clear();
    result
}
pub(super) async fn launch(shared: &Shared, board: &BoardConfig) -> Result<Child> {
    let engines = shared
        .assignments
        .iter()
        .filter(|a| a.board_id == board.id)
        .count();
    let mut args = arguments(board, engines)?;
    if board.profile == ProfileMode::Automatic {
        let point = startup_point(
            shared,
            board,
            engines,
            &automatic::fingerprint(&shared.config)?,
            &automatic::host(),
        )?;
        let frequency = args
            .iter()
            .position(|a| a == "--freq")
            .context("Automatic frequency missing")?
            + 1;
        let voltage = args
            .iter()
            .position(|a| a == "--voltage")
            .context("Automatic voltage missing")?
            + 1;
        args[frequency] = point.frequency_mhz.to_string();
        args[voltage] = point.voltage_v.to_string();
    }
    Command::new(
        shared
            .config
            .hardware
            .client_binary
            .as_ref()
            .context("No native client configured")?,
    )
    .args(args)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .context("Launching native ASIC client")
}

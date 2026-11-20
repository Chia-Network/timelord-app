//! Native process, control transport and telemetry. Board policy lives in board.rs.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Recovery {
    pub engine: u8,
}
impl Recovery {
    pub(super) fn parse(line: &str) -> Option<Self> {
        let (_, message) = line.split_once("VDF ")?;
        let (engine, message) = message.split_once(": Restarting VDF at ")?;
        message.strip_suffix(" iters")?.parse::<u64>().ok()?;
        let engine = engine.parse::<u8>().ok()?;
        (engine < 3).then_some(Self { engine })
    }
}
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
#[cfg(test)]
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

// Drain stderr even while control is awaiting its acknowledgement. A recovery
// flood must not fill the pipe and block the USB owner before it reads TLSET.
pub(crate) static INITIALIZATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// One live operating-point change at a time across all boards. Boards share a
/// USB bus; simultaneous changes during another board's fault failed together.
pub(crate) static LIVE_CONTROL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) struct ClientLog {
    pub(super) lines: crate::bounded_lines::Lines<BufReader<tokio::process::ChildStderr>>,
    /// All engine sockets have connected to their listener.
    pub(super) connected: bool,
    pub(super) initialization: Option<tokio::sync::MutexGuard<'static, ()>>,
    pub(super) preparing: Option<timelord_supervisor::progress::Guard>,
    pub(super) last_temperature: tokio::time::Instant,
    /// Last raw-speed, temperature or heartbeat record from the event loop.
    pub(super) last_liveness: tokio::time::Instant,
    /// The native client's own reason for stopping, when it reported one.
    pub(super) stop_reason: Option<String>,
    /// The native client could not open its USB device: busy (for example
    /// still released by a previous owner) or gone. Not a board failure.
    pub(super) open_failed: bool,
    pub(super) recent: std::collections::VecDeque<String>,
    shutdown_confirmed: bool,
}
/// The one native record that proves the client paused its engines and restored
/// its safe point. Any reader that consumes it must keep the evidence.
const SHUTDOWN_CONFIRMED: &str = "Hardware shutdown: engines_paused=1 safe_point_restored=1";
/// Routine per-second native output. Kept at debug so the worker log retains
/// days of decisions instead of minutes of throughput chatter.
fn routine(line: &str) -> bool {
    const ROUTINE: [&str; 14] = [
        "TLRAW ",
        "TLHB ",
        "ASIC Temp = ",
        "HW iters done",
        "SW iters done",
        "aux threads",
        "Proof done for",
        "Starting proof thread",
        "Queued proofs for",
        "intermediate block",
        "Proof retrieved for",
        "Avg iters per intermediate",
        "Received challenge",
        "Stopped at iters",
    ];
    let payload = line.split_once(' ').map_or(line, |(_, rest)| rest).trim();
    payload.is_empty() || ROUTINE.iter().any(|marker| line.contains(marker))
}
pub(super) fn record_thermal_stop(
    state: &mut crate::runtime::RuntimeState,
    shared: &Shared,
    board: &BoardConfig,
    reason: String,
) {
    let already_thermal = state.boards.get(&board.id).is_some_and(|hardware| {
        hardware.lifecycle.status().fault == Some(crate::board_lifecycle::Fault::Thermal)
    });
    if already_thermal {
        return;
    }
    state
        .boards
        .entry(board.id.clone())
        .or_default()
        .lifecycle
        .fault(
            crate::board_lifecycle::Fault::Thermal,
            reason,
            crate::unix_now(),
        );
    state.event(Some(board.id.clone()), "thermal_stop");
    if let Some(metrics) = shared.metrics() {
        metrics.thermal_stopped(&crate::observability::board(&shared.config, &board.id));
    }
}
impl ClientLog {
    pub(super) fn new(stderr: tokio::process::ChildStderr) -> Self {
        Self {
            lines: crate::bounded_lines::Lines::new(BufReader::new(stderr)),
            connected: false,
            initialization: None,
            preparing: None,
            last_temperature: tokio::time::Instant::now(),
            last_liveness: tokio::time::Instant::now(),
            stop_reason: None,
            open_failed: false,
            recent: Default::default(),
            shutdown_confirmed: false,
        }
    }
    /// Read what a stopped client still had to say (bounded), so its last
    /// records decide the outcome rather than which pipe closed first.
    pub(super) async fn drain(&mut self, board: &BoardConfig, limit: Duration) {
        let deadline = tokio::time::Instant::now() + limit;
        while let Ok(Ok(Some(line))) =
            tokio::time::timeout_at(deadline, self.lines.next_line()).await
        {
            let crate::bounded_lines::Line::Text(line) = line else {
                continue;
            };
            // These bytes never reach the retirement reader, so a clean
            // shutdown recorded here must count as confirmation now.
            if self.lines.terminated() && line.ends_with(SHUTDOWN_CONFIRMED) {
                self.shutdown_confirmed = true;
            }
            if let Some((_, reason)) = line.split_once("VDF client stopped: ") {
                self.stop_reason = Some(reason.trim().chars().take(120).collect());
            }
            if line.contains("Failed to open device") {
                self.open_failed = true;
            }
            if !routine(&line) {
                tracing::info!(board=%board.id, "{line}");
                self.recent.push_back(line.chars().take(512).collect());
                if self.recent.len() > 8 {
                    self.recent.pop_front();
                }
            }
        }
    }
    pub(super) async fn read(
        &mut self,
        shared: &Shared,
        board: &BoardConfig,
    ) -> Result<Option<Recovery>> {
        let Some(line) = self.lines.next_line().await? else {
            bail!(
                "ASIC client log closed; recent log: {}",
                self.recent.iter().cloned().collect::<Vec<_>>().join(" | ")
            );
        };
        let line = match line {
            crate::bounded_lines::Line::Text(line) => line,
            crate::bounded_lines::Line::Oversized => {
                tracing::warn!(board=%board.id, "Discarded oversized native log line (over 4096 bytes)");
                return Ok(None);
            }
            crate::bounded_lines::Line::Invalid => {
                tracing::warn!(board=%board.id, "Discarded native log line that is not valid UTF-8");
                return Ok(None);
            }
        };
        if self.lines.terminated() && line.ends_with(SHUTDOWN_CONFIRMED) {
            self.shutdown_confirmed = true;
        }
        if let Some((_, reason)) = line.split_once("VDF client stopped: ") {
            self.stop_reason = Some(reason.trim().chars().take(120).collect());
        }
        if line.contains("Failed to open device") {
            self.open_failed = true;
        }
        if line.contains("TLHB ") {
            self.last_liveness = tokio::time::Instant::now();
        }
        if routine(&line) {
            tracing::debug!(board=%board.id, "{line}");
        } else {
            self.recent.push_back(line.chars().take(512).collect());
            if self.recent.len() > 8 {
                self.recent.pop_front();
            }
            tracing::info!(board=%board.id, "{line}");
        }
        let mut state = shared.runtime.lock().expect("runtime");
        if let Some((engine, ips, activity)) = raw_speed(&line) {
            self.last_liveness = tokio::time::Instant::now();
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
            return Err(NativeFailure::new(
                NativeFailureKind::SensorUnavailable,
                "ASIC temperature telemetry unavailable",
            )
            .into());
        }
        if let Some((temp, freq)) = temperature(&line) {
            if temp <= 0. {
                return Err(NativeFailure::new(
                    NativeFailureKind::SensorUnavailable,
                    "ASIC temperature telemetry unavailable",
                )
                .into());
            }
            self.last_temperature = tokio::time::Instant::now();
            self.last_liveness = self.last_temperature;
            let over_limit = temp >= board.temperature_limit_c;
            {
                let hardware = state.boards.entry(board.id.clone()).or_default();
                hardware.temperature_unix = crate::unix_now();
                hardware.temperature_samples = hardware.temperature_samples.saturating_add(1);
                hardware.temperature_c = Some(temp);
                hardware.frequency_mhz = Some(freq);
            }
            if over_limit {
                record_thermal_stop(
                    &mut state,
                    shared,
                    board,
                    "ASIC reached its configured temperature limit".into(),
                );
                // Retire the native owner before attempting fault persistence.
                drop(state);
                return Err(NativeFailure::new(
                    NativeFailureKind::Thermal,
                    "ASIC reached its configured temperature limit",
                )
                .into());
            }
            // Pinned native code emits this only from event_loop/read_hw_status,
            // after init_hw completed USB open, clock/voltage and PVT setup.
            self.initialization.take();
        }
        let recovery = Recovery::parse(&line);
        if let Some(event) = recovery {
            tracing::debug!(engine = event.engine, "Native engine recovered");
            state.boards.entry(board.id.clone()).or_default().recoveries += 1;
            state.event(Some(board.id.clone()), "recovery");
            if let Some(metrics) = shared.metrics() {
                metrics.asic_recovered(
                    &crate::observability::board(&shared.config, &board.id),
                    event.engine,
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
            self.connected = true;
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
    pub(super) output: crate::bounded_lines::Lines<BufReader<tokio::process::ChildStdout>>,
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
    fn accepts_readback(&self, point: Point) -> bool {
        point.frequency_mhz.is_finite()
            && point.voltage_v.is_finite()
            && (self.minimum_frequency - 1. ..=self.maximum_frequency + 1.)
                .contains(&point.frequency_mhz)
            && (self.minimum_voltage - 0.0063..=1.0063).contains(&point.voltage_v)
    }
}

/// The startup operating point of one native lifetime. It is computed once, so
/// the journaled, launched and applied points are the same value.
pub(super) struct Startup {
    pub(super) point: Point,
    learned: Option<automatic::Learned>,
    /// A thermal recovery probes at low power, then resumes here once fresh
    /// telemetry is cool.
    resume: Option<Point>,
    /// A cautious start returns to this best-known point after verified work.
    pub(super) ramp: Option<Point>,
    pub(super) cautious: bool,
}

/// The best-known point for this board at its live engine count, and the
/// learned profile it came from.
async fn best_known(
    shared: &Shared,
    board: &BoardConfig,
    engines: usize,
) -> Result<(Point, Option<automatic::Learned>)> {
    if board.profile != ProfileMode::Automatic {
        let (frequency_mhz, voltage_v) = board.operating_point();
        return Ok((
            Point {
                frequency_mhz,
                voltage_v,
            },
            None,
        ));
    }
    let facts = shared
        .native
        .as_ref()
        .context("Automatic capabilities were not prepared")?;
    let learned = facts.learned(&shared.config, board, engines).await?;
    let point = match &learned {
        Some(entry) => automatic::initial_point(board, Some(entry)),
        None => facts
            .inherited(&shared.config, board, engines)
            .await?
            .unwrap_or_else(|| automatic::initial_point(board, None)),
    };
    Ok((point, learned))
}

pub(super) async fn startup(shared: &Shared, board: &BoardConfig) -> Result<Startup> {
    let engines = shared.engines(&board.id).max(1);
    let policy = shared
        .runtime
        .lock()
        .expect("runtime")
        .safety
        .as_ref()
        .map(|store| store.policy(board))
        .unwrap_or_default();
    let (best, learned) = best_known(shared, board, engines).await?;
    let cautious_point = |point: Point| Point {
        frequency_mhz: point.frequency_mhz.min(best.frequency_mhz),
        voltage_v: point.voltage_v.min(best.voltage_v),
    };
    if policy.restriction == Some(crate::safety::Reason::Thermal) {
        let resume = cautious_point(recovery_point(shared, board));
        return Ok(Startup {
            point: Point {
                frequency_mhz: 200.,
                voltage_v: 0.7,
            },
            learned,
            resume: Some(resume),
            ramp: (best != resume).then_some(best),
            cautious: true,
        });
    }
    if policy.restriction.is_some() || policy.cautious {
        let point = cautious_point(recovery_point(shared, board));
        return Ok(Startup {
            point,
            learned,
            resume: None,
            ramp: (best != point).then_some(best),
            cautious: true,
        });
    }
    Ok(Startup {
        point: best,
        learned,
        resume: None,
        ramp: None,
        cautious: false,
    })
}

impl NativeOwner {
    pub(super) fn attach(board: &BoardConfig, child: &mut Child) -> Result<Self> {
        let automatic = board.profile == ProfileMode::Automatic;
        let owner = Self {
            control: NativeControl {
                input: child.stdin.take().context("Native control input missing")?,
                output: crate::bounded_lines::Lines::new(BufReader::new(
                    child
                        .stdout
                        .take()
                        .context("Native control output missing")?,
                )),
                sequence: 0,
                // The thermal probe must remain possible below tuning minima.
                minimum_frequency: 200.,
                maximum_frequency: if automatic {
                    board.automatic.max_frequency_mhz
                } else {
                    2200.
                },
                minimum_voltage: 0.7,
            },
            tuning: None,
        };
        Ok(owner)
    }
    pub(super) async fn initialize(
        &mut self,
        shared: &Shared,
        board: &BoardConfig,
        log: &mut ClientLog,
        startup: &Startup,
    ) -> Result<()> {
        let point = startup.point;
        let tuning = if board.profile == ProfileMode::Automatic {
            let facts = shared
                .native
                .as_ref()
                .context("Automatic capabilities were not prepared")?;
            let mut policy = Policy::new(
                board.automatic.clone(),
                point,
                crate::unix_now(),
                facts.frequencies.clone(),
            );
            // A cautious start restores learned limits only after it returns
            // to the learned point; until then it never saves a profile.
            if !startup.cautious
                && let Some(entry) = &startup.learned
            {
                policy.restore(entry);
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
            Some(Tuning {
                experiment: None,
                policy,
                serial,
                engines: shared.engines(&board.id),
                hash: facts.hash.clone(),
                host: facts.host.clone(),
                raw_window: Default::default(),
                last_record_unix: crate::unix_now(),
                journal,
                cautious: startup.cautious,
                learned: startup.learned.clone(),
            })
        } else {
            None
        };
        self.tuning = tuning;
        let actual = self
            .control
            .change_with_logs(point, 30, log, shared, board)
            .await?;
        if let Some(tuning) = &mut self.tuning {
            tuning.policy.applied(
                actual,
                crate::unix_now(),
                if startup.cautious {
                    "Starting cautiously; returning to the saved setting after verified work"
                } else {
                    "Observing live proofs at the initial setting"
                }
                .into(),
            );
            let recoveries = shared
                .runtime
                .lock()
                .expect("runtime")
                .boards
                .get(&board.id)
                .map_or(0, |hardware| hardware.recoveries);
            tuning.policy.synchronize_recoveries(recoveries);
        }
        Ok(())
    }
    /// Return from a cautious start to the best-known point.
    pub(super) async fn ramp(
        &mut self,
        shared: &Shared,
        board: &BoardConfig,
        log: &mut ClientLog,
        target: Point,
    ) -> Result<bool> {
        let journal = shared
            .runtime
            .lock()
            .expect("runtime")
            .safety
            .clone()
            .zip(board.serial.clone());
        if let Some((store, serial)) = &journal {
            let token = store
                .owner_token(serial)
                .context("Live owner lost its journal token")?;
            let (store, serial) = (store.clone(), serial.clone());
            match tokio::task::spawn_blocking(move || store.set_point(&serial, token, target)).await
            {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => bail!("Native owner journal token was superseded"),
                // Raising without a durable journal could leave a crash at a
                // point the journal does not know. Retry later.
                Ok(Err(_)) | Err(_) => return Ok(false),
            }
        }
        tracing::info!(board=%board.name, frequency_mhz=target.frequency_mhz, voltage_v=target.voltage_v, "Returning to the saved setting after verified work");
        let actual = self
            .control
            .change_with_logs(target, 30, log, shared, board)
            .await
            .context("Returning to the saved setting")?;
        if let Some(tuning) = &mut self.tuning {
            tuning.policy.applied(
                actual,
                crate::unix_now(),
                "Returned to the saved setting after verified work".into(),
            );
            if let Some(entry) = &tuning.learned {
                tuning.policy.restore(entry);
            }
        }
        let mut state = shared.runtime.lock().expect("runtime");
        let hardware = state.boards.entry(board.id.clone()).or_default();
        hardware.frequency_mhz = Some(actual.frequency_mhz);
        hardware.voltage_v = Some(actual.voltage_v);
        Ok(true)
    }
    pub(super) fn end_cautious(&mut self) {
        if let Some(tuning) = &mut self.tuning {
            tuning.cautious = false;
        }
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
            mut input,
            output,
            sequence,
            ..
        } = self;
        // Older clients reject an unfamiliar stop command by taking their
        // existing fail-safe EOF path. The pinned client acknowledges it.
        let stop_sequence = sequence.saturating_add(1);
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            input.write_all(format!("TLSTOP 1 {stop_sequence}\n").as_bytes()),
        )
        .await;
        drop(input);
        let ClientLog {
            lines,
            initialization,
            shutdown_confirmed,
            ..
        } = log;
        let _initialization = initialization;
        super::retirement::retire(
            child,
            output.into_inner(),
            lines.into_inner(),
            stop_sequence,
            shutdown_confirmed,
        )
        .await
        .confirmed()
    }
    pub(super) async fn change(&mut self, target: Point, seconds: u64) -> Result<Point> {
        if !self.accepts(target) {
            bail!("Native request exceeds configured limits");
        }
        self.sequence += 1;
        let sequence = self.sequence;
        tokio::time::timeout(Duration::from_secs(seconds), async {
            let _serialized = LIVE_CONTROL.lock().await;
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
            for _ in 0..16 {
                let Some(record) = self.output.next_line().await? else {
                    return Err(NativeFailure::new(
                        NativeFailureKind::ControlClosed,
                        "Native control closed before acknowledgement",
                    )
                    .into());
                };
                let crate::bounded_lines::Line::Text(line) = record else {
                    bail!("Native control response too large or invalid");
                };
                if line.len() > 512 || !self.output.terminated() {
                    bail!("Native control response too large or incomplete");
                }
                if line.starts_with("TLERR ") {
                    let parts: Vec<_> = line.trim_end().splitn(4, ' ').collect();
                    if parts.len() != 4 || parts[1] != "1" || parts[2].parse::<u64>()? != sequence {
                        bail!("Native control rejection does not match request");
                    }
                    return Err(NativeFailure::rejection(sequence, parts[3]).into());
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
                if !self.accepts_readback(actual)
                    || (actual.frequency_mhz - target.frequency_mhz).abs() > 1.
                    || (actual.voltage_v - target.voltage_v).abs() > 0.0063
                {
                    return Err(NativeFailure::new(
                        NativeFailureKind::Readback,
                        "Native hardware readback does not match the requested setting",
                    )
                    .into());
                }
                // Store a replayable command point within configured bounds;
                // physical voltage/frequency telemetry remains measured data.
                return Ok(Point {
                    frequency_mhz: actual
                        .frequency_mhz
                        .clamp(self.minimum_frequency, self.maximum_frequency),
                    voltage_v: actual.voltage_v.clamp(self.minimum_voltage, 1.),
                });
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

/// How often an unrouted serving board proves itself with a private proof.
pub(super) const CANARY_SECONDS: u64 = 60;

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(super) async fn supervise(
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    board: BoardConfig,
    child: Child,
    shutdown: watch::Receiver<bool>,
    initialization: tokio::sync::MutexGuard<'static, ()>,
    preparing: Option<timelord_supervisor::progress::Guard>,
    startup: Startup,
) -> Result<()> {
    supervise_with(
        shared,
        backend,
        board,
        child,
        shutdown,
        initialization,
        preparing,
        startup,
        super::board::Timing::default(),
    )
    .await
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn supervise_with(
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    board: BoardConfig,
    mut child: Child,
    mut shutdown: watch::Receiver<bool>,
    initialization: tokio::sync::MutexGuard<'static, ()>,
    preparing: Option<timelord_supervisor::progress::Guard>,
    startup: Startup,
    timing: super::board::Timing,
) -> Result<()> {
    let setup_deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    {
        let mut state = shared.runtime.lock().expect("runtime");
        let hardware = state.boards.entry(board.id.clone()).or_default();
        hardware.native_pid = child.id();
        hardware.lifecycle.checking();
    }

    // Attach the pipes before polling initialization: cancellation must leave EOF
    // shutdown and its confirmation available to the same owner as normal exit.
    let resume = startup.resume;
    let mut maintenance = Maintenance {
        ramp: startup.ramp,
        cautious: startup.cautious,
        last_canary: tokio::time::Instant::now(),
        canary: timing.canary,
    };
    let mut owner = NativeOwner::attach(&board, &mut child)?;
    let output = child.stderr.take().context("ASIC client log unavailable")?;
    let mut log = ClientLog::new(output);
    log.initialization = Some(initialization);
    log.preparing = preparing;
    if let Some(preparing) = &log.preparing {
        preparing.stage(timelord_supervisor::progress::Stage::BoardSetup);
    }
    let initialized = if *shutdown.borrow() {
        Ok(false)
    } else {
        tokio::select! {
            result = tokio::time::timeout_at(setup_deadline, owner.initialize(&shared, &board, &mut log, &startup)) => result.context("ASIC initialization timed out").and_then(|r| r).map(|()| true),
            _ = shutdown.changed() => Ok(false),
        }
    };
    if matches!(initialized, Ok(true)) {
        // The acknowledged setting confirms native hardware setup. Release USB
        // initialization before waiting for recovery evidence so other boards
        // can start meanwhile; network readiness keeps its own deadline.
        if board.profile == ProfileMode::Automatic {
            log.initialization.take();
        }
        if let Some(preparing) = &log.preparing {
            preparing.stage(timelord_supervisor::progress::Stage::BoardReadiness);
        }
    }
    let initialized = if matches!(initialized, Ok(true)) {
        super::recovery::establish(&shared, &board, &mut owner, &mut log, &mut shutdown, resume)
            .await
    } else {
        initialized
    };
    // A device that could not be opened is busy or absent, not failing. The
    // control pipe can close before stderr is read, so read its last words.
    if initialized.is_err() && !log.open_failed {
        log.drain(&board, Duration::from_millis(500)).await;
    }
    let initialized = match initialized {
        Err(error) if log.open_failed => Err(anyhow::Error::new(super::board::BoardAbsent(
            board.name.clone(),
        ))
        .context(format!("{error:#}"))),
        other => other,
    };
    if let Err(error) = &initialized {
        let fault = classify(error);
        let mut state = shared.runtime.lock().expect("runtime");
        if fault == crate::board_lifecycle::Fault::Thermal {
            record_thermal_stop(&mut state, &shared, &board, format!("{error:#}"));
        } else {
            state
                .boards
                .entry(board.id.clone())
                .or_default()
                .lifecycle
                .fault(fault, format!("{error:#}"), crate::unix_now());
        }
        state
            .boards
            .entry(board.id.clone())
            .or_default()
            .fault(error);
    }
    // Successful setup starts a fresh readiness budget; time spent obtaining
    // the first ACK cannot starve engine/socket initialization of its window.
    let readiness_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut watchdog = tokio::time::interval(
        Duration::from_secs(5)
            .min(timing.liveness / 3)
            .max(Duration::from_millis(50)),
    );
    let result: Result<()> = match initialized {
        Err(error) => Err(error),
        Ok(false) => Ok(()),
        Ok(true) => loop {
            if *shutdown.borrow() {
                break Ok(());
            }
            tokio::select! {
                // Deterministic precedence: shutdown, then the readiness
                // deadline, then periodic checks, then native output.
                biased;
                _ = shutdown.changed() => break Ok(()),
                // Temperatures do not certify engine/socket initialization.
                // Hold the lease through confirmed retirement on this deadline.
                _ = tokio::time::sleep_until(readiness_deadline), if !log.connected => {
                    break Err(anyhow::anyhow!("ASIC initialization timed out: the native client never connected its engines"));
                }
                _ = watchdog.tick() => {
                    if let Some(hardware) = shared.runtime.lock().expect("runtime").boards.get(&board.id)
                        && hardware.lifecycle.needs_retirement() {
                        let status = hardware.lifecycle.status();
                        let reason = status.fault_reason.unwrap_or("ASIC needs attention").to_owned();
                        break Err(match status.fault {
                            Some(crate::board_lifecycle::Fault::InvalidProof) => crate::asic::AsicFailure { board_id: board.id.clone(), verification: true, message: reason }.into(),
                            _ => anyhow::anyhow!(reason),
                        });
                    }
                    if log.last_liveness.elapsed() > timing.liveness {
                        break Err(NativeFailure::new(NativeFailureKind::Stalled, format!(
                            "Native client stopped reporting for {} seconds; its USB event loop appears stuck",
                            timing.liveness.as_secs()
                        )).into());
                    }
                    if log.last_temperature.elapsed() > Duration::from_secs(TEMPERATURE_TIMEOUT_SECONDS) {
                        break Err(NativeFailure::new(NativeFailureKind::SensorUnavailable, "ASIC temperature telemetry stopped").into());
                    }
                    match child.try_wait() {
                        Ok(Some(exit)) => break Err(exited(exit, &log)),
                        Err(error) => break Err(error.into()),
                        Ok(None) => {}
                    }
                    if let Err(error) = maintenance.run(&shared, &backend, &board, &mut owner, &mut log, &mut shutdown).await { break Err(error); }
                    if let Err(error)=owner.tick(&shared,&board,&mut log,&mut shutdown).await {break Err(error);}
                }
                result = log.read(&shared, &board) => {
                    let recovery = match result { Ok(recovery) => recovery, Err(error) => break Err(match child.try_wait() { Ok(Some(exit)) => exited(exit, &log), _ => error }) };
                    if recovery.is_some() && let Err(error)=owner.tick(&shared,&board,&mut log,&mut shutdown).await {break Err(error);}
                }
            }
        },
    };
    shared.runtime.lock().expect("runtime").disturb(
        crate::unix_now(),
        crate::hardware::continuous::DISTURBANCE_SECONDS,
    );
    let quiesced = owner.stop(&mut child, log).await;
    if !quiesced {
        tracing::warn!(board=%board.name,"Native shutdown lacked confirmed quiescence; next start is cautious");
    }
    record_retirement(&shared, &board, quiesced, result.as_ref().err());
    result
}

/// The native client's exit, with its own stop reason when it reported one.
fn exited(exit: std::process::ExitStatus, log: &ClientLog) -> anyhow::Error {
    let reason = log.stop_reason.clone();
    NativeFailure::new(
        NativeFailureKind::Exited,
        match reason {
            Some(reason) => format!("ASIC client exited ({exit}): {reason}"),
            None => format!("ASIC client exited: {exit}"),
        },
    )
    .into()
}

/// Periodic per-owner work on a serving board: return from a cautious start,
/// prove an unrouted board with a private canary, clear a stable failure
/// streak and relax a lowered recovery ceiling.
struct Maintenance {
    ramp: Option<Point>,
    cautious: bool,
    last_canary: tokio::time::Instant,
    canary: Duration,
}
impl Maintenance {
    async fn run(
        &mut self,
        shared: &Shared,
        backend: &Arc<crate::asic::AsicBackend>,
        board: &BoardConfig,
        owner: &mut NativeOwner,
        log: &mut ClientLog,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let now = crate::unix_now();
        let (serving, generation, serving_since, evidence, routed, disturbed, cool) = {
            let state = shared.runtime.lock().expect("runtime");
            let Some(hardware) = state.boards.get(&board.id) else {
                return Ok(());
            };
            let status = hardware.lifecycle.status();
            (
                hardware.lifecycle.accepts_work(),
                status.generation,
                status.serving_since_unix,
                status.evidence_unix,
                state.routing.iter().any(|a| a.board_id == board.id),
                state.disturbed(now),
                hardware
                    .temperature_c
                    .is_some_and(|t| t < board.temperature_limit_c - 5.)
                    && now.saturating_sub(hardware.temperature_unix) <= 10,
            )
        };
        let store = shared.runtime.lock().expect("runtime").safety.clone();
        let journal = store.zip(board.serial.clone());
        if let Some((store, serial)) = &journal
            && store.health(serial).disabled(now)
        {
            return Err(super::board::OperatorHold.into());
        }
        if !serving {
            return Ok(());
        }
        if self.cautious && evidence > 0 && evidence >= serving_since && !disturbed && cool {
            // Automatic: the best-known point for the live engine count,
            // which may differ from the count when this owner started.
            // Manual: the configured point.
            let target = if let Some(tuning) = &mut owner.tuning {
                let (best, learned) =
                    best_known(shared, board, shared.engines(&board.id).max(1)).await?;
                tuning.learned = learned;
                tuning.engines = shared.engines(&board.id);
                (best != tuning.policy.point).then_some(best)
            } else {
                self.ramp
            };
            let returned = match target {
                Some(target) => owner.ramp(shared, board, log, target).await?,
                None => true,
            };
            if returned {
                self.ramp = None;
                self.cautious = false;
                owner.end_cautious();
                if let Some((store, serial)) = &journal
                    && let Some(token) = store.owner_token(serial)
                    && let Err(error) = store.clear_cautious(serial, token)
                {
                    tracing::warn!(error=%format!("{error:#}"), "Cautious-start completion could not be saved; next start remains cautious");
                }
                shared
                    .runtime
                    .lock()
                    .expect("runtime")
                    .boards
                    .entry(board.id.clone())
                    .or_default()
                    .cautious = false;
                crate::board_history::record(shared, board, "cautious_complete", None, None);
            }
        }
        // Private canary proofs run on an engine no route uses: on an
        // unrouted board (benched, spare) to keep checking it, and on a board
        // still finishing a cautious start whose routed chain has no work yet.
        let needs_evidence = !routed || (self.cautious && evidence < serving_since.max(1));
        let idle_engine = {
            let state = shared.runtime.lock().expect("runtime");
            [1u8, 0, 2].into_iter().find(|engine| {
                !state
                    .routing
                    .iter()
                    .any(|a| a.board_id == board.id && a.engine == *engine)
            })
        };
        if needs_evidence
            && let Some(engine) = idle_engine
            && self.last_canary.elapsed() >= self.canary
        {
            self.last_canary = tokio::time::Instant::now();
            let check = backend.check_engine(&board.id, engine);
            tokio::pin!(check);
            loop {
                tokio::select! {
                    result = &mut check => { result.context("Private canary proof failed")?; break; },
                    _ = shutdown.changed() => return Ok(()),
                    result = log.read(shared, board) => { result?; }
                }
            }
            shared
                .runtime
                .lock()
                .expect("runtime")
                .boards
                .entry(board.id.clone())
                .or_default()
                .lifecycle
                .canary(generation, crate::unix_now());
        }
        if let Some((store, serial)) = &journal {
            let health = store.health(serial);
            if health.stable(now, serving_since, evidence) {
                match store.update_health(serial, true, |h| h.clear_streak()) {
                    Ok(health) => {
                        tracing::info!(board=%board.name, "Board served stably; its failure streak is cleared");
                        crate::board_history::record(shared, board, "stable", None, Some(&health));
                    }
                    Err(error) => {
                        tracing::warn!(error=%format!("{error:#}"), "Board stability could not be saved")
                    }
                }
            }
            // Cheap read first: the journal writer is touched only when due.
            let due = store.board(serial).is_some_and(|b| {
                b.recovery_ceiling_mhz.is_some()
                    && b.restriction.is_none()
                    && now.saturating_sub(b.safety_clear_since_unix)
                        >= crate::safety::CEILING_RELAX_SECONDS
            });
            if due && let Some(token) = store.owner_token(serial) {
                match store.relax_ceiling(serial, token) {
                    Ok(Some(ceiling)) => {
                        tracing::info!(board=%board.name, ceiling_mhz=?ceiling, "Raised the recovery ceiling after sustained fault-free serving");
                        crate::board_history::record(shared, board, "ceiling_raised", None, None);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(error=%format!("{error:#}"), "Recovery ceiling could not be raised")
                    }
                }
            }
        }
        Ok(())
    }
}

pub(super) fn record_retirement(
    shared: &Shared,
    board: &BoardConfig,
    quiesced: bool,
    error: Option<&anyhow::Error>,
) {
    let mut state = shared.runtime.lock().expect("runtime");
    state.disturb(
        crate::unix_now(),
        crate::hardware::continuous::DISTURBANCE_SECONDS,
    );
    if let Some(error) = error
        && classify(error) == crate::board_lifecycle::Fault::Thermal
    {
        // Live control can reject a setting before stderr supplies a hot sample.
        // Latch the typed restriction before the caller checkpoints retirement.
        record_thermal_stop(&mut state, shared, board, format!("{error:#}"));
    }
    let hardware = state.boards.entry(board.id.clone()).or_default();
    hardware
        .lifecycle
        .retired(quiesced, error.map(|e| format!("{e:#}")), crate::unix_now());
    hardware.raw_speeds.clear();
    hardware.native_pid = None;
    if let Some(error) = error {
        hardware.fault(error);
    }
}
/// Every persistent owner opens all three engines with identified sockets;
/// routes decide which engines receive work.
pub(super) async fn launch(
    shared: &Shared,
    board: &BoardConfig,
    startup: &Startup,
) -> Result<Child> {
    let mut args = arguments(board, 3)?;
    args.push("--engine-identity".into());
    let point = startup.point;
    let frequency = args
        .iter()
        .position(|a| a == "--freq")
        .context("Native frequency missing")?
        + 1;
    let voltage = args
        .iter()
        .position(|a| a == "--voltage")
        .context("Native voltage missing")?
        + 1;
    args[frequency] = point.frequency_mhz.to_string();
    args[voltage] = point.voltage_v.to_string();
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

#[cfg(test)]
mod tests;

//! Native process, control transport and telemetry. Board policy lives in board.rs.
use super::*;
mod client_log;
pub(super) mod supervision;
pub(super) use client_log::{ClientLog, record_thermal_stop};

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
    engines: usize,
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
        .ok_or_else(|| anyhow::Error::new(NotPrepared))?;
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
    if board.profile == ProfileMode::Automatic && shared.native.is_none() {
        return Err(NotPrepared.into());
    }
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
            engines,
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
            engines,
            learned,
            resume: None,
            ramp: (best != point).then_some(best),
            cautious: true,
        });
    }
    Ok(Startup {
        point: best,
        engines,
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
                .ok_or_else(|| anyhow::Error::new(NotPrepared))?;
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
                engines: startup.engines,
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
        super::retirement::retire(child, output, lines, stop_sequence, shutdown_confirmed)
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

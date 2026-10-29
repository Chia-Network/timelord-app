//! Supervise only explicitly configured native clients. Initialization is sequential.
mod admission;
use crate::{
    Shared,
    automatic::{self, Action, Point, Policy},
    runtime::{HardwareState, RawState},
};
use anyhow::{Context, Result, bail};
use std::{process::Stdio, sync::Arc, time::Duration};
use timelord_core::{BoardConfig, Config, ProfileMode};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{oneshot, watch},
    task::JoinSet,
};

pub fn arguments(board: &BoardConfig, engines: usize) -> Result<Vec<String>> {
    let mask = timelord_core::engine_mask(engines).context("Expected one to three engines")?;
    let mut arguments = vec![
        "--ip".into(),
        "127.0.0.1".into(),
        "--vdfs-mask".into(),
        mask.to_string(),
        "--device-loc".into(),
        board
            .device_location
            .expect("validated location")
            .to_string(),
        board.client_port.to_string(),
        engines.to_string(),
    ];
    if let Some(serial) = &board.serial {
        arguments.extend(["--expected-serial".into(), serial.clone()]);
    }
    arguments.push("--raw-speed".into());
    let (frequency, voltage) = board.operating_point();
    arguments.extend([
        "--vdf-threads".into(),
        "4".into(),
        "--proof-threads".into(),
        "3".into(),
        "--retain-asic-values".into(),
        "--checkpoint-threshold".into(),
        "250000".into(),
        "--freq".into(),
        frequency.to_string(),
        "--voltage".into(),
        voltage.to_string(),
    ]);
    // Manual profiles retain their requested setting but need the same inherited
    // controller-lifetime and hardware-readback guard as Automatic profiles.
    arguments.push("--live-control".into());
    Ok(arguments)
}
// Versioned native telemetry distinguishes a new rate baseline from real idle.
fn raw_speed(line: &str) -> Option<(usize, f64, RawState)> {
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

fn temperature(line: &str) -> Option<(f64, f64)> {
    let (_, value) = line.split_once("ASIC Temp = ")?;
    let (temp, frequency) = value.split_once(" C; Frequency = ")?;
    let (frequency, _) = frequency.split_once(" MHz")?;
    let temperature: f64 = temp.trim().parse().ok()?;
    let frequency: f64 = frequency.trim().parse().ok()?;
    (temperature.is_finite() && frequency.is_finite()).then_some((temperature, frequency))
}
async fn terminate(child: &mut Child) {
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
async fn drain_shutdown(source: &mut (impl tokio::io::AsyncRead + Unpin)) -> Result<bool> {
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

struct ClientLog {
    lines: crate::bounded_lines::Lines<BufReader<tokio::process::ChildStderr>>,
    ready: Option<oneshot::Sender<()>>,
    initialization: Option<tokio::sync::MutexGuard<'static, ()>>,
    preparing: Option<timelord_supervisor::progress::Guard>,
    last_temperature: tokio::time::Instant,
    recent: std::collections::VecDeque<String>,
}
impl ClientLog {
    fn new(stderr: tokio::process::ChildStderr, ready: oneshot::Sender<()>) -> Self {
        Self {
            lines: crate::bounded_lines::Lines::new(BufReader::new(stderr)),
            ready: Some(ready),
            initialization: None,
            preparing: None,
            last_temperature: tokio::time::Instant::now(),
            recent: Default::default(),
        }
    }
    async fn read(&mut self, shared: &Shared, board: &BoardConfig) -> Result<bool> {
        let Some(line) = self.lines.next_line().await? else {
            bail!(
                "ASIC client log closed; recent log: {}",
                self.recent.iter().cloned().collect::<Vec<_>>().join(" | ")
            );
        };
        self.recent.push_back(line.chars().take(512).collect());
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
            if hardware.state == "starting" || hardware.state == "connected" {
                hardware.state = "connected".into();
            }
            if hardware.native_initialized && temp < board.temperature_limit_c {
                hardware.unavailable_since_unix = None;
            }
            if temp >= board.temperature_limit_c {
                let first = !hardware.latched_fault;
                hardware.state = "thermal_stop".into();
                hardware.latched_fault = true;
                state.event(Some(board.id.clone()), "thermal_stop");
                if first && let Some(metrics) = &state.metrics {
                    metrics.inc(
                        "thermal_stops_total",
                        &[&crate::observability::board(&shared.config, &board.id)],
                    );
                }
                // Return to the owner immediately. Journal I/O happens only
                // after native retirement, never ahead of thermal shutdown.
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
                    .and_then(|(n, _)| n.parse::<u8>().ok())
                    .filter(|n| *n < 3)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unknown".into());
                metrics.inc(
                    "asic_recoveries_total",
                    &[
                        &crate::observability::board(&shared.config, &board.id),
                        &engine,
                    ],
                );
            }
        }
        if line.contains("Connected to timelord, waiting for challenge") {
            state
                .boards
                .entry(board.id.clone())
                .or_default()
                .native_initialized = true;
            self.initialization.take();
            self.preparing.take();
            if let Some(ready) = self.ready.take() {
                let _ = ready.send(());
            }
        }
        Ok(recovery)
    }
}
async fn while_logging(
    pending: impl std::future::Future<Output = Result<()>>,
    shared: &Shared,
    board: &BoardConfig,
    log: &mut ClientLog,
) -> Result<Option<anyhow::Error>> {
    tokio::pin!(pending);
    loop {
        tokio::select! {
            result=&mut pending=>return Ok(result.err()),
            result=log.read(shared,board)=>{result?;},
        }
    }
}
async fn journal_point(
    shared: &Shared,
    board: &BoardConfig,
    point: Point,
    log: &mut ClientLog,
) -> Result<Option<anyhow::Error>> {
    let safety = shared.runtime.lock().expect("runtime").safety.clone();
    let Some(store) = safety else { return Ok(None) };
    let serial = board
        .serial
        .clone()
        .context("Managed board identity missing")?;
    static JOURNALS: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
        std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
    let progress = shared.runtime.lock().expect("runtime").progress.clone();
    let gate = progress
        .as_ref()
        .map(|p| p.arithmetic(9))
        .unwrap_or_else(|| JOURNALS.clone());
    // Serialize real writes, not admission to the caller: another board's
    // pending write must not make a restrictive setting skip its own journal.
    while_logging(
        async move {
            let permit = gate
                .acquire_owned()
                .await
                .context("Operating-point journal gate closed")?;
            let stage = progress
                .as_ref()
                .map(|p| p.begin(9, 0, 0, timelord_supervisor::progress::Stage::Persistence));
            let job = tokio::task::spawn_blocking(move || {
                // Actual jobs retain their slot and watchdog if a log fault
                // cancels the caller. Waiting callers have not spawned jobs.
                let (_permit, _stage) = (permit, stage);
                store.before_open(&serial, point)
            });
            job.await.context("Operating-point journal task failed")?
        },
        shared,
        board,
        log,
    )
    .await
}
struct NativeOwner {
    experiment: Option<tokio::sync::OwnedSemaphorePermit>,
    automatic: bool,
    policy: Policy,
    input: tokio::process::ChildStdin,
    output: BufReader<tokio::process::ChildStdout>,
    sequence: u64,
    serial: String,
    engines: usize,
    hash: String,
    host: String,
    raw_window: crate::assignment::RawWindow,
    last_record_unix: u64,
}
fn force_safe(shared: &Shared, board: &BoardConfig) -> bool {
    let safety = shared.runtime.lock().expect("runtime").safety.clone();
    safety
        .zip(board.serial.as_ref())
        .and_then(|(store, serial)| store.board(serial))
        .is_some_and(|b| b.force_safe)
}
fn startup_point(
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
    async fn new(shared: &Shared, board: &BoardConfig, child: &mut Child) -> Result<Self> {
        let engines = shared
            .assignments
            .iter()
            .filter(|a| a.board_id == board.id)
            .count();
        let automatic = board.profile == ProfileMode::Automatic;
        let hash = if automatic {
            automatic::fingerprint(&shared.config)?
        } else {
            String::new()
        };
        let host = if automatic {
            automatic::host()
        } else {
            String::new()
        };
        let point = if automatic {
            startup_point(shared, board, engines, &hash, &host)?
        } else {
            let (frequency_mhz, voltage_v) = board.operating_point();
            Point {
                frequency_mhz,
                voltage_v,
            }
        };
        let settings = if automatic {
            board.automatic.clone()
        } else {
            timelord_core::AutomaticConfig {
                min_frequency_mhz: 200.,
                min_voltage_v: 0.7,
                start_frequency_mhz: point.frequency_mhz,
                start_voltage_v: point.voltage_v,
                ..Default::default()
            }
        };
        let mut owner = Self {
            experiment: None,
            automatic,
            policy: Policy::new(
                settings,
                point,
                crate::unix_now(),
                if automatic {
                    supported_frequencies(&shared.config).await?
                } else {
                    vec![point.frequency_mhz]
                },
            ),
            input: child.stdin.take().context("Native control input missing")?,
            output: BufReader::new(
                child
                    .stdout
                    .take()
                    .context("Native control output missing")?,
            ),
            sequence: 0,
            serial: if automatic {
                board
                    .serial
                    .clone()
                    .context("Automatic tuning needs a persistent board serial")?
            } else {
                board.serial.clone().unwrap_or_default()
            },
            engines,
            hash,
            host,
            raw_window: Default::default(),
            last_record_unix: crate::unix_now(),
        };
        if automatic
            && !force_safe(shared, board)
            && let Some(entry) =
                automatic::learned(&shared.config, board, engines, &owner.hash, &owner.host)?
        {
            owner.policy.restore(&entry);
        }
        let actual = owner.change(point, 30).await?;
        owner.policy.applied(
            actual,
            crate::unix_now(),
            "Observing live proofs at the initial setting".into(),
        );
        Ok(owner)
    }
    async fn stop(self, child: &mut Child, log: ClientLog) -> bool {
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
    async fn change(&mut self, target: Point, seconds: u64) -> Result<Point> {
        if !self
            .policy
            .settings
            .contains(target.frequency_mhz, target.voltage_v)
        {
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
                if !self
                    .policy
                    .settings
                    .contains(actual.frequency_mhz, actual.voltage_v)
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
    async fn change_with_logs(
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
    async fn record(&mut self, shared: &Shared, board: &BoardConfig, clean: u64) -> Result<()> {
        static IO: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
            std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
        let path = shared
            .config
            .hardware
            .automatic_state
            .as_ref()
            .context("Automatic state path missing")?
            .clone();
        self.last_record_unix = crate::unix_now();
        let entry = automatic::Learned {
            serial: self.serial.clone(),
            engines: self.engines,
            engine_mask: timelord_core::engine_mask(self.engines),
            host: self.host.clone(),
            client_sha256: self.hash.clone(),
            point: self.policy.point,
            active_seconds: clean,
            updated_unix: crate::unix_now(),
            ceiling_mhz: self.policy.learned_limits().0,
            cooldown_seconds: self.policy.learned_limits().1,
            retest: Some(self.policy.retest_state()),
            performance: if clean >= board.automatic.healthy_seconds {
                self.raw_window.performance()
            } else {
                None
            },
        };
        let (safety, progress) = {
            let state = shared.runtime.lock().expect("runtime");
            (state.safety.clone(), state.progress.clone())
        };
        let epoch = safety
            .as_ref()
            .and_then(|s| s.board(&self.serial))
            .map(|b| b.owner_epoch);
        let clear_safe = clean >= board.automatic.healthy_seconds && !self.policy.in_trial();
        let serial = self.serial.clone();
        let gate = progress
            .as_ref()
            .map(|p| p.arithmetic(20))
            .unwrap_or_else(|| IO.clone());
        let permit = tokio::time::timeout(Duration::from_secs(2), gate.acquire_owned())
            .await
            .context("Another diagnostic checkpoint is pending")??;
        let stage = progress.as_ref().map(|p| {
            p.begin(
                20,
                0,
                0,
                timelord_supervisor::progress::Stage::OptionalPersistence,
            )
        });
        let job = tokio::task::spawn_blocking(move || {
            let (_permit, _stage) = (permit, stage);
            automatic::save(&path, entry)?;
            if clear_safe && let (Some(store), Some(epoch)) = (safety, epoch) {
                store.healthy_profile_saved(&serial, epoch)?;
            }
            Ok::<_, anyhow::Error>(())
        });
        // The actual optional job remains visible and retains its one permit
        // after a caller timeout. No replacement jobs accumulate behind it.
        tokio::time::timeout(Duration::from_secs(2), job)
            .await
            .context("Automatic checkpoint delayed; hardware point held")??
    }
    async fn record_with_logs(
        &mut self,
        shared: &Shared,
        board: &BoardConfig,
        clean: u64,
        log: &mut ClientLog,
    ) -> Result<Option<anyhow::Error>> {
        while_logging(self.record(shared, board, clean), shared, board, log).await
    }
    async fn tick(
        &mut self,
        shared: &Shared,
        board: &BoardConfig,
        log: &mut ClientLog,
    ) -> Result<()> {
        if !self.automatic {
            return Ok(());
        }
        let now = crate::unix_now();
        let gate = shared.runtime.lock().expect("runtime").experiments.clone();
        if gate.admission_pending() {
            self.policy.interrupt_trial(now);
            self.experiment.take();
        }
        if self.experiment.is_none() {
            self.experiment = gate.trial();
        }
        let input = {
            let state = shared.runtime.lock().expect("runtime");
            let hardware = state
                .boards
                .get(&board.id)
                .context("Automatic board state missing")?;
            if self.engines == 1
                && hardware
                    .frequency_mhz
                    .is_some_and(|f| (f - self.policy.point.frequency_mhz).abs() <= 1.)
            {
                self.raw_window.observe(
                    self.policy.epoch,
                    now,
                    hardware.recoveries,
                    &hardware.raw_speeds,
                );
            }
            let chains: Vec<_> = shared
                .assignments
                .iter()
                .filter(|a| a.board_id == board.id)
                .map(|a| a.chain)
                .collect();
            let latest = state
                .observations
                .iter()
                .rev()
                .find(|o| chains.contains(&o.chain))
                .map(|o| o.timestamp_unix);
            let score = automatic::phase_score(
                &state.observations,
                &chains,
                self.policy
                    .epoch
                    .max(now.saturating_sub(self.policy.settings.recovery_window_seconds)),
                self.policy.settings.minimum_proofs_per_chain,
            );
            automatic::Input {
                exploration_allowed: self.experiment.is_some()
                    && !gate.admission_pending()
                    && shared.config.hardware.boards.iter().all(|b| {
                        state.boards.get(&b.id).is_some_and(|h| {
                            h.native_initialized
                                && h.state == "connected"
                                && !h.latched_fault
                                && now.saturating_sub(h.temperature_unix) <= 30
                        })
                    }),
                healthy: state.phase == "running"
                    && ["ready", "authenticated"].contains(&state.node.as_str())
                    && latest.is_some_and(|t| now.saturating_sub(t) <= 30)
                    && now.saturating_sub(hardware.temperature_unix) <= 30,
                recoveries: hardware.recoveries,
                score,
                temperature: hardware.temperature_c,
                temperature_limit: board.temperature_limit_c,
                paused: state.tuning_paused,
            }
        };
        let healthy = input.healthy && !input.paused;
        match self.policy.observe(now, input) {
            Some(Action::Save) => {
                if let Some(error) = self
                    .record_with_logs(
                        shared,
                        board,
                        self.policy
                            .status
                            .active_seconds
                            .max(self.policy.settings.healthy_seconds),
                        log,
                    )
                    .await?
                {
                    tracing::warn!(%error,"Automatic profile checkpoint unavailable; proof work continues");
                }
            }
            Some(Action::Set(point, reason)) => {
                let persist = !self.policy.in_trial();
                if self.policy.is_retest() {
                    // Observe has selected a target but point is still the last
                    // accepted setting. Persist its reserved retry wait first.
                    if let Some(error) = self.record_with_logs(shared, board, 0, log).await? {
                        tracing::warn!(%error,"Retest reservation unavailable; current point held");
                        self.policy.interrupt_trial(now);
                        self.experiment.take();
                        return Ok(());
                    }
                }
                if let Some(error) = journal_point(shared, board, point, log).await? {
                    tracing::warn!(%error,board=%board.name,"Operating-point journal unavailable");
                    if point.frequency_mhz > self.policy.point.frequency_mhz
                        || point.voltage_v > self.policy.point.voltage_v
                    {
                        self.policy.interrupt_trial(now);
                        self.experiment.take();
                        return Ok(());
                    }
                    // before_open retains restrictive safety state in memory
                    // even if the disk write fails; contention is awaited above.
                }
                tracing::info!(board=%board.name, frequency_mhz=point.frequency_mhz, voltage_v=point.voltage_v, %reason, "Applying automatic setting");
                let actual = self
                    .change_with_logs(point, 5, log, shared, board)
                    .await
                    .map_err(|error| anyhow::anyhow!("{}: {error}", board.name))?;
                if let Some(metrics) = &shared.runtime.lock().expect("runtime").metrics {
                    let kind = if !persist {
                        "trial"
                    } else if reason.to_ascii_lowercase().contains("restor") {
                        "restore"
                    } else {
                        "backoff"
                    };
                    metrics.inc(
                        "operating_point_changes_total",
                        &[
                            &crate::observability::board(&shared.config, &board.id),
                            kind,
                        ],
                    );
                }
                self.policy.applied(actual, crate::unix_now(), reason);
                self.policy.synchronize_recoveries(
                    shared.runtime.lock().expect("runtime").boards[&board.id].recoveries,
                );
                // Save rollback limits/cooldown, never an outstanding experiment.
                // Zero clean seconds keeps the resumed point explicitly provisional.
                if persist && let Some(error) = self.record_with_logs(shared, board, 0, log).await?
                {
                    tracing::warn!(%error,"Backoff profile checkpoint unavailable; proof work continues");
                }
                self.raw_window = Default::default();
                let mut state = shared.runtime.lock().expect("runtime");
                state.event(Some(board.id.clone()), "profile_change");
                let hardware = state.boards.entry(board.id.clone()).or_default();
                hardware.frequency_mhz = Some(actual.frequency_mhz);
                hardware.voltage_v = Some(actual.voltage_v);
            }
            None => {
                // Refresh only an accepted, healthy point; never persist a trial
                // merely because it produced a few fast raw samples.
                if healthy
                    && !self.policy.in_trial()
                    && ["monitoring", "cooldown"].contains(&self.policy.status.state.as_str())
                    && self.policy.status.active_seconds >= board.automatic.healthy_seconds
                    && now.saturating_sub(self.last_record_unix) >= 300
                    && (self.raw_window.performance().is_some()
                        || self.policy.retest_remaining().is_some())
                    && let Some(error) = self
                        .record_with_logs(shared, board, self.policy.status.active_seconds, log)
                        .await?
                {
                    tracing::warn!(%error,"Profile refresh unavailable; proof work continues");
                }
            }
        }
        if !self.policy.in_trial() {
            self.experiment.take();
        }
        shared
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .entry(board.id.clone())
            .or_default()
            .automatic = Some(self.policy.status.clone());
        Ok(())
    }
}

async fn supervise(
    shared: Arc<Shared>,
    board: BoardConfig,
    mut child: Child,
    ready: oneshot::Sender<()>,
    mut shutdown: watch::Receiver<bool>,
    initialization: tokio::sync::MutexGuard<'static, ()>,
    preparing: Option<timelord_supervisor::progress::Guard>,
) -> Result<()> {
    let owner = tokio::select! {
        result=NativeOwner::new(&shared,&board,&mut child)=>result,
        _=shutdown.changed()=>{terminate(&mut child).await;return Ok(());}
    };
    let mut owner = match owner {
        Ok(owner) => owner,
        Err(error) => {
            terminate(&mut child).await;
            let mut state = shared.runtime.lock().expect("runtime");
            let hardware = state.boards.entry(board.id.clone()).or_default();
            hardware.state = "fault".into();
            hardware.fault_reason = Some(error.to_string());
            return Err(error);
        }
    };
    let output = child.stderr.take().context("ASIC client log unavailable")?;
    let mut log = ClientLog::new(output, ready);
    log.initialization = Some(initialization);
    log.preparing = preparing;
    let mut watchdog = tokio::time::interval(Duration::from_secs(5));
    let result: Result<()> = loop {
        tokio::select! {
            _ = shutdown.changed() => break Ok(()),
            _ = watchdog.tick() => {
                if let Some(hardware) = shared.runtime.lock().expect("runtime").boards.get(&board.id)
                    && (hardware.latched_fault || hardware.state == "fault") {
                    break Err(anyhow::anyhow!(hardware.fault_reason.clone().unwrap_or_else(|| "ASIC needs attention".into())));
                }
                if log.last_temperature.elapsed() > Duration::from_secs(TEMPERATURE_TIMEOUT_SECONDS) { break Err(anyhow::anyhow!("ASIC temperature telemetry stopped")); }
                match child.try_wait() {
                    Ok(Some(exit)) => break Err(anyhow::anyhow!("ASIC client exited: {exit}")),
                    Err(error) => break Err(error.into()),
                    Ok(None) => {}
                }
                if let Err(error)=owner.tick(&shared,&board,&mut log).await {break Err(error);}
            }
            result = log.read(&shared, &board) => {
                let recovery = match result { Ok(recovery) => recovery, Err(error) => break Err(error) };
                if recovery && let Err(error)=owner.tick(&shared,&board,&mut log).await {break Err(error);}
            }
        }
    };
    let quiesced = owner.stop(&mut child, log).await;
    if !quiesced {
        tracing::warn!(board=%board.name,"Native shutdown lacked confirmed quiescence; owner is unclean");
    }
    let mut state = shared.runtime.lock().expect("runtime");
    let hardware = state.boards.entry(board.id.clone()).or_default();
    hardware.quiesced = quiesced;
    hardware.raw_speeds.clear();
    if hardware.state != "thermal_stop" {
        hardware.state = if result.is_ok() { "stopped" } else { "fault" }.into();
    }
    if let Err(error) = &result {
        state
            .boards
            .entry(board.id.clone())
            .or_default()
            .fault_reason = Some(error.to_string());
    }
    result
}
async fn supported_frequencies(config: &Config) -> Result<Vec<f64>> {
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new(
            config
                .hardware
                .client_binary
                .as_ref()
                .context("Choose a native client")?,
        )
        .arg("--control-frequencies")
        .kill_on_drop(true)
        .output(),
    )
    .await
    .context("Native frequency capability check timed out")??;
    if !output.status.success() || output.stdout.len() > 2048 {
        bail!("Native client could not report supported Automatic frequencies");
    }
    decode_frequencies(&output.stdout)
}

fn decode_frequencies(bytes: &[u8]) -> Result<Vec<f64>> {
    let frequencies: Vec<f64> = serde_json::from_slice(bytes)
        .context("Native client needs the bundled supported-frequency capability")?;
    if frequencies.is_empty()
        || frequencies.len() > 201
        || frequencies
            .iter()
            .any(|f| !f.is_finite() || !(200. ..=2200.).contains(f))
        || frequencies.windows(2).any(|f| f[0] >= f[1])
    {
        bail!("Native client reported invalid supported frequencies");
    }
    Ok(frequencies)
}

pub async fn validate_automatic(
    config: &Config,
    assignments: &[timelord_core::Assignment],
) -> Result<()> {
    if !config
        .hardware
        .boards
        .iter()
        .any(|b| b.profile == ProfileMode::Automatic)
    {
        return Ok(());
    }
    let binary = config
        .hardware
        .client_binary
        .as_ref()
        .context("Automatic mode requires a native client")?;
    let probe = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new(binary)
            .arg("--control-version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Native control capability check timed out")??;
    if !probe.status.success() || probe.stdout != b"timelord-live-control-v1\n" {
        bail!(
            "Automatic mode needs the bundled live-control native client. Select it in board settings, or use Manual settings."
        );
    }
    let frequencies = supported_frequencies(config).await?;
    let hash = automatic::fingerprint(config)?;
    let host = automatic::host();
    for board in &config.hardware.boards {
        if board.profile == ProfileMode::Automatic {
            if board.serial.as_ref().is_none_or(|s| s.is_empty()) {
                bail!(
                    "Automatic mode needs a persistent serial for {}",
                    board.name
                );
            }
            let point = automatic::initial(
                config,
                board,
                assignments
                    .iter()
                    .filter(|a| a.board_id == board.id)
                    .count(),
                &hash,
                &host,
            )?;
            if !frequencies
                .iter()
                .any(|f| (*f - point.frequency_mhz).abs() <= 1.)
            {
                bail!(
                    "{}: Initial Automatic frequency {} MHz is not supported by this native client",
                    board.name,
                    point.frequency_mhz
                );
            }
        }
    }
    Ok(())
}

pub(crate) const TEMPERATURE_TIMEOUT_SECONDS: u64 = 60;

const RESTART_DELAYS: [u64; 3] = [1, 5, 30];
struct RestartBackoff {
    used: usize,
    delays: [Duration; 3],
}
impl Default for RestartBackoff {
    fn default() -> Self {
        Self {
            used: 0,
            delays: RESTART_DELAYS.map(Duration::from_secs),
        }
    }
}
impl RestartBackoff {
    fn next(&mut self, established: bool) -> Duration {
        if established {
            self.used = 0;
        }
        let delay = self.delays[self.used.min(self.delays.len() - 1)];
        self.used = self.used.saturating_add(1);
        delay
    }
}
fn established(shared: &Shared, board: &BoardConfig, started: u64, elapsed: Duration) -> bool {
    if elapsed < Duration::from_secs(600) {
        return false;
    }
    let now = crate::unix_now();
    let state = shared.runtime.lock().expect("runtime");
    let telemetry_fresh = state
        .boards
        .get(&board.id)
        .is_some_and(|b| now.saturating_sub(b.temperature_unix) <= 30);
    telemetry_fresh
        && shared
            .assignments
            .iter()
            .filter(|a| a.board_id == board.id)
            .all(|a| {
                let proofs: Vec<_> = state
                    .observations
                    .iter()
                    .filter(|o| o.chain == a.chain && o.work_started_unix >= started)
                    .collect();
                proofs.len() >= 3
                    && proofs
                        .last()
                        .is_some_and(|o| now.saturating_sub(o.timestamp_unix) <= 30)
            })
}
async fn launch(shared: &Shared, board: &BoardConfig) -> Result<Child> {
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
fn thermal_rejection(error: &anyhow::Error) -> bool {
    let reason = error.to_string();
    reason.contains(": temperature_limit")
        || reason.contains(": temperature_guard")
        || reason.contains("configured temperature limit")
}

fn checkpoint_retired(
    shared: &Shared,
    board: &BoardConfig,
    store: &crate::safety::Store,
    serial: &str,
) -> Result<()> {
    let (quiesced, latched, thermal) = {
        let state = shared.runtime.lock().expect("runtime");
        let hardware = state
            .boards
            .get(&board.id)
            .context("Retired board missing")?;
        (
            hardware.quiesced,
            hardware.latched_fault,
            hardware.state == "thermal_stop",
        )
    };
    if latched {
        store.quarantine(
            serial,
            if thermal {
                crate::safety::Reason::Thermal
            } else {
                crate::safety::Reason::InvalidProof
            },
        )?;
        if quiesced {
            store.closed(serial)?;
        }
    } else if quiesced {
        store.closed(serial)?;
    } else {
        store.quarantine(serial, crate::safety::Reason::UncleanOwner)?;
    }
    Ok(())
}
async fn supervise_board(
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    mut board: BoardConfig,
    ready: oneshot::Sender<()>,
    mut shutdown: watch::Receiver<bool>,
    mut budget: RestartBackoff,
) {
    let safety = shared.runtime.lock().expect("runtime").safety.clone();
    let mut ready = Some(ready);
    let mut previous_failure = "topology";
    loop {
        if *shutdown.borrow() {
            return;
        }
        if let (Some(store), Some(serial)) = (&safety, &board.serial)
            && let Some(record) = store.board(serial)
            && record.quarantine.is_some()
        {
            let point = Point {
                frequency_mhz: board.frequency_mhz,
                voltage_v: board.voltage_v,
            };
            let manual_invalid = board.profile == ProfileMode::Manual
                && record.quarantine == Some(crate::safety::Reason::InvalidProof)
                && record.last_point.is_none_or(|old| old == point);
            if manual_invalid || record.retry_after_unix > crate::unix_now() {
                ready.take();
                {
                    let mut state = shared.runtime.lock().expect("runtime");
                    let hardware = state.boards.entry(board.id.clone()).or_default();
                    hardware.latched_fault = true;
                    hardware.state = "quarantined".into();
                    hardware.fault_reason = Some(if manual_invalid {
                        "Invalid proof: change the Manual operating point or select Automatic before retrying.".into()
                    } else {
                        format!(
                            "Waiting for fresh admission after {:?}",
                            record.quarantine.unwrap()
                        )
                    });
                }
                tokio::select! {_=shutdown.changed()=>return,_=tokio::time::sleep(Duration::from_secs(5))=>{}}
                continue;
            }
        }
        {
            let mut state = shared.runtime.lock().expect("runtime");
            let hardware = state.boards.entry(board.id.clone()).or_default();
            if hardware.latched_fault && safety.is_none() {
                return;
            }
            hardware.generation += 1;
            hardware.started_unix = crate::unix_now();
            hardware.state = "starting".into();
            hardware.native_initialized = false;
            hardware.quiesced = false;
            hardware.raw_speeds.clear();
            hardware.temperature_unix = 0;
            hardware.temperature_c = None;
            hardware.fault_reason = None;
        }
        let started = tokio::time::Instant::now();
        let started_unix = crate::unix_now();
        let tx = ready.take().unwrap_or_else(|| oneshot::channel().0);
        // D2XX enumeration and opening must not race another board's initialization.
        let initialization = tokio::select! {
            guard = INITIALIZATION.lock() => guard,
            _ = shutdown.changed() => return,
        };
        let progress = shared.runtime.lock().expect("runtime").progress.clone();
        let board_index = shared
            .config
            .hardware
            .boards
            .iter()
            .position(|b| b.id == board.id)
            .unwrap_or(0);
        let mut preparing = progress.as_ref().map(|p| {
            p.begin(
                17 + board_index,
                0,
                0,
                timelord_supervisor::progress::Stage::BoardPreparation,
            )
        });
        let preparation: Result<()> = tokio::select! {
            result = async {
                if board.serial.is_some() {
                    let devices = crate::inventory::discover(&shared.config).await?;
                    // This owner's old process has exited, so its own open FTDI
                    // handle cannot be hiding metadata. Only this observation
                    // may declare an already-managed board absent.
                    let matches = devices.iter().filter(|d| d.serial == board.serial).count();
                    shared.runtime.lock().expect("runtime").boards.get_mut(&board.id).expect("board").usb_present = match matches {
                        0 => Some(false), 1 => Some(true), _ => None,
                    };
                    board.device_location = Some(crate::inventory::board_location(&board, &devices)?);
                }
                shared.runtime.lock().expect("runtime").boards.get_mut(&board.id).expect("board").device_location = board.device_location;
                Ok(())
            } => result,
            _ = shutdown.changed() => return,
        };
        let result: Result<()> = async {
            preparation?;
            let mut initialization = initialization;
            let engines = shared
                .assignments
                .iter()
                .filter(|a| a.board_id == board.id)
                .count();
            let _lease = match (&safety, &board.serial) {
                (Some(store), Some(serial)) => Some(store.lease(serial)?),
                (Some(_), None) => bail!("Managed ASIC needs a persistent serial identity"),
                _ => None,
            };
            if let (Some(store), Some(serial)) = (&safety, &board.serial) {
                let prospective = if engines > 0 {
                    engines
                } else {
                    let blocked = unavailable(&shared);
                    let mut candidate = shared.config.clone();
                    candidate
                        .hardware
                        .boards
                        .retain(|b| b.id == board.id || !blocked.contains(&b.id));
                    candidate.hardware.chain_assignments = None;
                    crate::assignment::resolve(&candidate)?
                        .iter()
                        .filter(|a| a.board_id == board.id)
                        .count()
                };
                let q = admission::qualification(&shared, &board, prospective)?;
                if engines == 0 || store.needs_self_test(serial, &q) {
                    store.before_open(serial, q.point)?;
                    {
                        let mut state = shared.runtime.lock().expect("runtime");
                        state.boards.entry(board.id.clone()).or_default().state = "checking".into();
                        state.event(Some(board.id.clone()), "admission_check");
                    }
                    let checked = admission::check(
                        &shared,
                        &board,
                        &q,
                        initialization,
                        admission::Rules::default(),
                        shutdown.clone(),
                        preparing,
                    )
                    .await;
                    if matches!(checked, Ok(admission::Outcome::Cancelled)) {
                        // Admission reports cancellation only after its owner has
                        // retired with confirmed quiescence (or before any open).
                        store.closed(serial)?;
                        return Ok(());
                    }
                    if let Err(error) = checked {
                        let reason = if error
                            .downcast_ref::<crate::asic::AsicFailure>()
                            .is_some_and(|e| e.verification)
                        {
                            crate::safety::Reason::InvalidProof
                        } else if thermal_rejection(&error) {
                            crate::safety::Reason::Thermal
                        } else {
                            crate::safety::Reason::UncleanOwner
                        };
                        if let Err(saved) = store.quarantine(serial, reason) {
                            tracing::error!(%saved,"Admission fault could not be checkpointed");
                        }
                        return Err(error);
                    }
                    store.qualify(serial, q.clone())?;
                    {
                        let mut state = shared.runtime.lock().expect("runtime");
                        let hardware = state.boards.entry(board.id.clone()).or_default();
                        hardware.latched_fault = false;
                        hardware.unavailable_since_unix = None;
                        hardware.state = "admitted".into();
                        hardware.usb_present = None;
                        hardware.fault_reason = None;
                    }
                    if engines == 0 {
                        let _ = tx.send(());
                        // A new usable-capacity generation will assign this board.
                        let _ = shutdown.changed().await;
                        return Ok(());
                    }
                    initialization = INITIALIZATION.lock().await;
                    preparing = progress.as_ref().map(|p| {
                        p.begin(
                            17 + board_index,
                            0,
                            0,
                            timelord_supervisor::progress::Stage::BoardPreparation,
                        )
                    });
                    let devices = crate::inventory::discover(&shared.config).await?;
                    board.device_location =
                        Some(crate::inventory::board_location(&board, &devices)?);
                }
                if *shutdown.borrow() {
                    return Ok(());
                }
                let point = if board.profile == ProfileMode::Automatic {
                    startup_point(&shared, &board, engines, &q.client_sha256, &q.host)?
                } else {
                    let (frequency_mhz, voltage_v) = board.operating_point();
                    Point {
                        frequency_mhz,
                        voltage_v,
                    }
                };
                store.before_open(serial, point)?;
                let mut state = shared.runtime.lock().expect("runtime");
                let hardware = state.boards.entry(board.id.clone()).or_default();
                hardware.state = "starting".into();
                hardware.native_initialized = false;
                hardware.quiesced = false;
            }
            let child = launch(&shared, &board).await?;
            if let Some(metrics) = &shared.runtime.lock().expect("runtime").metrics {
                metrics.client_started(
                    &crate::observability::board(&shared.config, &board.id),
                    previous_failure,
                );
            }
            let result = supervise(
                shared.clone(),
                board.clone(),
                child,
                tx,
                shutdown.clone(),
                initialization,
                preparing,
            )
            .await;
            if let (Some(store), Some(serial)) = (&safety, &board.serial) {
                checkpoint_retired(&shared, &board, store, serial)?;
            }
            result
        }
        .await;
        if *shutdown.borrow() || result.is_ok() {
            return;
        }
        let error = result.unwrap_err();
        previous_failure = crate::observability::restart_reason(&error.to_string());
        {
            let state = shared.runtime.lock().expect("runtime");
            if let Some(metrics) = &state.metrics {
                let label = crate::observability::board(&shared.config, &board.id);
                if thermal_rejection(&error)
                    && !state.boards.get(&board.id).is_some_and(|b| b.latched_fault)
                {
                    metrics.inc("thermal_stops_total", &[&label]);
                }
            }
        }
        let delay = budget.next(established(
            &shared,
            &board,
            started_unix,
            started.elapsed(),
        ));
        let stop = {
            let mut state = shared.runtime.lock().expect("runtime");
            let hardware = state.boards.entry(board.id.clone()).or_default();
            // Missing USB/sensor data is recoverable. Explicit overtemperature and
            // invalid verified proofs remain latched. Old ambiguous guards fail closed.
            let thermal = thermal_rejection(&error);
            hardware.latched_fault |= thermal;
            hardware
                .unavailable_since_unix
                .get_or_insert(crate::unix_now());
            hardware.state = if thermal || hardware.state == "thermal_stop" {
                "thermal_stop"
            } else if hardware.latched_fault {
                "fault"
            } else {
                "reconnecting"
            }
            .into();
            hardware.raw_speeds.clear();
            hardware.temperature_c = None;
            hardware.temperature_unix = 0;
            hardware.fault_reason = Some(error.to_string());
            let stop = hardware.latched_fault;
            if !state
                .boards
                .iter()
                .any(|(id, b)| id != &board.id && b.state == "connected")
            {
                state.phase = "waiting_for_clients".into();
                state.message = if stop {
                    format!("{} needs attention: {error}", board.name)
                } else {
                    format!("Recovering {}: {error}", board.name)
                };
            }
            if !state.events.iter().any(|e| {
                e.board_id.as_ref() == Some(&board.id)
                    && e.kind == "board_fault"
                    && e.end_unix.is_none()
            }) {
                state.event(Some(board.id.clone()), "board_fault");
            }
            stop
        };
        backend.stop_board(&board.id).await;
        tracing::warn!(board=%board.name, %error, attempt=budget.used, retry_seconds=delay.as_secs(), stop, "ASIC client stopped; other boards remain supervised");
        if thermal_rejection(&error)
            && let (Some(store), Some(serial)) = (&safety, &board.serial)
            && let Err(saved) = store.quarantine(serial, crate::safety::Reason::Thermal)
        {
            tracing::error!(%saved,"Thermal fault could not be checkpointed");
        }
        if stop && safety.is_none() {
            return;
        }
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}
pub(crate) fn unavailable(shared: &Shared) -> std::collections::HashSet<String> {
    let (boards, safety) = {
        let state = shared.runtime.lock().expect("runtime");
        (state.boards.clone(), state.safety.clone())
    };
    unavailable_for(&shared.config, &boards, safety.as_deref())
}
pub(crate) fn unavailable_for(
    config: &Config,
    boards: &std::collections::HashMap<String, HardwareState>,
    safety: Option<&crate::safety::Store>,
) -> std::collections::HashSet<String> {
    config
        .hardware
        .boards
        .iter()
        .filter(|board| {
            boards.get(&board.id).is_some_and(|h| {
                h.latched_fault
                    || (h.state == "admitted" && h.usb_present != Some(true))
                    || h.unavailable_since_unix
                        .is_some_and(|since| crate::unix_now().saturating_sub(since) >= 60)
            }) || safety
                .zip(board.serial.as_deref())
                .and_then(|(store, serial)| store.board(serial))
                .is_some_and(|b| b.quarantine.is_some())
        })
        .map(|b| b.id.clone())
        .collect()
}
pub async fn run(
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    validate_automatic(&shared.config, &shared.assignments).await?;
    let mut children = JoinSet::new();
    let (stop, child_shutdown) = watch::channel(false);
    let managed = shared.runtime.lock().expect("runtime").safety.is_some();
    for board in &shared.config.hardware.boards {
        if !shared.assignments.iter().any(|a| a.board_id == board.id) && !managed {
            continue;
        }
        if shared
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .get(&board.id)
            .is_some_and(|b| b.latched_fault)
            && !managed
        {
            continue;
        }
        shared
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .entry(board.id.clone())
            .or_default()
            .state = "starting".into();
        let (tx, rx) = oneshot::channel();
        children.spawn(supervise_board(
            shared.clone(),
            backend.clone(),
            board.clone(),
            tx,
            child_shutdown.clone(),
            RestartBackoff::default(),
        ));
        // Keep normal USB initialization sequential, but one broken board must
        // not prevent the next board from starting or bring down existing owners.
        tokio::select! {
            _ = shutdown.changed() => break,
            initialized = tokio::time::timeout(Duration::from_secs(30), rx) => {
                if !matches!(initialized, Ok(Ok(()))) {
                    tracing::warn!(board=%board.name, "ASIC not ready yet; continuing other boards");
                }
            }
        }
    }
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            Some(result) = children.join_next(), if !children.is_empty() => {
                if let Err(error) = result {
                    if let Some(metrics)=&shared.runtime.lock().expect("runtime").metrics {metrics.failed.store(true,std::sync::atomic::Ordering::Relaxed);}
                    tracing::error!(%error, "ASIC supervisor task failed");
                    return Err(error.into());
                }
            }
        }
    }
    let _ = stop.send(true);
    while let Some(result) = children.join_next().await {
        if let Err(error) = result {
            tracing::warn!(%error, "ASIC shutdown failed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn capability_accepts_the_complete_2200_mhz_grid_and_rejects_invalid_lists() {
        let full: Vec<f64> = (200..=2200).step_by(10).map(f64::from).collect();
        assert_eq!(
            super::decode_frequencies(&serde_json::to_vec(&full).unwrap()).unwrap(),
            full
        );
        for invalid in [
            vec![],
            vec![200., 2201.],
            vec![200., 200.],
            vec![2200., 200.],
            vec![200.; 202],
        ] {
            assert!(super::decode_frequencies(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
    }

    #[test]
    fn raw_speed_telemetry_is_bounded_and_engine_specific() {
        use super::RawState::*;
        assert_eq!(
            super::raw_speed("time TLRAW 2 2 1055000 running"),
            Some((2, 1055000., Running))
        );
        assert_eq!(
            super::raw_speed("TLRAW 2 0 -1 sampling"),
            Some((0, -1., Sampling))
        );
        assert_eq!(super::raw_speed("TLRAW 2 0 -1 idle"), Some((0, -1., Idle)));
        for line in [
            "TLRAW 2 3 10 running",
            "TLRAW 2 0 NaN running",
            "TLRAW 2 0 inf running",
            "TLRAW 2 0 -2 idle",
            "TLRAW 2 0 12 running extra",
            "TLRAW 1 0 10",
            "TLRAW 2 0 -1 running",
            "TLRAW 2 0 12 idle",
        ] {
            assert!(super::raw_speed(line).is_none(), "{line}");
        }
    }
    #[test]
    fn handoff_preserves_last_raw_value_without_refreshing_its_age_or_calling_it_current() {
        use super::RawState::*;
        let mut h = super::HardwareState::default();
        h.raw_sample(0, 1055000., Running, 100);
        h.raw_sample(0, -1., Sampling, 101);
        assert_eq!(h.raw_speeds[0].ips, 1055000.);
        assert_eq!(h.raw_speeds[0].timestamp_unix, 100);
        assert_eq!(h.raw_speeds[0].state, Sampling);
        h.raw_sample(0, -1., Idle, 102);
        assert_eq!(h.raw_speeds[0].state, Idle);
        h.raw_sample(1, -1., Sampling, 103);
        assert_eq!(h.raw_speeds[1].ips, -1.);
        h.raw_sample(0, 1060000., Running, 104);
        assert_eq!(h.raw_speeds[0].timestamp_unix, 104);
        assert_eq!(h.raw_speeds[0].state, Running);
        assert_eq!(h.raw_speeds.len(), 2);
    }

    use super::*;
    #[tokio::test]
    async fn native_control_requires_matching_acknowledgements_and_keeps_hard_limits() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!("tl-ack-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let binary = directory.join("fake-client");
        std::fs::write(&binary,"#!/bin/sh\nif [ \"$1\" = --control-frequencies ]; then echo '[1450,1460,1470,1480,1490]'; exit 0; fi\nwhile read command version id frequency voltage; do\n  if [ \"$id\" = 4 ]; then printf '%0600d\\n' 0; continue; fi\n  if [ \"$id\" = 3 ]; then id=999; fi\n  if [ \"$id\" = 5 ]; then printf 'TLERR 1 5 readback frequency=0.000000\\n'; continue; fi\n  printf 'TLACK 1 %s %s %s\\n' \"$id\" \"$frequency\" \"$voltage\"\ndone\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config:Config=toml::from_str("[[hardware.boards]]\nid='a'\nname='A'\nclient_port=18110\ndevice_location=4113\nserial='a'\nprofile='automatic'").unwrap();
        config.hardware.client_binary = Some(binary.clone());
        config.hardware.automatic_state = Some(directory.join("cache.json"));
        let board = config.hardware.boards[0].clone();
        let args = arguments(&board, 3).unwrap();
        assert!(args.windows(2).any(|a| a == ["--freq", "1450"]));
        assert!(args.windows(2).any(|a| a == ["--voltage", "1"]));
        assert!(args.contains(&"--live-control".into()));
        let shared = Shared {
            assignments: config.assignments().unwrap(),
            config,
            clients: Default::default(),
            runtime: Default::default(),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        };
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut owner = NativeOwner::new(&shared, &board, &mut child).await.unwrap();
        assert_eq!(owner.policy.point.frequency_mhz, 1450.);
        assert!(
            owner
                .change(
                    Point {
                        frequency_mhz: 2201.,
                        voltage_v: 1.
                    },
                    1
                )
                .await
                .is_err()
        );
        assert_eq!(
            owner
                .change(
                    Point {
                        frequency_mhz: 1460.,
                        voltage_v: 1.
                    },
                    1
                )
                .await
                .unwrap()
                .frequency_mhz,
            1460.
        );
        assert!(
            owner
                .change(
                    Point {
                        frequency_mhz: 1470.,
                        voltage_v: 1.
                    },
                    1
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        assert!(
            owner
                .change(
                    Point {
                        frequency_mhz: 1480.,
                        voltage_v: 1.
                    },
                    1
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("too large")
        );
        assert!(
            owner
                .change(
                    Point {
                        frequency_mhz: 1490.,
                        voltage_v: 1.
                    },
                    1
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("readback frequency=0.000000")
        );
        terminate(&mut child).await;
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn live_backoff_drains_a_full_stderr_pipe_before_acknowledgement() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tl-storm-pipe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("client");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
if [ "$1" = --control-frequencies ]; then echo '[1430,1450,1460]'; exit 0; fi
while read command version id frequency voltage; do
 if [ "$id" = 2 ]; then
  n=0
  while [ "$n" -lt 5000 ]; do
   printf '%s\n' 'VDF 0: Restarting VDF at 123456789 iters (fixture recovery log flood)' >&2
   n=$((n + 1))
  done
 fi
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config: Config = toml::from_str("[[hardware.boards]]\nid='storm'\nname='Storm'\nclient_port=18110\ndevice_location=1\nserial='fixture-only'\n").unwrap();
        config.hardware.client_binary = Some(binary.clone());
        let board = config.hardware.boards[0].clone();
        let shared = Shared {
            assignments: config.assignments().unwrap(),
            config,
            clients: Default::default(),
            runtime: Default::default(),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        };
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut owner = NativeOwner::new(&shared, &board, &mut child).await.unwrap();
        let (ready, _receiver) = oneshot::channel();
        let mut log = ClientLog::new(child.stderr.take().unwrap(), ready);
        let result = owner
            .change_with_logs(
                Point {
                    frequency_mhz: 1430.,
                    voltage_v: 1.,
                },
                5,
                &mut log,
                &shared,
                &board,
            )
            .await;
        assert_eq!(result.unwrap().frequency_mhz, 1430.);
        while shared.runtime.lock().unwrap().boards["storm"].recoveries < 5000 {
            tokio::time::timeout(Duration::from_secs(1), log.read(&shared, &board))
                .await
                .unwrap()
                .unwrap();
        }
        terminate(&mut child).await;
        assert_eq!(
            shared.runtime.lock().unwrap().boards["storm"].recoveries,
            5000
        );
        assert_eq!(log.recent.len(), 8);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn healthy_journal_failure_does_not_fail_profile_record_or_retire_owner() {
        let dir = std::env::temp_dir().join(format!("tl-healthy-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        let store = Arc::new(crate::safety::Store::open(&config_path).unwrap());
        let point = Point {
            frequency_mhz: 1450.,
            voltage_v: 1.,
        };
        store.before_open("fixture", point).unwrap();
        store
            .quarantine("fixture", crate::safety::Reason::UncleanOwner)
            .unwrap();
        let paths = timelord_supervisor::Paths::for_config(&config_path).unwrap();
        let journal = paths.directory.join("board-safety.json");
        std::fs::remove_file(&journal).unwrap();
        std::fs::create_dir(&journal).unwrap(); // Force the atomic rename to fail.
        let board: BoardConfig =
            toml::from_str("id='a'\nname='A'\nserial='fixture'\nclient_port=18110").unwrap();
        let mut config = Config::default();
        config.hardware.automatic_state = Some(dir.join("learned.json"));
        let runtime = crate::runtime::RuntimeState {
            safety: Some(store.clone()),
            ..Default::default()
        };
        let shared = Shared {
            config,
            assignments: vec![],
            clients: Default::default(),
            runtime: Arc::new(std::sync::Mutex::new(runtime)),
            started_unix: 0,
            service_started_unix: 0,
        };
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut owner = NativeOwner {
            experiment: None,
            automatic: true,
            policy: Policy::new(
                board.automatic.clone(),
                point,
                crate::unix_now(),
                vec![1450.],
            ),
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            sequence: 0,
            serial: "fixture".into(),
            engines: 1,
            hash: "a".repeat(64),
            host: "fixture".into(),
            raw_window: Default::default(),
            last_record_unix: 0,
        };
        let (ready, _ready) = oneshot::channel();
        let mut log = ClientLog::new(child.stderr.take().unwrap(), ready);
        assert!(
            owner
                .record_with_logs(&shared, &board, board.automatic.healthy_seconds, &mut log)
                .await
                .unwrap()
                .is_some()
        );
        assert!(store.board("fixture").unwrap().force_safe);
        assert!(child.try_wait().unwrap().is_none());
        assert!(dir.join("learned.json").is_file());
        child.kill().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn eof_drains_large_native_output_and_reaps_a_confirmed_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tl-eof-drain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("client");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
while read command version id frequency voltage; do
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
done
n=0
while [ "$n" -lt 4000 ]; do
 printf '%s\n' 'pending stdout pending stdout pending stdout pending stdout'
 printf '%s\n' 'pending stderr pending stderr pending stderr pending stderr' >&2
 n=$((n + 1))
done
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let board: BoardConfig = toml::from_str(
            "id='eof'\nname='EOF'\nclient_port=18110\ndevice_location=1\nprofile='manual'",
        )
        .unwrap();
        let mut config = Config::default();
        config.hardware.boards = vec![board.clone()];
        config.hardware.client_binary = Some(binary);
        let shared = Shared {
            assignments: config.assignments().unwrap(),
            config,
            clients: Default::default(),
            runtime: Default::default(),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        };
        let mut child = launch(&shared, &board).await.unwrap();
        let owner = NativeOwner::new(&shared, &board, &mut child).await.unwrap();
        let (ready, _rx) = oneshot::channel();
        let log = ClientLog::new(child.stderr.take().unwrap(), ready);
        assert!(owner.stop(&mut child, log).await);
        assert!(child.try_wait().unwrap().unwrap().success());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn thermal_shutdown_does_not_wait_for_the_safety_journal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tl-thermal-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("client");
        std::fs::write(&binary,r#"#!/bin/sh
while read command version id frequency voltage; do
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
 printf '%s\n' 'Connected to timelord, waiting for challenge' 'ASIC Temp = 60 C; Frequency = 1450 MHz' >&2
done
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let board:BoardConfig=toml::from_str("id='hot'\nname='Hot'\nserial='fixture'\nclient_port=18110\ndevice_location=1\nprofile='manual'").unwrap();
        let mut config = Config::default();
        config.hardware.client_binary = Some(binary);
        config.hardware.boards = vec![board.clone()];
        let store = Arc::new(crate::safety::Store::open(&dir.join("config.toml")).unwrap());
        store
            .before_open(
                "fixture",
                Point {
                    frequency_mhz: 1450.,
                    voltage_v: 1.,
                },
            )
            .unwrap();
        let runtime = crate::runtime::RuntimeState {
            safety: Some(store.clone()),
            ..Default::default()
        };
        let shared = Arc::new(Shared {
            assignments: config.assignments().unwrap(),
            config,
            clients: Default::default(),
            runtime: Arc::new(std::sync::Mutex::new(runtime)),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        });
        let initialization = INITIALIZATION.lock().await;
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, done) = std::sync::mpsc::channel();
        let held = store.clone();
        let holder = std::thread::spawn(move || held.held_fixture(entered, done));
        waiting.recv().unwrap();
        let before = tokio::time::Instant::now();
        let child = launch(&shared, &board).await.unwrap();
        let (ready, _ready) = oneshot::channel();
        let (_stop, rx) = watch::channel(false);
        let result = supervise(
            shared.clone(),
            board.clone(),
            child,
            ready,
            rx,
            initialization,
            None,
        )
        .await;
        let elapsed = before.elapsed();
        let _ = release.send(());
        holder.join().unwrap();
        let quiesced = shared.runtime.lock().unwrap().boards[&board.id].quiesced;
        checkpoint_retired(&shared, &board, &store, "fixture").unwrap();
        let record = store.board("fixture").unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_secs(1),
            "thermal stop waited {elapsed:?} for journal"
        );
        assert!(quiesced);
        assert_eq!(record.quarantine, Some(crate::safety::Reason::Thermal));
        assert!(!record.owner_active);
    }

    #[tokio::test]
    async fn operating_point_journal_waits_for_the_actual_writer_and_retains_its_slot() {
        let dir = std::env::temp_dir().join(format!("tl-critical-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(crate::safety::Store::open(&dir.join("config.toml")).unwrap());
        let board: BoardConfig =
            toml::from_str("id='a'\nname='A'\nserial='fixture'\nclient_port=18110").unwrap();
        let mut config = Config::default();
        config.hardware.boards.push(board.clone());
        let progress = Arc::new(timelord_supervisor::progress::Progress::default());
        let shared = Shared {
            assignments: config.assignments().unwrap(),
            config,
            clients: Default::default(),
            runtime: Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState {
                safety: Some(store.clone()),
                progress: Some(progress.clone()),
                ..Default::default()
            })),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        };
        let mut child = tokio::process::Command::new("/bin/cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (ready, _rx) = oneshot::channel();
        let mut log = ClientLog::new(child.stderr.take().unwrap(), ready);
        let mut second_board = board.clone();
        second_board.id = "b".into();
        second_board.serial = Some("second".into());
        store
            .before_open(
                "second",
                Point {
                    frequency_mhz: 1960.,
                    voltage_v: 1.,
                },
            )
            .unwrap();
        let mut second_child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (second_ready, _second_ready) = oneshot::channel();
        let mut second_log = ClientLog::new(second_child.stderr.take().unwrap(), second_ready);
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel();
        let saved = store.clone();
        let holder = std::thread::spawn(move || saved.held_fixture(entered, held));
        waiting.recv().unwrap();
        let point = Point {
            frequency_mhz: 1460.,
            voltage_v: 1.,
        };
        let pending = journal_point(&shared, &board, point, &mut log);
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(2100), &mut pending)
                .await
                .is_err(),
            "caller treated an unfinished critical journal as a completed failure"
        );
        assert!(progress.arithmetic(9).try_acquire_owned().is_err());
        let lower = Point {
            frequency_mhz: 1440.,
            voltage_v: 1.,
        };
        let second_pending = journal_point(&shared, &second_board, lower, &mut second_log);
        tokio::pin!(second_pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut second_pending)
                .await
                .is_err(),
            "another board skipped its journal instead of waiting for the writer"
        );
        release.send(()).unwrap();
        let (first_result, second_result) = tokio::join!(pending, second_pending);
        assert!(first_result.unwrap().is_none());
        assert!(second_result.unwrap().is_none());
        assert_eq!(store.board("second").unwrap().last_point, Some(lower));
        store
            .quarantine("second", crate::safety::Reason::Thermal)
            .unwrap();
        assert_eq!(
            store.board("second").unwrap().admission_frequency_mhz,
            Some(1390.)
        );
        second_child.kill().await.unwrap();
        holder.join().unwrap();
        assert_eq!(store.board("fixture").unwrap().last_point, Some(point));
        assert!(progress.arithmetic(9).try_acquire_owned().is_ok());
        let newer = Point {
            frequency_mhz: 1470.,
            voltage_v: 1.,
        };
        store.before_open("fixture", newer).unwrap();
        assert_eq!(store.board("fixture").unwrap().last_point, Some(newer));
        child.kill().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn optional_save_failure_keeps_the_owner_and_pending_io_still_reads_thermal_faults() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tl-optional-io-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("client");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
if [ "$1" = --control-frequencies ]; then echo '[1450,1460]'; exit 0; fi
while read command version id frequency voltage; do
 printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
 if [ "$id" = 2 ]; then printf '%s\n' 'ASIC Temp = 60 C; Frequency = 1450 MHz' >&2; fi
done
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let board:BoardConfig=toml::from_str("id='io'\nname='IO'\nserial='fixture'\nclient_port=18110\ndevice_location=1\nprofile='automatic'").unwrap();
        let mut config = Config::default();
        config.hardware.client_binary = Some(binary);
        config.hardware.boards = vec![board.clone()];
        config.hardware.automatic_state = Some(dir.join("cache.json"));
        let progress = Arc::new(timelord_supervisor::progress::Progress::default());
        let runtime = crate::runtime::RuntimeState {
            progress: Some(progress.clone()),
            ..Default::default()
        };
        let mut shared = Shared {
            assignments: config.assignments().unwrap(),
            config,
            clients: Default::default(),
            runtime: Arc::new(std::sync::Mutex::new(runtime)),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        };
        let mut child = launch(&shared, &board).await.unwrap();
        let mut owner = NativeOwner::new(&shared, &board, &mut child).await.unwrap();
        let bad = dir.join("not-a-cache");
        std::fs::create_dir(&bad).unwrap();
        shared.config.hardware.automatic_state = Some(bad);
        let now = crate::unix_now();
        owner.policy.epoch = now - 30;
        owner.policy.status.active_seconds = board.automatic.healthy_seconds;
        {
            let mut state = shared.runtime.lock().unwrap();
            state.phase = "running".into();
            state.node = "authenticated".into();
            state.boards.insert(
                board.id.clone(),
                HardwareState {
                    native_initialized: true,
                    state: "connected".into(),
                    temperature_c: Some(40.),
                    temperature_unix: now,
                    ..Default::default()
                },
            );
            for chain in timelord_core::Chain::ALL {
                for _ in 0..board.automatic.minimum_proofs_per_chain {
                    state.observations.push(crate::runtime::Observation {
                        id: 1,
                        chain,
                        timestamp_unix: now,
                        work_started_unix: now - 10,
                        ips: 1_000_000.,
                    });
                }
            }
        }
        let (ready, _rx) = oneshot::channel();
        let mut log = ClientLog::new(child.stderr.take().unwrap(), ready);
        let point = owner.policy.point;
        owner.tick(&shared, &board, &mut log).await.unwrap();
        assert_eq!(owner.policy.point, point);
        assert!(child.try_wait().unwrap().is_none());
        let held = progress.arithmetic(20).acquire_owned().await.unwrap();
        owner.change(point, 1).await.unwrap();
        let before = tokio::time::Instant::now();
        let result = owner.record_with_logs(&shared, &board, 0, &mut log).await;
        let elapsed = before.elapsed();
        drop(held);
        let quiesced = owner.stop(&mut child, log).await;
        let thermal = shared.runtime.lock().unwrap().boards[&board.id].latched_fault;
        std::fs::remove_dir_all(dir).unwrap();
        assert!(result.is_err());
        assert!(elapsed < Duration::from_secs(1));
        assert!(thermal);
        assert!(quiesced);
    }

    #[test]
    fn automatic_and_manual_arguments_share_the_optimized_proof_settings() {
        let mut board: BoardConfig =
            toml::from_str("id='a'\nname='A'\nclient_port=18110\ndevice_location=1").unwrap();
        let automatic = arguments(&board, 3).unwrap();
        assert!(automatic.contains(&"--live-control".into()));
        assert!(automatic.contains(&"--retain-asic-values".into()));
        assert!(automatic.windows(2).any(|w| w == ["--freq", "1450"]));
        board.profile = ProfileMode::Manual;
        board.frequency_mhz = 1540.;
        let manual = arguments(&board, 1).unwrap();
        assert!(manual.contains(&"--live-control".into()));
        assert!(manual.contains(&"--retain-asic-values".into()));
        assert!(manual.windows(2).any(|w| w == ["--freq", "1540"]));
        assert!(
            manual
                .windows(2)
                .any(|w| w == ["--checkpoint-threshold", "250000"])
        );
    }

    #[test]
    fn native_mask_matches_physical_assignments_and_profile_mode() {
        let mut config = Config::default();
        for boards in 1..=3 {
            config.hardware.boards = (0..boards)
                .map(|n| {
                    toml::from_str(&format!(
                        "id='b{n}'\nname='Board {n}'\nclient_port={}\ndevice_location={}\n",
                        18110 + n,
                        n + 1
                    ))
                    .unwrap()
                })
                .collect();
            let assignments = config.assignments().unwrap();
            for board in &config.hardware.boards {
                let assigned: Vec<_> = assignments
                    .iter()
                    .filter(|a| a.board_id == board.id)
                    .collect();
                let expected = assigned.iter().fold(0u8, |mask, a| mask | (1 << a.engine));
                for mode in [ProfileMode::Automatic, ProfileMode::Manual] {
                    let mut board = board.clone();
                    board.profile = mode;
                    let args = arguments(&board, assigned.len()).unwrap();
                    assert!(
                        args.windows(2)
                            .any(|w| w == ["--vdfs-mask", &expected.to_string()])
                    );
                }
            }
        }
        let board = &config.hardware.boards[0];
        assert!(arguments(board, 0).is_err());
        assert!(arguments(board, 4).is_err());
    }

    #[tokio::test]
    async fn temperature_guard_stops_only_the_owned_fixture() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!(
            "timelord-client-fixture-{}-{}",
            std::process::id(),
            crate::unix_now()
        ));
        std::fs::create_dir(&directory).unwrap();
        let binary = directory.join("hw_vdf_client");
        std::fs::write(&binary, "#!/bin/sh\ntrap 'exit 0' TERM INT\nread command version id frequency voltage\nprintf 'TLACK 1 %s %s %s\\n' \"$id\" \"$frequency\" \"$voltage\"\nprintf '%s\\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' 'VDF 0: Connected to timelord, waiting for challenge' 'ASIC Temp = 55.0 C; Frequency = 1500.0 MHz; freq_idx = 1' >&2\ncat >/dev/null\nprintf '%s\\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config: Config = toml::from_str(
            "[[hardware.boards]]\nid='a'\nname='A'\nclient_port=18110\ndevice_location=12305\nprofile='manual'\n",
        )
        .unwrap();
        config.hardware.client_binary = Some(binary);
        let shared = Arc::new(Shared {
            assignments: config.assignments().unwrap(),
            config,
            clients: Arc::new(std::sync::Mutex::new(Default::default())),
            runtime: Default::default(),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        });
        let (stop, receiver) = watch::channel(false);
        let (backend, _offers) = crate::asic::AsicBackend::new(
            &shared.config,
            &shared.assignments,
            shared.runtime.clone(),
        )
        .unwrap();
        let task = tokio::spawn(run(shared.clone(), Arc::new(backend), receiver));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let stopped = {
                    let state = shared.runtime.lock().unwrap();
                    state
                        .boards
                        .get("a")
                        .is_some_and(|b| b.state == "thermal_stop" && b.latched_fault)
                        && state.message.contains("needs attention")
                };
                if stopped {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !task.is_finished(),
            "A thermal stop must not end other board supervision"
        );
        assert!(
            shared
                .runtime
                .lock()
                .unwrap()
                .message
                .contains("needs attention")
        );
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn restart_delay_is_capped_and_never_exhausts_for_communication_faults() {
        let mut budget = RestartBackoff::default();
        assert_eq!(budget.next(false), Duration::from_secs(1));
        assert_eq!(budget.next(false), Duration::from_secs(5));
        assert_eq!(budget.next(false), Duration::from_secs(30));
        for _ in 0..100 {
            assert_eq!(budget.next(false), Duration::from_secs(30));
        }
        assert_eq!(budget.next(true), Duration::from_secs(1));
    }
    #[tokio::test]
    async fn hotplug_recovers_after_repeated_failures_without_restarting_healthy_board() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tl-board-isolation-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("client");
        std::fs::write(&binary,r#"#!/bin/sh
root=$(dirname "$0")
while [ "$#" -gt 0 ]; do
 if [ "$1" = --device-loc ]; then loc=$2; break; fi
 shift
done
echo $$ >> "$root/launches-$loc"
trap 'exit 0' TERM INT
if [ "$loc" = 2 ] && [ ! -f "$root/reconnected" ]; then echo "ASIC Temp = 0.0 C; Frequency = 1500.0 MHz" >&2; exit 1; fi
read command version id frequency voltage
printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
printf '%s\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' 'VDF 0: Connected to timelord, waiting for challenge' >&2
while :; do printf '%s\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' >&2; sleep .05; done &
logger=$!
cat >/dev/null
kill "$logger"
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config:Config=toml::from_str("[[hardware.boards]]\nid='healthy'\nname='Healthy'\nclient_port=18110\ndevice_location=1\nprofile='manual'\n[[hardware.boards]]\nid='flaky'\nname='Flaky'\nclient_port=18111\ndevice_location=2\nprofile='manual'").unwrap();
        config.hardware.client_binary = Some(binary);
        let shared = Arc::new(Shared {
            assignments: config.assignments().unwrap(),
            config,
            clients: Default::default(),
            runtime: Default::default(),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        });
        let (backend, _offers) = crate::asic::AsicBackend::new(
            &shared.config,
            &shared.assignments,
            shared.runtime.clone(),
        )
        .unwrap();
        let backend = Arc::new(backend);
        let (stop, rx) = watch::channel(false);
        let (tx, _) = oneshot::channel();
        let healthy = tokio::spawn(supervise_board(
            shared.clone(),
            backend.clone(),
            shared.config.hardware.boards[0].clone(),
            tx,
            rx.clone(),
            RestartBackoff::default(),
        ));
        let (tx, _) = oneshot::channel();
        let flaky = tokio::spawn(supervise_board(
            shared.clone(),
            backend.clone(),
            shared.config.hardware.boards[1].clone(),
            tx,
            rx.clone(),
            RestartBackoff {
                used: 0,
                delays: [Duration::from_millis(10); 3],
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if shared
                    .runtime
                    .lock()
                    .unwrap()
                    .boards
                    .get("flaky")
                    .is_some_and(|b| b.generation >= 6)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!flaky.is_finished());
        assert!(!healthy.is_finished());
        std::fs::write(dir.join("reconnected"), "").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if shared
                    .runtime
                    .lock()
                    .unwrap()
                    .boards
                    .get("flaky")
                    .is_some_and(|b| b.state == "connected")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        {
            let state = shared.runtime.lock().unwrap();
            assert!(!state.boards["flaky"].latched_fault);
            assert!(state.boards["flaky"].generation >= 6);
            assert_eq!(state.boards["healthy"].generation, 1);
            assert_eq!(state.boards["healthy"].state, "connected");
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("launches-1"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        stop.send(true).unwrap();
        for task in [flaky, healthy] {
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
        }
        std::fs::remove_file(dir.join("reconnected")).unwrap();
        let (stop, rx) = watch::channel(false);
        let (tx, _) = oneshot::channel();
        let retrying = tokio::spawn(supervise_board(
            shared.clone(),
            backend,
            shared.config.hardware.boards[1].clone(),
            tx,
            rx,
            RestartBackoff {
                used: 0,
                delays: [Duration::from_secs(30); 3],
            },
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if shared
                    .runtime
                    .lock()
                    .unwrap()
                    .boards
                    .get("flaky")
                    .is_some_and(|b| b.state == "reconnecting")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(3), retrying)
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn missing_sensor_data_is_not_a_thermal_rejection() {
        for reason in [
            "temperature_unavailable",
            "initial_readback attempts=3",
            "ASIC temperature telemetry stopped",
            "USB open failed",
        ] {
            assert!(!thermal_rejection(&anyhow::anyhow!(
                "Native control rejected request 2: {reason}"
            )));
        }
        for reason in ["temperature_limit", "temperature_guard"] {
            assert!(thermal_rejection(&anyhow::anyhow!(
                "Native control rejected request 2: {reason}"
            )));
        }
    }
    #[test]
    fn temperature_parser_rejects_nonfinite_measurements() {
        assert_eq!(
            temperature("2026 ASIC Temp = 39.90 C; Frequency = 1540.0 MHz; freq_idx = 1"),
            Some((39.9, 1540.0))
        );
        assert_eq!(
            temperature("ASIC Temp = NaN C; Frequency = 1540.0 MHz"),
            None
        );
    }
}

//! Supervise only explicitly configured native clients. Initialization is sequential.
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
    let mut arguments = vec![
        "--ip".into(),
        "127.0.0.1".into(),
        "--device-loc".into(),
        board
            .device_location
            .expect("validated location")
            .to_string(),
        board.client_port.to_string(),
        engines.to_string(),
    ];
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
    if board.profile == ProfileMode::Automatic {
        arguments.push("--live-control".into());
    }
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
// Drain stderr even while control is awaiting its acknowledgement. A recovery
// flood must not fill the pipe and block the USB owner before it reads TLSET.
struct ClientLog {
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStderr>>,
    ready: Option<oneshot::Sender<()>>,
    last_temperature: tokio::time::Instant,
    recent: std::collections::VecDeque<String>,
}
impl ClientLog {
    fn new(stderr: tokio::process::ChildStderr, ready: oneshot::Sender<()>) -> Self {
        Self {
            lines: BufReader::new(stderr).lines(),
            ready: Some(ready),
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
        if let Some((temp, freq)) = temperature(&line) {
            self.last_temperature = tokio::time::Instant::now();
            let hardware = state.boards.entry(board.id.clone()).or_default();
            hardware.temperature_unix = crate::unix_now();
            hardware.temperature_c = Some(temp);
            hardware.frequency_mhz = Some(freq);
            if hardware.state == "starting" || hardware.state == "connected" {
                hardware.state = "connected".into();
            }
            if temp >= board.temperature_limit_c {
                hardware.state = "thermal_stop".into();
                hardware.latched_fault = true;
                state.event(Some(board.id.clone()), "thermal_stop");
                bail!("ASIC reached its configured temperature limit");
            }
        }
        let recovery = line.contains("Restarting VDF at ");
        if recovery {
            state.boards.entry(board.id.clone()).or_default().recoveries += 1;
            state.event(Some(board.id.clone()), "recovery");
        }
        if line.contains("Connected to timelord, waiting for challenge")
            && let Some(ready) = self.ready.take()
        {
            let _ = ready.send(());
        }
        Ok(recovery)
    }
}
struct AutomaticOwner {
    policy: Policy,
    input: tokio::process::ChildStdin,
    output: BufReader<tokio::process::ChildStdout>,
    sequence: u64,
    serial: String,
    engines: usize,
    hash: String,
    host: String,
}
impl AutomaticOwner {
    async fn new(shared: &Shared, board: &BoardConfig, child: &mut Child) -> Result<Self> {
        let engines = shared
            .assignments
            .iter()
            .filter(|a| a.board_id == board.id)
            .count();
        let hash = automatic::fingerprint(&shared.config)?;
        let host = automatic::host();
        let point = automatic::initial(&shared.config, board, engines, &hash, &host)?;
        let mut owner = Self {
            policy: Policy::new(
                board.automatic.clone(),
                point,
                crate::unix_now(),
                supported_frequencies(&shared.config).await?,
            ),
            input: child.stdin.take().context("Native control input missing")?,
            output: BufReader::new(
                child
                    .stdout
                    .take()
                    .context("Native control output missing")?,
            ),
            sequence: 0,
            serial: board
                .serial
                .clone()
                .context("Automatic tuning needs a persistent board serial")?,
            engines,
            hash,
            host,
        };
        if let Some(entry) =
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
    async fn change(&mut self, target: Point, seconds: u64) -> Result<Point> {
        if !self
            .policy
            .settings
            .contains(target.frequency_mhz, target.voltage_v)
        {
            bail!("Automatic request exceeds configured limits");
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
    fn record(&self, shared: &Shared, clean: u64) -> Result<()> {
        let path = shared
            .config
            .hardware
            .automatic_state
            .as_ref()
            .context("Automatic state path missing")?;
        automatic::save(
            path,
            automatic::Learned {
                serial: self.serial.clone(),
                engines: self.engines,
                host: self.host.clone(),
                client_sha256: self.hash.clone(),
                point: self.policy.point,
                active_seconds: clean,
                updated_unix: crate::unix_now(),
                ceiling_mhz: self.policy.learned_limits().0,
                cooldown_seconds: self.policy.learned_limits().1,
            },
        )
    }
    async fn tick(
        &mut self,
        shared: &Shared,
        board: &BoardConfig,
        log: &mut ClientLog,
    ) -> Result<()> {
        let now = crate::unix_now();
        let input = {
            let state = shared.runtime.lock().expect("runtime");
            let hardware = state
                .boards
                .get(&board.id)
                .context("Automatic board state missing")?;
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
        match self.policy.observe(now, input) {
            Some(Action::Save) => self.record(
                shared,
                self.policy
                    .status
                    .active_seconds
                    .max(self.policy.settings.healthy_seconds),
            )?,
            Some(Action::Set(point, reason)) => {
                let persist = !self.policy.in_trial();
                tracing::info!(board=%board.name, frequency_mhz=point.frequency_mhz, voltage_v=point.voltage_v, %reason, "Applying automatic setting");
                let actual = self
                    .change_with_logs(point, 5, log, shared, board)
                    .await
                    .map_err(|error| anyhow::anyhow!("{}: {error}", board.name))?;
                self.policy.applied(actual, crate::unix_now(), reason);
                self.policy.synchronize_recoveries(
                    shared.runtime.lock().expect("runtime").boards[&board.id].recoveries,
                );
                // Save rollback limits/cooldown, never an outstanding experiment.
                // Zero clean seconds keeps the resumed point explicitly provisional.
                if persist {
                    self.record(shared, 0)?;
                }
                let mut state = shared.runtime.lock().expect("runtime");
                state.event(Some(board.id.clone()), "profile_change");
                let hardware = state.boards.entry(board.id.clone()).or_default();
                hardware.frequency_mhz = Some(actual.frequency_mhz);
                hardware.voltage_v = Some(actual.voltage_v);
            }
            None => {}
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
) -> Result<()> {
    let automatic = if board.profile == ProfileMode::Automatic {
        tokio::select! {
            result=AutomaticOwner::new(&shared,&board,&mut child)=>Some(result),
            _=shutdown.changed()=>{terminate(&mut child).await;return Ok(());}
        }
    } else {
        None
    };
    let mut automatic = match automatic {
        Some(Ok(owner)) => Some(owner),
        Some(Err(error)) => {
            terminate(&mut child).await;
            let mut state = shared.runtime.lock().expect("runtime");
            state.boards.entry(board.id.clone()).or_default().state = "fault".into();
            state
                .boards
                .entry(board.id.clone())
                .or_default()
                .fault_reason = Some(error.to_string());
            return Err(error);
        }
        None => None,
    };
    let output = child.stderr.take().context("ASIC client log unavailable")?;
    let mut log = ClientLog::new(output, ready);
    let mut watchdog = tokio::time::interval(Duration::from_secs(5));
    let result: Result<()> = loop {
        tokio::select! {
            _ = shutdown.changed() => break Ok(()),
            _ = watchdog.tick() => {
                if let Some(hardware) = shared.runtime.lock().expect("runtime").boards.get(&board.id)
                    && (hardware.latched_fault || hardware.state == "fault") {
                    break Err(anyhow::anyhow!(hardware.fault_reason.clone().unwrap_or_else(|| "ASIC needs attention".into())));
                }
                if log.last_temperature.elapsed() > Duration::from_secs(60) { break Err(anyhow::anyhow!("ASIC temperature telemetry stopped")); }
                match child.try_wait() {
                    Ok(Some(exit)) => break Err(anyhow::anyhow!("ASIC client exited: {exit}")),
                    Err(error) => break Err(error.into()),
                    Ok(None) => {}
                }
                if let Some(owner)=&mut automatic && let Err(error)=owner.tick(&shared,&board,&mut log).await {break Err(error);}
            }
            result = log.read(&shared, &board) => {
                let recovery = match result { Ok(recovery) => recovery, Err(error) => break Err(error) };
                if recovery && let Some(owner)=&mut automatic && let Err(error)=owner.tick(&shared,&board,&mut log).await {break Err(error);}
            }
        }
    };
    terminate(&mut child).await;
    let mut state = shared.runtime.lock().expect("runtime");
    let hardware = state.boards.entry(board.id.clone()).or_default();
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

pub async fn validate_automatic(config: &Config) -> Result<()> {
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
                config
                    .assignments()
                    .map_err(anyhow::Error::msg)?
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

const RESTART_DELAYS: [u64; 3] = [1, 5, 30];
struct RestartBudget {
    used: usize,
    delays: [Duration; 3],
}
impl Default for RestartBudget {
    fn default() -> Self {
        Self {
            used: 0,
            delays: RESTART_DELAYS.map(Duration::from_secs),
        }
    }
}
impl RestartBudget {
    fn next(&mut self, established: bool) -> Option<Duration> {
        if established {
            self.used = 0;
        }
        let delay = *self.delays.get(self.used)?;
        self.used += 1;
        Some(delay)
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
    let automatic = board.profile == ProfileMode::Automatic;
    if automatic {
        let point = automatic::initial(
            &shared.config,
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
    .stdin(if automatic {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(if automatic {
        Stdio::piped()
    } else {
        Stdio::inherit()
    })
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .context("Launching native ASIC client")
}
async fn supervise_board(
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    board: BoardConfig,
    ready: oneshot::Sender<()>,
    mut shutdown: watch::Receiver<bool>,
    mut budget: RestartBudget,
) {
    let mut ready = Some(ready);
    loop {
        if *shutdown.borrow() {
            return;
        }
        {
            let mut state = shared.runtime.lock().expect("runtime");
            let hardware = state.boards.entry(board.id.clone()).or_default();
            if hardware.latched_fault {
                return;
            }
            hardware.generation += 1;
            hardware.started_unix = crate::unix_now();
            hardware.state = "starting".into();
            hardware.raw_speeds.clear();
            hardware.temperature_unix = 0;
            hardware.temperature_c = None;
            hardware.fault_reason = None;
        }
        let started = tokio::time::Instant::now();
        let started_unix = crate::unix_now();
        let tx = ready.take().unwrap_or_else(|| oneshot::channel().0);
        let result = match launch(&shared, &board).await {
            Ok(child) => {
                supervise(shared.clone(), board.clone(), child, tx, shutdown.clone()).await
            }
            Err(error) => {
                drop(tx);
                Err(error)
            }
        };
        if *shutdown.borrow() || result.is_ok() {
            return;
        }
        let error = result.unwrap_err();
        let delay = budget.next(established(
            &shared,
            &board,
            started_unix,
            started.elapsed(),
        ));
        let stop = {
            let mut state = shared.runtime.lock().expect("runtime");
            let hardware = state.boards.entry(board.id.clone()).or_default();
            // Temperature/sensor stops and invalid proofs require explicit attention.
            hardware.latched_fault |=
                delay.is_none() || error.to_string().to_lowercase().contains("temperature");
            if hardware.state != "thermal_stop" {
                hardware.state = "fault".into();
            }
            hardware.raw_speeds.clear();
            hardware.fault_reason = Some(error.to_string());
            let stop = hardware.latched_fault;
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
        tracing::warn!(board=%board.name, %error, attempt=budget.used, retry_seconds=?delay.map(|d| d.as_secs()), stop, "ASIC client stopped; other boards remain supervised");
        if stop {
            return;
        }
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(delay.expect("restart delay")) => {}
        }
    }
}
pub async fn run(
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    validate_automatic(&shared.config).await?;
    let mut children = JoinSet::new();
    let (stop, child_shutdown) = watch::channel(false);
    for board in &shared.config.hardware.boards {
        if !shared.assignments.iter().any(|a| a.board_id == board.id) {
            continue;
        }
        shared.runtime.lock().expect("runtime").boards.insert(
            board.id.clone(),
            HardwareState {
                state: "starting".into(),
                ..Default::default()
            },
        );
        let (tx, rx) = oneshot::channel();
        children.spawn(supervise_board(
            shared.clone(),
            backend.clone(),
            board.clone(),
            tx,
            child_shutdown.clone(),
            RestartBudget::default(),
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
                if let Err(error) = result { tracing::error!(%error, "ASIC supervisor task failed"); }
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
        };
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut owner = AutomaticOwner::new(&shared, &board, &mut child)
            .await
            .unwrap();
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
        };
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut owner = AutomaticOwner::new(&shared, &board, &mut child)
            .await
            .unwrap();
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
        assert!(!manual.contains(&"--live-control".into()));
        assert!(manual.contains(&"--retain-asic-values".into()));
        assert!(manual.windows(2).any(|w| w == ["--freq", "1540"]));
        assert!(
            manual
                .windows(2)
                .any(|w| w == ["--checkpoint-threshold", "250000"])
        );
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
        std::fs::write(&binary, "#!/bin/sh\ntrap 'exit 0' TERM INT\nprintf '%s\\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' 'VDF 0: Connected to timelord, waiting for challenge' 'ASIC Temp = 55.0 C; Frequency = 1500.0 MHz; freq_idx = 1' >&2\nwhile :; do sleep 1; done\n").unwrap();
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
        });
        let (stop, receiver) = watch::channel(false);
        let (backend, _offers) =
            crate::asic::AsicBackend::new(&shared.config, shared.runtime.clone()).unwrap();
        let task = tokio::spawn(run(shared.clone(), Arc::new(backend), receiver));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if shared
                    .runtime
                    .lock()
                    .unwrap()
                    .boards
                    .get("a")
                    .is_some_and(|b| b.state == "thermal_stop" && b.latched_fault)
                {
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
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn restart_backoff_is_bounded_and_only_established_work_renews_the_budget() {
        let mut budget = RestartBudget::default();
        assert_eq!(budget.next(false), Some(Duration::from_secs(1)));
        assert_eq!(budget.next(false), Some(Duration::from_secs(5)));
        assert_eq!(budget.next(false), Some(Duration::from_secs(30)));
        assert_eq!(budget.next(false), None);
        assert_eq!(budget.next(true), Some(Duration::from_secs(1)));
    }
    #[tokio::test]
    async fn flapping_board_exhausts_retries_without_restarting_healthy_board() {
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
printf '%s\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' 'VDF 0: Connected to timelord, waiting for challenge' >&2
if [ "$loc" = 2 ]; then exit 1; fi
while :; do printf '%s\n' 'ASIC Temp = 40.0 C; Frequency = 1500.0 MHz; freq_idx = 1' >&2; sleep .05; done
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
        });
        let (backend, _offers) =
            crate::asic::AsicBackend::new(&shared.config, shared.runtime.clone()).unwrap();
        let backend = Arc::new(backend);
        let (stop, rx) = watch::channel(false);
        let (tx, _) = oneshot::channel();
        let healthy = tokio::spawn(supervise_board(
            shared.clone(),
            backend.clone(),
            shared.config.hardware.boards[0].clone(),
            tx,
            rx.clone(),
            RestartBudget::default(),
        ));
        let (tx, _) = oneshot::channel();
        let flaky = tokio::spawn(supervise_board(
            shared.clone(),
            backend.clone(),
            shared.config.hardware.boards[1].clone(),
            tx,
            rx.clone(),
            RestartBudget {
                used: 0,
                delays: [Duration::from_millis(10); 3],
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), flaky)
            .await
            .unwrap()
            .unwrap();
        assert!(!healthy.is_finished());
        {
            let state = shared.runtime.lock().unwrap();
            assert!(state.boards["flaky"].latched_fault);
            assert_eq!(state.boards["flaky"].generation, 4);
            assert_eq!(state.boards["healthy"].generation, 1);
            assert_eq!(state.boards["healthy"].state, "connected");
            assert_ne!(state.phase, "fault");
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("launches-1"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("launches-2"))
                .unwrap()
                .lines()
                .count(),
            4
        );
        // An explicit new recovery session can be cancelled during a long backoff.
        shared
            .runtime
            .lock()
            .unwrap()
            .boards
            .get_mut("flaky")
            .unwrap()
            .latched_fault = false;
        let (tx, _) = oneshot::channel();
        let retrying = tokio::spawn(supervise_board(
            shared.clone(),
            backend.clone(),
            shared.config.hardware.boards[1].clone(),
            tx,
            rx.clone(),
            RestartBudget {
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
                    .is_some_and(|b| b.generation == 5 && b.state == "fault")
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
        assert_eq!(
            std::fs::read_to_string(dir.join("launches-2"))
                .unwrap()
                .lines()
                .count(),
            5
        );
        tokio::time::timeout(Duration::from_secs(3), healthy)
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
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

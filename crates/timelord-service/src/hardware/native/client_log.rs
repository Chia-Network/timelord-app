use super::*;

pub(crate) struct ClientLog {
    pub(crate) lines: crate::bounded_lines::Lines<BufReader<tokio::process::ChildStderr>>,
    /// All engine sockets have connected to their listener.
    pub(crate) connected: bool,
    pub(crate) initialization: Option<tokio::sync::MutexGuard<'static, ()>>,
    pub(crate) preparing: Option<timelord_supervisor::progress::Guard>,
    pub(crate) last_temperature: tokio::time::Instant,
    /// Last raw-speed, temperature or heartbeat record from the event loop.
    pub(crate) last_liveness: tokio::time::Instant,
    /// The native client's own reason for stopping, when it reported one.
    pub(crate) stop_reason: Option<String>,
    /// The native client could not open its USB device: busy (for example
    /// still released by a previous owner) or gone. Not a board failure.
    pub(crate) open_failed: bool,
    pub(crate) recent: std::collections::VecDeque<String>,
    pub(super) shutdown_confirmed: bool,
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
pub(crate) fn record_thermal_stop(
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
    pub(crate) fn new(stderr: tokio::process::ChildStderr) -> Self {
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
    pub(crate) async fn drain(&mut self, board: &BoardConfig, limit: Duration) {
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
    pub(crate) async fn read(
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

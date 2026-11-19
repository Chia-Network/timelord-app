//! One board's persistent supervision: locate, launch, check, serve, retire
//! and retry. Retries never exhaust; reliability decides routing, not retries.
use super::*;
use timelord_supervisor::progress::{BoardSlot, Resource, Stage};

async fn acquire_initialization(
    shutdown: &mut watch::Receiver<bool>,
) -> Option<tokio::sync::MutexGuard<'static, ()>> {
    if *shutdown.borrow() {
        return None;
    }
    tokio::select! {
        biased;
        _ = shutdown.changed() => None,
        guard = INITIALIZATION.lock() => Some(guard),
    }
}

/// The board is not attached (or not uniquely identifiable). Absence is not a
/// reliability failure: the board did nothing wrong by being unplugged.
#[derive(Debug)]
pub(crate) struct BoardAbsent(pub String);
impl std::fmt::Display for BoardAbsent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is disconnected, busy, or has an ambiguous USB identity",
            self.0
        )
    }
}
impl std::error::Error for BoardAbsent {}
/// The operator disabled or held this board; retiring it is not a failure.
#[derive(Debug)]
pub(crate) struct OperatorHold;
impl std::fmt::Display for OperatorHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Board disabled by the operator")
    }
}
impl std::error::Error for OperatorHold {}
/// Supervision timing. Production uses the defaults; fixtures scale them down.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// Restart delays after the first, second and later consecutive failures.
    pub restart: [Duration; 3],
    /// How often an absent board is looked for.
    pub absent: Duration,
    /// Native silence that means its event loop is stuck.
    pub liveness: Duration,
    /// Interval between private canary proofs on an unrouted serving board.
    pub canary: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            restart: [1, 5, 30].map(Duration::from_secs),
            absent: Duration::from_secs(5),
            liveness: Duration::from_secs(crate::hardware::LIVENESS_TIMEOUT_SECONDS),
            canary: Duration::from_secs(native::CANARY_SECONDS),
        }
    }
}
impl Timing {
    fn restart_delay(&self, health: &crate::hardware::health::Health) -> Duration {
        let index = (health.consecutive_failures.max(1) - 1) as usize;
        self.restart[index.min(self.restart.len() - 1)]
    }
}

pub(super) fn checkpoint_retired(
    shared: &Shared,
    board: &BoardConfig,
    store: &crate::safety::Store,
    serial: &str,
) -> Result<()> {
    let (quiesced, fault) = {
        let state = shared.runtime.lock().expect("runtime");
        let status = state
            .boards
            .get(&board.id)
            .context("Retired board missing")?
            .lifecycle
            .status();
        (status.quiesced, status.fault)
    };
    match fault {
        Some(crate::board_lifecycle::Fault::Thermal) => {
            store.restrict(serial, crate::safety::Reason::Thermal)?
        }
        Some(crate::board_lifecycle::Fault::InvalidProof) => {
            store.restrict(serial, crate::safety::Reason::InvalidProof)?
        }
        _ => {}
    }
    if !quiesced {
        // Without confirmation the physical operating point is unknown: start
        // the next owner cautiously. This is not a safety restriction.
        store.mark_cautious(serial)?;
    }
    store.closed(serial)
}

/// Closed-owner discovery is the only observation allowed to declare this
/// serial absent.
async fn locate(shared: &Shared, board: &mut BoardConfig) -> Result<()> {
    let devices = crate::inventory::discover(&shared.config).await?;
    let present = crate::inventory::board_observation(board, &devices)
        == crate::inventory::Observation::Present;
    shared
        .runtime
        .lock()
        .expect("runtime")
        .boards
        .entry(board.id.clone())
        .or_default()
        .lifecycle
        .inventory(present);
    let location = crate::inventory::board_location(board, &devices)
        .map_err(|_| anyhow::Error::new(BoardAbsent(board.name.clone())))?;
    board.device_location = Some(location);
    shared
        .runtime
        .lock()
        .expect("runtime")
        .boards
        .entry(board.id.clone())
        .or_default()
        .device_location = Some(location);
    Ok(())
}

/// The unique task for this configured serial.
struct BoardSupervisor {
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    board: BoardConfig,
    shutdown: watch::Receiver<bool>,
    safety: Arc<crate::safety::Store>,
    previous_failure: crate::observability::RestartReason,
    timing: Timing,
    /// Health from before the last counted transport failure. If the board is
    /// then found unplugged within `FORGIVE_WINDOW`, that failure was the
    /// unplug (USB enumeration can outlive the I/O error) and is taken back.
    forgivable: Option<(crate::hardware::health::Health, tokio::time::Instant)>,
}
/// How long after a counted failure a board found missing proves the failure
/// was an unplug.
const FORGIVE_WINDOW: Duration = Duration::from_secs(120);
pub(super) async fn supervise_board(
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    board: BoardConfig,
    shutdown: watch::Receiver<bool>,
) {
    supervise_board_with(shared, backend, board, shutdown, Timing::default()).await
}
pub(crate) async fn supervise_board_with(
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    board: BoardConfig,
    shutdown: watch::Receiver<bool>,
    timing: Timing,
) {
    let Some(safety) = shared.runtime.lock().expect("runtime").safety.clone() else {
        tracing::error!(board=%board.name, "Board supervision requires the safety journal");
        return;
    };
    BoardSupervisor {
        shared,
        backend,
        board,
        shutdown,
        safety,
        previous_failure: crate::observability::RestartReason::Topology,
        timing,
        forgivable: None,
    }
    .run()
    .await;
}
impl BoardSupervisor {
    async fn run(mut self) {
        while !*self.shutdown.borrow() {
            if let Some(reason) = self.hold() {
                self.note_hold(reason);
                if !self.wait(Duration::from_secs(1)).await {
                    return;
                }
                continue;
            }
            let result = self.attempt().await;
            if *self.shutdown.borrow() {
                return;
            }
            let delay = match result {
                Ok(()) => Duration::from_secs(1),
                Err(error) => self.recover(error).await,
            };
            if !self.wait(delay).await {
                return;
            }
        }
    }
    /// Sleep up to `delay`, ending early on an operator retry. Returns false on
    /// shutdown.
    async fn wait(&mut self, delay: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + delay;
        loop {
            {
                let mut state = self.shared.runtime.lock().expect("runtime");
                if let Some(hardware) = state.boards.get_mut(&self.board.id)
                    && std::mem::take(&mut hardware.retry_requested)
                {
                    return true;
                }
            }
            let step = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .min(Duration::from_secs(1));
            if step.is_zero() {
                return true;
            }
            tokio::select! {
                _ = self.shutdown.changed() => return false,
                _ = tokio::time::sleep(step) => {}
            }
        }
    }
    /// Reasons this board must not run a native owner right now.
    fn hold(&self) -> Option<String> {
        if self.shared.config.hardware.client_binary.is_none() {
            return Some("Choose the bundled native ASIC client in Settings".into());
        }
        let policy = self.safety.policy(&self.board);
        let now = crate::unix_now();
        self.shared
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .entry(self.board.id.clone())
            .or_default()
            .lifecycle
            .restore_policy(policy.clone());
        if policy.health.disabled(now) {
            return Some(match policy.health.disabled_until_unix {
                Some(u64::MAX) | None => "Disabled by the operator".into(),
                Some(until) => format!(
                    "Kept out of rotation by the operator for {} more seconds",
                    until.saturating_sub(now)
                ),
            });
        }
        policy.hold_reason(now)
    }
    fn note_hold(&self, reason: String) {
        let policy = self.safety.policy(&self.board);
        let mut state = self.shared.runtime.lock().expect("runtime");
        let hardware = state.boards.entry(self.board.id.clone()).or_default();
        hardware.recovery_reason = Some(reason);
        hardware.retry_after_unix = policy
            .restriction
            .map(|_| policy.retry_after_unix)
            .or(policy.health.disabled_until_unix.filter(|&u| u != u64::MAX));
    }
    fn progress(&self) -> timelord_supervisor::progress::Guard {
        let index = self
            .shared
            .config
            .hardware
            .boards
            .iter()
            .position(|configured| configured.id == self.board.id)
            .unwrap_or(0);
        self.shared.process.progress.begin(
            Resource::BoardPreparation(
                BoardSlot::from_index(index).expect("at most three configured boards"),
            ),
            0,
            0,
            Stage::BoardPreparation,
        )
    }
    /// One native lifetime: lease, locate, journal, launch, supervise, retire.
    async fn attempt(&mut self) -> Result<()> {
        let serial = self
            .board
            .serial
            .clone()
            .context("ASIC boards need a persistent serial identity")?;
        // Another process owning this serial is "busy", not a board failure.
        let lease = self
            .safety
            .lease(&serial)
            .map_err(|_| anyhow::Error::new(BoardAbsent(self.board.name.clone())))?;
        let Some(initialization) = acquire_initialization(&mut self.shutdown).await else {
            return Ok(());
        };
        let preparing = self.progress();
        tokio::select! {
            result = locate(&self.shared, &mut self.board) => result?,
            _ = self.shutdown.changed() => return Ok(()),
        }
        let startup = startup(&self.shared, &self.board).await?;
        self.safety.begin_owner(&serial, startup.point)?;
        {
            let mut state = self.shared.runtime.lock().expect("runtime");
            state.disturb(
                crate::unix_now(),
                crate::hardware::continuous::DISTURBANCE_SECONDS,
            );
            let hardware = state.boards.entry(self.board.id.clone()).or_default();
            hardware.lifecycle.begin_attempt(crate::unix_now());
            hardware.raw_speeds.clear();
            hardware.temperature_unix = 0;
            hardware.temperature_c = None;
            hardware.recovery_reason = None;
            hardware.retry_after_unix = None;
            hardware.cautious = startup.cautious;
        }
        let child = match launch(&self.shared, &self.board, &startup).await {
            Ok(child) => child,
            Err(error) => {
                // No child exists, so the journal can close without confirmation.
                self.safety
                    .closed(&serial)
                    .context("Closing journal after native spawn failed")?;
                self.shared
                    .runtime
                    .lock()
                    .expect("runtime")
                    .boards
                    .entry(self.board.id.clone())
                    .or_default()
                    .lifecycle
                    .retired(true, None, crate::unix_now());
                return Err(error);
            }
        };
        if let Some(metrics) = self.shared.metrics() {
            metrics.client_started(
                &crate::observability::board(&self.shared.config, &self.board.id),
                self.previous_failure,
            );
        }
        let result = supervise_with(
            self.shared.clone(),
            self.backend.clone(),
            self.board.clone(),
            child,
            self.shutdown.clone(),
            initialization,
            Some(preparing),
            startup,
            self.timing,
        )
        .await;
        let checkpoint = checkpoint_retired(&self.shared, &self.board, &self.safety, &serial);
        if let (Err(error), Err(saved)) = (&result, &checkpoint) {
            tracing::error!(error=%format!("{error:#}"), saved=%format!("{saved:#}"), "Native retirement could not be checkpointed");
        }
        drop(lease);
        // The recovery path needs the typed native fault even if persistence failed.
        result.and(checkpoint)
    }
    /// After a running owner dies, a closed-owner inventory tells an unplug
    /// apart from a failure: an unplugged board did nothing wrong. Unknown
    /// (discovery failed or ambiguous) still counts, conservatively.
    async fn unplugged(&mut self) -> bool {
        if self.board.serial.is_none() {
            return false;
        }
        let Some(_initialization) = acquire_initialization(&mut self.shutdown).await else {
            return false;
        };
        let devices = match tokio::time::timeout(
            Duration::from_secs(15),
            crate::inventory::discover(&self.shared.config),
        )
        .await
        {
            Ok(Ok(devices)) => devices,
            Ok(Err(error)) => {
                tracing::info!(board=%self.board.name, error=%format!("{error:#}"), "USB check after the owner stopped failed; counting the stop");
                return false;
            }
            Err(_) => {
                tracing::info!(board=%self.board.name, "USB check after the owner stopped timed out; counting the stop");
                return false;
            }
        };
        let observation = crate::inventory::board_observation(&self.board, &devices);
        let missing = observation == crate::inventory::Observation::Missing;
        if !missing {
            tracing::info!(board=%self.board.name, ?observation, "ASIC still attached after its owner stopped");
        }
        if missing {
            self.shared
                .runtime
                .lock()
                .expect("runtime")
                .boards
                .entry(self.board.id.clone())
                .or_default()
                .lifecycle
                .inventory(false);
        }
        missing
    }
    async fn recover(&mut self, error: anyhow::Error) -> Duration {
        let mut absent = error.is::<BoardAbsent>() || error.is::<OperatorHold>();
        if !absent
            && classify(&error) == crate::board_lifecycle::Fault::Transport
            && self.unplugged().await
        {
            tracing::info!(board=%self.board.name, error=%format!("{error:#}"), "ASIC was unplugged; not counted as a failure");
            absent = true;
            // This stop was the unplug; an earlier failure stays counted.
            self.forgivable = None;
        }
        if absent && error.is::<BoardAbsent>() {
            self.forgive();
        }
        let shared = &self.shared;
        let board = &self.board;
        let reason = if absent && !error.is::<BoardAbsent>() && !error.is::<OperatorHold>() {
            format!("{} was unplugged", board.name)
        } else {
            format!("{error:#}")
        };
        self.previous_failure = crate::observability::restart_reason(&error);
        let fault = classify(&error);
        let thermal = fault == crate::board_lifecycle::Fault::Thermal;
        // Only failures of a board that was found count against its
        // reliability. Unplugged boards are simply looked for again.
        let before = board
            .serial
            .as_deref()
            .map(|serial| self.safety.health(serial));
        let delay = if absent {
            self.timing.absent
        } else {
            match board
                .serial
                .as_deref()
                .map(|serial| self.safety.record_failure(serial, &reason))
            {
                Some(Ok(health)) => {
                    self.forgivable = (fault == crate::board_lifecycle::Fault::Transport)
                        .then_some(before)
                        .flatten()
                        .map(|health| (health, tokio::time::Instant::now()));
                    if let Some(metrics) = shared.metrics() {
                        metrics
                            .board_failed(&crate::observability::board(&shared.config, &board.id));
                    }
                    crate::board_history::record(
                        shared,
                        board,
                        "failure",
                        Some(&reason),
                        Some(&health),
                    );
                    self.timing.restart_delay(&health)
                }
                Some(Err(saved)) => {
                    tracing::error!(error=%format!("{saved:#}"), "Board failure could not be recorded");
                    Duration::from_secs(30)
                }
                None => Duration::from_secs(30),
            }
        };
        {
            let mut state = shared.runtime.lock().expect("runtime");
            if thermal {
                native::record_thermal_stop(&mut state, shared, board, reason.clone());
            }
            let hardware = state.boards.entry(board.id.clone()).or_default();
            hardware
                .lifecycle
                .fault(fault, reason.clone(), crate::unix_now());
            hardware.raw_speeds.clear();
            hardware.temperature_c = None;
            hardware.temperature_unix = 0;
            hardware.fault(&error);
            hardware.recovery_reason = Some(reason.clone());
            hardware.retry_after_unix = Some(crate::unix_now().saturating_add(delay.as_secs()));
            if !absent
                && !state.events.iter().any(|e| {
                    e.board_id.as_ref() == Some(&board.id)
                        && e.kind == "board_fault"
                        && e.end_unix.is_none()
                })
            {
                state.event(Some(board.id.clone()), "board_fault");
            }
        }
        self.backend.stop_board(&board.id).await;
        if absent {
            tracing::debug!(board=%board.name, error=%reason, "ASIC not attached; looking again shortly");
        } else {
            tracing::warn!(board=%board.name, error=%reason, retry_seconds=delay.as_secs(), "ASIC client stopped; other boards remain supervised");
        }
        if thermal
            && let Some(serial) = &board.serial
            && let Err(saved) = self.safety.restrict(serial, crate::safety::Reason::Thermal)
        {
            tracing::error!(error=%format!("{saved:#}"), "Thermal fault could not be checkpointed");
        }
        delay
    }
    /// The board was found unplugged soon after a counted transport failure:
    /// that failure was the unplug, so restore the health from before it.
    fn forgive(&mut self) {
        let Some((previous, at)) = self.forgivable.take() else {
            return;
        };
        let Some(serial) = self.board.serial.as_deref() else {
            return;
        };
        if at.elapsed() > FORGIVE_WINDOW {
            return;
        }
        match self
            .safety
            .update_health(serial, true, |health| *health = previous)
        {
            Ok(health) => {
                tracing::info!(board=%self.board.name, consecutive_failures=health.consecutive_failures, "ASIC was unplugged; its last stop is no longer counted as a failure");
                crate::board_history::record(
                    &self.shared,
                    &self.board,
                    "failure_forgiven",
                    Some(&format!("{} was unplugged", self.board.name)),
                    Some(&health),
                );
            }
            Err(saved) => {
                tracing::error!(error=%format!("{saved:#}"), "Unplugged board's failure could not be forgiven")
            }
        }
    }
}

#[cfg(test)]
mod tests;

//! One board orchestration: discovery, admission, production lifetime and retry.
use super::*;

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

const RESTART_DELAYS: [u64; 3] = [1, 5, 30];
pub(super) struct RestartBackoff {
    pub(super) used: usize,
    pub(super) delays: [Duration; 3],
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
    pub(super) fn next(&mut self, established: bool) -> Duration {
        if established {
            self.used = 0;
        }
        let delay = self.delays[self.used.min(self.delays.len() - 1)];
        self.used = self.used.saturating_add(1);
        delay
    }
}
pub(super) fn established(
    shared: &Shared,
    board: &BoardConfig,
    started: u64,
    elapsed: Duration,
) -> bool {
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
pub(super) fn thermal_rejection(error: &anyhow::Error) -> bool {
    let reason = error.to_string();
    reason.contains(": temperature_limit")
        || reason.contains(": temperature_guard")
        || reason.contains("configured temperature limit")
}

pub(super) fn checkpoint_retired(
    shared: &Shared,
    board: &BoardConfig,
    store: &crate::safety::Store,
    serial: &str,
) -> Result<()> {
    let (quiesced, fault, failed) = {
        let state = shared.runtime.lock().expect("runtime");
        let status = state
            .boards
            .get(&board.id)
            .context("Retired board missing")?
            .lifecycle
            .status();
        (
            status.quiesced,
            status.fault,
            status.phase == crate::board_lifecycle::Phase::Fault,
        )
    };
    if let Some(fault) = fault {
        let reason = match fault {
            crate::board_lifecycle::Fault::Thermal => crate::safety::Reason::Thermal,
            crate::board_lifecycle::Fault::InvalidProof => crate::safety::Reason::InvalidProof,
            _ => crate::safety::Reason::UncleanOwner,
        };
        store.quarantine(serial, reason)?;
    } else if failed || !quiesced {
        // Confirmed retirement proves the hardware stopped; it cannot validate a
        // failed control exchange. Transport failures require fresh admission.
        store.quarantine(serial, crate::safety::Reason::UncleanOwner)?;
    }
    if quiesced {
        store.closed(serial)?;
    }
    Ok(())
}

// The journal must be armed before spawning an owner. Only a failed spawn can
// roll that intent back without a native shutdown confirmation: no child exists.
pub(super) async fn launch_prepared(shared: &Shared, board: &BoardConfig) -> Result<Child> {
    match launch(shared, board).await {
        Ok(child) => Ok(child),
        Err(error) => {
            let safety = shared.runtime.lock().expect("runtime").safety.clone();
            if let (Some(store), Some(serial)) = (safety, &board.serial) {
                store
                    .closed(serial)
                    .context("Closing journal after native spawn failed")?;
            }
            shared
                .runtime
                .lock()
                .expect("runtime")
                .boards
                .entry(board.id.clone())
                .or_default()
                .lifecycle
                .retired(true, None, crate::unix_now());
            Err(error)
        }
    }
}

/// Closed-owner discovery is the only observation allowed to declare this
/// serial absent. Both initial preparation and post-admission use this path.
async fn locate(shared: &Shared, board: &mut BoardConfig) -> Result<()> {
    if board.serial.is_some() {
        let devices = crate::inventory::discover(&shared.config).await?;
        crate::inventory::confirm_board_presence(
            shared
                .runtime
                .lock()
                .expect("runtime")
                .boards
                .get_mut(&board.id)
                .expect("board"),
            board,
            &devices,
        );
        board.device_location = Some(crate::inventory::board_location(board, &devices)?);
    }
    shared
        .runtime
        .lock()
        .expect("runtime")
        .boards
        .get_mut(&board.id)
        .expect("board")
        .device_location = board.device_location;
    Ok(())
}

pub(super) async fn supervise_board(
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    mut board: BoardConfig,
    ready: oneshot::Sender<()>,
    mut shutdown: watch::Receiver<bool>,
    mut budget: RestartBackoff,
) {
    let safety = shared.runtime.lock().expect("runtime").safety.clone();
    let mut ready = Some(ready);
    let mut previous_failure = crate::observability::RestartReason::Topology;
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
                    hardware.lifecycle.quarantined(record.quarantine.expect("quarantined").into(), if manual_invalid {
                        "Invalid proof: change the Manual operating point or select Automatic before retrying.".into()
                    } else {
                        format!(
                            "Waiting for fresh admission after {:?}",
                            record.quarantine.unwrap()
                        )
                    }, crate::unix_now());
                }
                tokio::select! {_=shutdown.changed()=>return,_=tokio::time::sleep(Duration::from_secs(5))=>{}}
                continue;
            }
        }
        {
            let mut state = shared.runtime.lock().expect("runtime");
            let hardware = state.boards.entry(board.id.clone()).or_default();
            if hardware.lifecycle.status().latched_fault && safety.is_none() {
                return;
            }
            hardware.lifecycle.begin_attempt(crate::unix_now());
            hardware.raw_speeds.clear();
            hardware.temperature_unix = 0;
            hardware.temperature_c = None;
        }
        let started = tokio::time::Instant::now();
        let started_unix = crate::unix_now();
        let tx = ready.take().unwrap_or_else(|| oneshot::channel().0);
        // D2XX enumeration and opening must not race another board's initialization.
        let Some(initialization) = acquire_initialization(&mut shutdown).await else {
            return;
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
                timelord_supervisor::progress::Resource::BoardPreparation(
                    timelord_supervisor::progress::BoardSlot::from_index(board_index)
                        .expect("at most three configured boards"),
                ),
                0,
                0,
                timelord_supervisor::progress::Stage::BoardPreparation,
            )
        });
        let preparation = tokio::select! {
            result = locate(&shared, &mut board) => result,
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
            // The serial lease pins identity; rediscovery only updates location.
            // Own that identity independently of the mutable discovery config.
            let serial = board.serial.clone();
            if let (Some(store), Some(serial)) = (&safety, serial.as_deref()) {
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
                    store.begin_owner(serial, q.point)?;
                    {
                        let mut state = shared.runtime.lock().expect("runtime");
                        state
                            .boards
                            .entry(board.id.clone())
                            .or_default()
                            .lifecycle
                            .checking();
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
                    let admission_epoch = store
                        .board(serial)
                        .context("Admitted board missing")?
                        .owner_epoch;
                    {
                        let mut state = shared.runtime.lock().expect("runtime");
                        let hardware = state.boards.entry(board.id.clone()).or_default();
                        hardware.lifecycle.admitted(admission_epoch);
                    }
                    if engines == 0 {
                        let _ = tx.send(());
                        // A new usable-capacity generation will assign this board.
                        let _ = shutdown.changed().await;
                        return Ok(());
                    }
                    let Some(guard) = acquire_initialization(&mut shutdown).await else {
                        return Ok(());
                    };
                    initialization = guard;
                    preparing = progress.as_ref().map(|p| {
                        p.begin(
                            timelord_supervisor::progress::Resource::BoardPreparation(
                                timelord_supervisor::progress::BoardSlot::from_index(board_index)
                                    .expect("at most three configured boards"),
                            ),
                            0,
                            0,
                            timelord_supervisor::progress::Stage::BoardPreparation,
                        )
                    });
                    tokio::select! {
                        result = locate(&shared, &mut board) => result?,
                        _ = shutdown.changed() => return Ok(()),
                    }
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
                store.begin_owner(serial, point)?;
                let mut state = shared.runtime.lock().expect("runtime");
                let hardware = state.boards.entry(board.id.clone()).or_default();
                hardware.lifecycle.start_native();
            }
            let child = launch_prepared(&shared, &board).await?;
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
                    && !state
                        .boards
                        .get(&board.id)
                        .is_some_and(|b| b.lifecycle.status().latched_fault)
                {
                    metrics.thermal_stopped(&label);
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
            hardware.lifecycle.fault(
                if thermal {
                    crate::board_lifecycle::Fault::Thermal
                } else {
                    crate::board_lifecycle::Fault::Transport
                },
                error.to_string(),
                crate::unix_now(),
            );
            hardware.lifecycle.retry();
            hardware.raw_speeds.clear();
            hardware.temperature_c = None;
            hardware.temperature_unix = 0;
            let stop = hardware.lifecycle.status().latched_fault;
            if !state.boards.iter().any(|(id, b)| {
                id != &board.id
                    && b.lifecycle.status().phase == crate::board_lifecycle::Phase::Connected
            }) {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn admission_reacquire_cancels_without_waiting_for_another_native_owner() {
        let _held = INITIALIZATION.lock().await;
        let (stop, mut shutdown) = watch::channel(false);
        let wait = acquire_initialization(&mut shutdown);
        tokio::pin!(wait);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut wait)
                .await
                .is_err()
        );
        stop.send(true).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), wait)
                .await
                .unwrap()
                .is_none()
        );
        let mut already_stopped = stop.subscribe();
        assert!(acquire_initialization(&mut already_stopped).await.is_none());
    }
}

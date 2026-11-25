//! One native supervision attempt: the initialize, establish, serve and retire phases.
use super::*;

/// How often an unrouted serving board proves itself with a private proof.
pub(in crate::hardware) const CANARY_SECONDS: u64 = 60;

/// The long-lived context of one native supervision attempt: shared runtime,
/// backend, board, shutdown signal and timing. It owns the initialize,
/// establish, serve_loop and retire phases used by production and fixtures.
pub(in crate::hardware) struct NativeSupervision {
    pub(in crate::hardware) shared: Arc<Shared>,
    pub(in crate::hardware) backend: Arc<crate::asic::AsicBackend>,
    pub(in crate::hardware) board: BoardConfig,
    pub(in crate::hardware) shutdown: watch::Receiver<bool>,
    pub(in crate::hardware) timing: super::board::Timing,
}
impl NativeSupervision {
    pub(in crate::hardware) async fn run(
        mut self,
        mut child: Child,
        initialization: tokio::sync::MutexGuard<'static, ()>,
        preparing: Option<timelord_supervisor::progress::Guard>,
        startup: Startup,
    ) -> Result<()> {
        let resume = startup.resume;
        let (mut owner, mut log, mut maintenance, initialized) = self
            .initialize(&mut child, initialization, preparing, startup)
            .await?;
        // `establish` carries a failed initialization as the returned result so
        // retirement still runs; only the setup phases above can abort early.
        let initialized = self
            .establish(&mut owner, &mut log, initialized, resume)
            .await;
        let result = self
            .serve_loop(
                &mut child,
                &mut owner,
                &mut log,
                &mut maintenance,
                initialized,
            )
            .await;
        self.retire(&mut child, owner, log, result).await
    }

    async fn initialize(
        &mut self,
        child: &mut Child,
        initialization: tokio::sync::MutexGuard<'static, ()>,
        preparing: Option<timelord_supervisor::progress::Guard>,
        startup: Startup,
    ) -> Result<(NativeOwner, ClientLog, Maintenance, Result<bool>)> {
        let setup_deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        {
            let mut state = self.shared.runtime.lock().expect("runtime");
            let hardware = state.boards.entry(self.board.id.clone()).or_default();
            hardware.native_pid = child.id();
            hardware.lifecycle.checking();
        }

        // Attach the pipes before polling initialization: cancellation must leave EOF
        // shutdown and its confirmation available to the same owner as normal exit.
        let maintenance = Maintenance {
            ramp: startup.ramp,
            cautious: startup.cautious,
            last_canary: tokio::time::Instant::now(),
            canary: self.timing.canary,
        };
        let mut owner = NativeOwner::attach(&self.board, child)?;
        let output = child.stderr.take().context("ASIC client log unavailable")?;
        let mut log = ClientLog::new(output);
        log.initialization = Some(initialization);
        log.preparing = preparing;
        if let Some(preparing) = &log.preparing {
            preparing.stage(timelord_supervisor::progress::Stage::BoardSetup);
        }
        let initialized = if *self.shutdown.borrow() {
            Ok(false)
        } else {
            tokio::select! {
                result = tokio::time::timeout_at(setup_deadline, owner.initialize(&self.shared, &self.board, &mut log, &startup)) => result.context("ASIC initialization timed out").and_then(|r| r).map(|()| true),
                _ = self.shutdown.changed() => Ok(false),
            }
        };
        if matches!(initialized, Ok(true)) {
            // The acknowledged setting confirms native hardware setup. Release USB
            // initialization before waiting for recovery evidence so other boards
            // can start meanwhile; network readiness keeps its own deadline.
            if self.board.profile == ProfileMode::Automatic {
                log.initialization.take();
            }
            if let Some(preparing) = &log.preparing {
                preparing.stage(timelord_supervisor::progress::Stage::BoardReadiness);
            }
        }
        Ok((owner, log, maintenance, initialized))
    }

    async fn establish(
        &mut self,
        owner: &mut NativeOwner,
        log: &mut ClientLog,
        initialized: Result<bool>,
        resume: Option<Point>,
    ) -> Result<bool> {
        let initialized = if matches!(initialized, Ok(true)) {
            super::recovery::establish(
                &self.shared,
                &self.board,
                owner,
                log,
                &mut self.shutdown,
                resume,
            )
            .await
        } else {
            initialized
        };
        // A device that could not be opened is busy or absent, not failing. The
        // control pipe can close before stderr is read, so read its last words.
        if initialized.is_err() && !log.open_failed {
            log.drain(&self.board, Duration::from_millis(500)).await;
        }
        let initialized = match initialized {
            Err(error) if log.open_failed => Err(anyhow::Error::new(super::board::BoardAbsent(
                self.board.name.clone(),
            ))
            .context(format!("{error:#}"))),
            other => other,
        };
        if let Err(error) = &initialized {
            let fault = classify(error);
            let mut state = self.shared.runtime.lock().expect("runtime");
            if fault == crate::board_lifecycle::Fault::Thermal {
                record_thermal_stop(&mut state, &self.shared, &self.board, format!("{error:#}"));
            } else {
                state
                    .boards
                    .entry(self.board.id.clone())
                    .or_default()
                    .lifecycle
                    .fault(fault, format!("{error:#}"), crate::unix_now());
            }
            state
                .boards
                .entry(self.board.id.clone())
                .or_default()
                .fault(error);
        }
        initialized
    }

    async fn serve_loop(
        &mut self,
        child: &mut Child,
        owner: &mut NativeOwner,
        log: &mut ClientLog,
        maintenance: &mut Maintenance,
        initialized: Result<bool>,
    ) -> Result<()> {
        // Successful setup starts a fresh readiness budget; time spent obtaining
        // the first ACK cannot starve engine/socket initialization of its window.
        let readiness_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut watchdog = tokio::time::interval(
            Duration::from_secs(5)
                .min(self.timing.liveness / 3)
                .max(Duration::from_millis(50)),
        );
        match initialized {
            Err(error) => Err(error),
            Ok(false) => Ok(()),
            Ok(true) => loop {
                if *self.shutdown.borrow() {
                    break Ok(());
                }
                tokio::select! {
                    // Deterministic precedence: shutdown, then the readiness
                    // deadline, then periodic checks, then native output.
                    biased;
                    _ = self.shutdown.changed() => break Ok(()),
                    // Temperatures do not certify engine/socket initialization.
                    // Hold the lease through confirmed retirement on this deadline.
                    _ = tokio::time::sleep_until(readiness_deadline), if !log.connected => {
                        break Err(anyhow::anyhow!("ASIC initialization timed out: the native client never connected its engines"));
                    }
                    _ = watchdog.tick() => {
                        if let Some(hardware) = self.shared.runtime.lock().expect("runtime").boards.get(&self.board.id)
                            && hardware.lifecycle.needs_retirement() {
                            let status = hardware.lifecycle.status();
                            let reason = status.fault_reason.unwrap_or("ASIC needs attention").to_owned();
                            break Err(match status.fault {
                                Some(crate::board_lifecycle::Fault::InvalidProof) => crate::asic::AsicFailure { board_id: self.board.id.clone(), verification: true, message: reason }.into(),
                                _ => anyhow::anyhow!(reason),
                            });
                        }
                        if log.last_liveness.elapsed() > self.timing.liveness {
                            break Err(NativeFailure::new(NativeFailureKind::Stalled, format!(
                                "Native client stopped reporting for {} seconds; its USB event loop appears stuck",
                                self.timing.liveness.as_secs()
                            )).into());
                        }
                        if log.last_temperature.elapsed() > Duration::from_secs(TEMPERATURE_TIMEOUT_SECONDS) {
                            break Err(NativeFailure::new(NativeFailureKind::SensorUnavailable, "ASIC temperature telemetry stopped").into());
                        }
                        match child.try_wait() {
                            Ok(Some(exit)) => break Err(exited(exit, log)),
                            Err(error) => break Err(error.into()),
                            Ok(None) => {}
                        }
                        if let Err(error) = maintenance.run(&self.shared, &self.backend, &self.board, owner, log, &mut self.shutdown).await { break Err(error); }
                        if let Err(error)=owner.tick(&self.shared,&self.board,log,&mut self.shutdown).await {break Err(error);}
                    }
                    result = log.read(&self.shared, &self.board) => {
                        let recovery = match result { Ok(recovery) => recovery, Err(error) => break Err(match child.try_wait() { Ok(Some(exit)) => exited(exit, log), _ => error }) };
                        if recovery.is_some() && let Err(error)=owner.tick(&self.shared,&self.board,log,&mut self.shutdown).await {break Err(error);}
                    }
                }
            },
        }
    }

    async fn retire(
        &mut self,
        child: &mut Child,
        owner: NativeOwner,
        log: ClientLog,
        result: Result<()>,
    ) -> Result<()> {
        self.shared.runtime.lock().expect("runtime").disturb(
            crate::unix_now(),
            crate::hardware::continuous::DISTURBANCE_SECONDS,
        );
        let quiesced = owner.stop(child, log).await;
        if !quiesced {
            tracing::warn!(board=%self.board.name,"Native shutdown lacked confirmed quiescence; next start is cautious");
        }
        record_retirement(&self.shared, &self.board, quiesced, result.as_ref().err());
        result
    }
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

#[cfg(test)]
mod tests;

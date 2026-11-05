//! Automatic policy and its acknowledged native control session.
use super::{NativeFailure, NativeFailureKind, native::ClientLog};
use crate::{
    Shared,
    automatic::{self, Action, Point, Policy},
};
use anyhow::{Context, Result, bail};
use std::time::Duration;
use timelord_core::BoardConfig;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Child,
};

// A failing checkpoint path is retried on this cadence, not every watchdog tick.
const RECORD_RETRY_SECONDS: u64 = 60;

pub(super) struct AutomaticOwner {
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
    last_record_attempt_unix: u64,
}
impl AutomaticOwner {
    pub(super) async fn new(
        shared: &Shared,
        board: &BoardConfig,
        child: &mut Child,
    ) -> Result<Self> {
        let engines = shared
            .assignments
            .iter()
            .filter(|a| a.board_id == board.id)
            .count();
        let facts = shared
            .native
            .as_ref()
            .context("Automatic capabilities were not prepared")?;
        let (hash, host) = (facts.hash.clone(), facts.host.clone());
        let learned = facts.learned(&shared.config, board, engines).await?;
        let point = automatic::initial_point(board, learned.as_ref());
        let mut owner = Self {
            policy: Policy::new(
                board.automatic.clone(),
                point,
                crate::unix_now(),
                facts.frequencies.clone(),
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
            raw_window: Default::default(),
            last_record_unix: crate::unix_now(),
            last_record_attempt_unix: 0,
        };
        if let Some(entry) = learned {
            owner.policy.restore(&entry);
        }
        let actual = owner.change(point, 30).await?;
        owner.policy.applied(
            actual,
            crate::unix_now(),
            "Observing live proofs at the initial setting".into(),
        );
        let recoveries = shared
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .get(&board.id)
            .map_or(0, |hardware| hardware.recoveries);
        owner.policy.synchronize_recoveries(recoveries);
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
                    return Err(NativeFailure::new(
                        NativeFailureKind::ControlClosed,
                        "Native control closed before acknowledgement",
                    )
                    .into());
                }
                if line.len() > 512 {
                    bail!("Native control response too large");
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
                if !self
                    .policy
                    .settings
                    .contains(actual.frequency_mhz, actual.voltage_v)
                    || (actual.frequency_mhz - target.frequency_mhz).abs() > 1.
                    || (actual.voltage_v - target.voltage_v).abs() > 0.0063
                {
                    return Err(NativeFailure::new(
                        NativeFailureKind::Readback,
                        "Native hardware readback does not match the requested setting",
                    )
                    .into());
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
    fn record_due(&self, now: u64) -> bool {
        now.saturating_sub(self.last_record_unix) >= 300
            && now.saturating_sub(self.last_record_attempt_unix) >= RECORD_RETRY_SECONDS
    }
    async fn record(
        &mut self,
        shared: &Shared,
        board: &BoardConfig,
        clean: u64,
        log: &mut ClientLog,
    ) -> Result<bool> {
        self.last_record_attempt_unix = crate::unix_now();
        let Some(path) = shared.config.hardware.automatic_state.clone() else {
            tracing::warn!(board=%board.id, "Automatic state path missing; proof production continues");
            return Ok(false);
        };
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
        let mut saving = tokio::task::spawn_blocking(move || automatic::save(&path, entry));
        loop {
            tokio::select! {
                result = &mut saving => match result {
                    Ok(Ok(())) => { self.last_record_unix = crate::unix_now(); return Ok(true); }
                    result => { tracing::warn!(?result, board=%board.id, "Optional Automatic checkpoint unavailable; proof production continues"); return Ok(false); }
                },
                result = log.read(shared, board) => { result?; }
            }
        }
    }
    pub(super) async fn tick(
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
                state.observations.iter(),
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
        let healthy = input.healthy && !input.paused;
        match self.policy.observe(now, input) {
            Some(Action::Save) => {
                self.record(
                    shared,
                    board,
                    self.policy
                        .status
                        .active_seconds
                        .max(self.policy.settings.healthy_seconds),
                    log,
                )
                .await?;
            }
            Some(Action::Set(point, reason)) => {
                let persist = !self.policy.in_trial();
                // Observe selected a target, but point is still the accepted
                // setting. A retest requires a durable retry reservation first.
                if self.policy.is_retest() && !self.record(shared, board, 0, log).await? {
                    self.policy.interrupt_trial(now);
                    return Ok(());
                }
                self.policy.commit_retest();
                tracing::info!(board=%board.name, frequency_mhz=point.frequency_mhz, voltage_v=point.voltage_v, kind=?reason.kind, %reason, "Applying automatic setting");
                let actual = self
                    .change_with_logs(point, 5, log, shared, board)
                    .await
                    .with_context(|| board.name.clone())?;
                let metrics = shared.process.metrics.clone();
                let kind = match reason.kind {
                    automatic::ChangeKind::Trial => {
                        crate::observability::OperatingPointReason::Trial
                    }
                    automatic::ChangeKind::Restore => {
                        crate::observability::OperatingPointReason::Restore
                    }
                    automatic::ChangeKind::Backoff => {
                        crate::observability::OperatingPointReason::Backoff
                    }
                };
                metrics.operating_point_changed(
                    &crate::observability::board(&shared.config, &board.id),
                    kind,
                );
                self.policy
                    .applied(actual, crate::unix_now(), reason.message);
                self.policy.synchronize_recoveries(
                    shared.runtime.lock().expect("runtime").boards[&board.id].recoveries,
                );
                // Save rollback limits/cooldown, never an outstanding experiment.
                // Zero clean seconds keeps the resumed point explicitly provisional.
                if persist {
                    self.record(shared, board, 0, log).await?;
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
                    && self.record_due(now)
                    && (self.raw_window.performance().is_some()
                        || self.policy.retest_remaining().is_some())
                {
                    self.record(shared, board, self.policy.status.active_seconds, log)
                        .await?;
                }
            }
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

#[cfg(test)]
mod tests;

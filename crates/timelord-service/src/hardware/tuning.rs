//! Automatic policy and its acknowledged native control session.
use super::{native::ClientLog, supported_frequencies};
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
            raw_window: Default::default(),
            last_record_unix: crate::unix_now(),
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
    fn record(&mut self, shared: &Shared, board: &BoardConfig, clean: u64) -> Result<()> {
        let path = shared
            .config
            .hardware
            .automatic_state
            .as_ref()
            .context("Automatic state path missing")?;
        self.last_record_unix = crate::unix_now();
        automatic::save(
            path,
            automatic::Learned {
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
            },
        )
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
        let healthy = input.healthy && !input.paused;
        match self.policy.observe(now, input) {
            Some(Action::Save) => self.record(
                shared,
                board,
                self.policy
                    .status
                    .active_seconds
                    .max(self.policy.settings.healthy_seconds),
            )?,
            Some(Action::Set(point, reason)) => {
                let persist = !self.policy.in_trial();
                if self.policy.is_retest() {
                    // Observe has selected a target but point is still the last
                    // accepted setting. Persist its reserved retry wait first.
                    self.record(shared, board, 0)?;
                }
                tracing::info!(board=%board.name, frequency_mhz=point.frequency_mhz, voltage_v=point.voltage_v, %reason, "Applying automatic setting");
                let actual = self
                    .change_with_logs(point, 5, log, shared, board)
                    .await
                    .map_err(|error| anyhow::anyhow!("{}: {error}", board.name))?;
                if let Some(metrics) = &shared.runtime.lock().expect("runtime").metrics {
                    use crate::observability::OperatingPointReason;
                    let kind = if !persist {
                        OperatingPointReason::Trial
                    } else if reason.to_ascii_lowercase().contains("restor") {
                        OperatingPointReason::Restore
                    } else {
                        OperatingPointReason::Backoff
                    };
                    metrics.operating_point_changed(
                        &crate::observability::board(&shared.config, &board.id),
                        kind,
                    );
                }
                self.policy.applied(actual, crate::unix_now(), reason);
                self.policy.synchronize_recoveries(
                    shared.runtime.lock().expect("runtime").boards[&board.id].recoveries,
                );
                // Save rollback limits/cooldown, never an outstanding experiment.
                // Zero clean seconds keeps the resumed point explicitly provisional.
                if persist {
                    self.record(shared, board, 0)?;
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
                {
                    self.record(shared, board, self.policy.status.active_seconds)?;
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

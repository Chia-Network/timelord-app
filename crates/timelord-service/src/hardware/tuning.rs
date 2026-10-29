//! Optional Automatic policy. Manual sessions have no policy or learned state.
use super::*;

// The permit lives in the actual blocking closure, across owner generations.
// A stalled optional file cannot accumulate more blocking jobs or own the
// critical safety writer. A later healthy observation retries a skipped save.
static OPTIONAL_CHECKPOINT: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

enum JournalDecision {
    Durable,
    Unavailable(anyhow::Error),
    Superseded,
    Cancelled,
}

pub(super) struct Journal {
    pub(super) store: Arc<crate::safety::Store>,
    pub(super) token: crate::safety::OwnerToken,
}
pub(super) struct Tuning {
    pub(super) experiment: Option<tokio::sync::OwnedSemaphorePermit>,
    pub(super) policy: Policy,
    pub(super) serial: String,
    pub(super) engines: usize,
    pub(super) hash: String,
    pub(super) host: String,
    pub(super) raw_window: crate::assignment::RawWindow,
    pub(super) last_record_unix: u64,
    pub(super) journal: Option<Journal>,
}
impl Tuning {
    pub(super) fn checkpoint(
        &mut self,
        shared: &Shared,
        board: &BoardConfig,
        clean: u64,
    ) -> Result<tokio::task::JoinHandle<Result<()>>> {
        let path = shared
            .config
            .hardware
            .automatic_state
            .as_ref()
            .context("Automatic state path missing")?;
        let progress = shared.runtime.lock().expect("runtime").progress.clone();
        let gate = progress
            .as_ref()
            .map(|p| p.arithmetic(timelord_supervisor::progress::Resource::OptionalCheckpoint))
            .unwrap_or_else(|| OPTIONAL_CHECKPOINT.clone());
        let permit = gate
            .try_acquire_owned()
            .context("Optional checkpoint writer is busy")?;
        self.last_record_unix = crate::unix_now();
        let path = path.clone();
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
        let health = if clean >= board.automatic.healthy_seconds && !self.policy.in_trial() {
            self.journal
                .as_ref()
                .map(|j| (j.store.clone(), j.token, self.serial.clone()))
        } else {
            None
        };
        let stage = shared
            .runtime
            .lock()
            .expect("runtime")
            .progress
            .as_ref()
            .map(|p| {
                p.begin(
                    timelord_supervisor::progress::Resource::OptionalCheckpoint,
                    0,
                    0,
                    timelord_supervisor::progress::Stage::OptionalPersistence,
                )
            });
        Ok(tokio::task::spawn_blocking(move || {
            let (_permit, _stage) = (permit, stage);
            automatic::save(&path, entry)?;
            if let Some((store, token, serial)) = health
                && let Err(error) = store.healthy_profile_saved(&serial, token)
            {
                tracing::warn!(%error, "Healthy safety evidence unavailable; retaining safe restriction");
            }
            Ok(())
        }))
    }
    // Optional persistence has a bounded wait, but a native fault is never
    // converted into an optional-file failure. Retests require actual success.
    async fn record(
        &mut self,
        shared: &Shared,
        board: &BoardConfig,
        clean: u64,
        log: &mut ClientLog,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<bool> {
        let mut write = match self.checkpoint(shared, board, clean) {
            Ok(write) => write,
            Err(error) => {
                tracing::warn!(%error, "Optional checkpoint unavailable; retaining current native point");
                return Ok(false);
            }
        };
        let deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                result = &mut write => match result {
                    Ok(Ok(())) => return Ok(true),
                    result => { tracing::warn!(?result, "Optional checkpoint failed; retaining native owner"); return Ok(false); }
                },
                _ = &mut deadline => { tracing::warn!("Optional checkpoint timed out; retaining native owner"); return Ok(false); },
                _ = shutdown.changed() => return Ok(false),
                result = log.read(shared, board) => { result?; }
            }
        }
    }
    async fn journal_point(
        &self,
        point: Point,
        log: &mut ClientLog,
        shared: &Shared,
        board: &BoardConfig,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<JournalDecision> {
        let Some(journal) = &self.journal else {
            return Ok(JournalDecision::Durable);
        };
        let (store, token, serial) = (journal.store.clone(), journal.token, self.serial.clone());
        let mut write = tokio::task::spawn_blocking(move || store.set_point(&serial, token, point));
        loop {
            tokio::select! {
                result = &mut write => return Ok(match result {
                    Ok(Ok(true)) => JournalDecision::Durable,
                    Ok(Ok(false)) => JournalDecision::Superseded,
                    Ok(Err(error)) => JournalDecision::Unavailable(error),
                    Err(error) => JournalDecision::Unavailable(error.into()),
                }),
                _ = shutdown.changed() => return Ok(JournalDecision::Cancelled),
                result = log.read(shared, board) => { result?; }
            }
        }
    }
    pub(super) async fn tick(
        &mut self,
        control: &mut NativeControl,
        shared: &Shared,
        board: &BoardConfig,
        log: &mut ClientLog,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<()> {
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
                            h.lifecycle.capacity(now, true).exploration_ready
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
                self.record(
                    shared,
                    board,
                    self.policy
                        .status
                        .active_seconds
                        .max(self.policy.settings.healthy_seconds),
                    log,
                    shutdown,
                )
                .await?;
            }
            Some(Action::Set(point, reason)) => {
                let persist = !self.policy.in_trial();
                if self.policy.is_retest() {
                    // Observe has selected a target but point is still the last
                    // accepted setting. Persist its reserved retry wait first.
                    if !self.record(shared, board, 0, log, shutdown).await? {
                        self.policy.interrupt_trial(now);
                        self.experiment.take();
                        return Ok(());
                    }
                }
                if *shutdown.borrow() {
                    return Ok(());
                }
                match self
                    .journal_point(point, log, shared, board, shutdown)
                    .await?
                {
                    JournalDecision::Durable => {}
                    JournalDecision::Cancelled => return Ok(()),
                    JournalDecision::Superseded => {
                        bail!("Native owner journal token was superseded")
                    }
                    JournalDecision::Unavailable(error) => {
                        tracing::warn!(%error,board=%board.name,"Operating-point journal unavailable");
                        if point.frequency_mhz > self.policy.point.frequency_mhz
                            || point.voltage_v > self.policy.point.voltage_v
                        {
                            self.policy.interrupt_trial(now);
                            self.experiment.take();
                            return Ok(());
                        }
                        // Restrictive backoff remains allowed; a crash retains the older faster point.
                    }
                }
                tracing::info!(board=%board.name, frequency_mhz=point.frequency_mhz, voltage_v=point.voltage_v, %reason, "Applying automatic setting");
                let actual = control
                    .change_with_logs(point, 5, log, shared, board)
                    .await
                    .map_err(|error| anyhow::anyhow!("{}: {error}", board.name))?;
                if let Some(metrics) = &shared.runtime.lock().expect("runtime").metrics {
                    let kind = if !persist {
                        crate::observability::OperatingPointReason::Trial
                    } else if reason.to_ascii_lowercase().contains("restor") {
                        crate::observability::OperatingPointReason::Restore
                    } else {
                        crate::observability::OperatingPointReason::Backoff
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
                    self.record(shared, board, 0, log, shutdown).await?;
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
                    self.record(
                        shared,
                        board,
                        self.policy.status.active_seconds,
                        log,
                        shutdown,
                    )
                    .await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn critical_journal_wait_still_drains_thermal_logs_and_retires_native_owner() {
        let dir = std::env::temp_dir().join(format!("tl-journal-thermal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("client");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
read command version id frequency voltage
printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
printf '%s\n' 'ASIC Temp = 60 C; Frequency = 1450 MHz' >&2
cat >/dev/null
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let board: BoardConfig = toml::from_str("id='a'\nname='A'\nserial='fixture'\nclient_port=18110\ndevice_location=1\nprofile='manual'").unwrap();
        let mut config = Config::default();
        config.hardware.boards = vec![board.clone()];
        config.hardware.client_binary = Some(binary);
        let store = Arc::new(crate::safety::Store::open(&dir.join("config.toml")).unwrap());
        let point = Point {
            frequency_mhz: 1450.,
            voltage_v: 1.,
        };
        let token = store.begin_owner("fixture", point).unwrap();
        let shared = Shared {
            assignments: config.assignments().unwrap(),
            config,
            runtime: Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeState {
                safety: Some(store.clone()),
                ..Default::default()
            })),
            clients: Default::default(),
            started_unix: 0,
            service_started_unix: 0,
        };
        let mut child = launch(&shared, &board).await.unwrap();
        let owner = NativeOwner::new(&shared, &board, &mut child).await.unwrap();
        let tuning = Tuning {
            experiment: None,
            policy: Policy::new(board.automatic.clone(), point, 0, vec![1450., 1460.]),
            serial: "fixture".into(),
            engines: 1,
            hash: "a".repeat(64),
            host: "fixture".into(),
            raw_window: Default::default(),
            last_record_unix: 0,
            journal: Some(Journal {
                store: store.clone(),
                token,
            }),
        };
        let (ready, _rx) = oneshot::channel();
        let mut log = ClientLog::new(child.stderr.take().unwrap(), ready);
        let (entered, waiting) = std::sync::mpsc::channel();
        let (release, done) = std::sync::mpsc::channel();
        let held = store.clone();
        let holder = std::thread::spawn(move || held.held_fixture(entered, done));
        waiting.recv().unwrap();
        let (_stop, mut shutdown) = watch::channel(false);
        let before = tokio::time::Instant::now();
        let result = tuning
            .journal_point(
                Point {
                    frequency_mhz: 1460.,
                    ..point
                },
                &mut log,
                &shared,
                &board,
                &mut shutdown,
            )
            .await;
        assert!(result.is_err());
        assert!(owner.stop(&mut child, log).await);
        assert!(before.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();
        holder.join().unwrap();
        let closed = store.clone();
        tokio::task::spawn_blocking(move || closed.closed("fixture"))
            .await
            .unwrap()
            .unwrap();
        assert!(!store.board("fixture").unwrap().owner_active);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

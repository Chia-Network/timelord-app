//! Optional Automatic policy. Manual sessions have no policy or learned state.
use super::*;

// The actual job retains the process-lifetime optional checkpoint permit after
// a caller timeout; safety-journal work uses its independent critical writer.

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
    /// A cautious start holds tuning and saves no profile until the owner has
    /// returned to its best-known point.
    pub(super) cautious: bool,
    /// The learned profile for the current engine count.
    pub(super) learned: Option<automatic::Learned>,
}
fn exploration_allowed(
    state: &crate::runtime::RuntimeState,
    board_id: &str,
    now: u64,
    owns_trial: bool,
) -> bool {
    owns_trial
        && !state.disturbed(now)
        && state.boards.get(board_id).is_some_and(|hardware| {
            hardware.lifecycle.accepts_work() && now.saturating_sub(hardware.temperature_unix) <= 30
        })
}
impl Tuning {
    pub(super) fn checkpoint(
        &mut self,
        shared: &Shared,
        board: &BoardConfig,
        clean: u64,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<()>>> {
        let path = shared
            .config
            .hardware
            .automatic_state
            .as_ref()
            .context("Automatic state path missing")?;
        let progress = shared.process.progress.clone();
        let permit = progress
            .arithmetic(timelord_supervisor::progress::Resource::OptionalCheckpoint)
            .try_acquire_owned()
            .context("Optional checkpoint writer is busy")?;
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
            // The best-known stable point survives provisional saves.
            stable_point: if clean >= board.automatic.healthy_seconds && !self.policy.in_trial() {
                Some(self.policy.point)
            } else {
                self.learned.as_ref().and_then(|l| l.stable_point)
            },
        };
        if entry.stable_point.is_some() {
            let mut learned = entry.clone();
            learned.active_seconds = clean;
            self.learned = Some(learned);
        }
        let stage = progress.begin(
            timelord_supervisor::progress::Resource::OptionalCheckpoint,
            0,
            0,
            timelord_supervisor::progress::Stage::OptionalPersistence,
        );
        crate::optional_io::spawn("automatic-checkpoint", move || {
            let (_permit, _stage) = (permit, stage);
            automatic::save(&path, entry)
        })
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
                    Ok(Ok(())) => { self.last_record_unix = crate::unix_now(); return Ok(true); },
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
    fn publish(&mut self, shared: &Shared, board: &BoardConfig, state: &str, message: &str) {
        self.policy.status.state = state.into();
        self.policy.status.message = message.into();
        shared
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .entry(board.id.clone())
            .or_default()
            .automatic = Some(self.policy.status.clone());
    }
    /// A route change gave this board a different number of active engines.
    /// Apply the setting learned for that count; without one, never keep a
    /// faster clock than the configured start for more active engines.
    async fn change_engine_count(
        &mut self,
        control: &mut NativeControl,
        shared: &Shared,
        board: &BoardConfig,
        log: &mut ClientLog,
        shutdown: &mut watch::Receiver<bool>,
        engines: usize,
    ) -> Result<()> {
        let now = crate::unix_now();
        let facts = shared
            .native
            .as_ref()
            .ok_or_else(|| anyhow::Error::new(NotPrepared))?
            .clone();
        let learned = facts.learned(&shared.config, board, engines).await?;
        let current = self.policy.point;
        let target = match &learned {
            Some(entry) => automatic::initial_point(board, Some(entry)),
            None => {
                let start = automatic::initial_point(board, None);
                Point {
                    frequency_mhz: if engines > self.engines {
                        current.frequency_mhz.min(start.frequency_mhz)
                    } else {
                        current.frequency_mhz
                    },
                    voltage_v: current.voltage_v,
                }
            }
        };
        tracing::info!(board=%board.name, from=self.engines, to=engines, frequency_mhz=target.frequency_mhz, learned=learned.is_some(), "Active engine count changed; applying the setting for the new count");
        let actual = if target != current {
            let lowering = target.frequency_mhz <= current.frequency_mhz
                && target.voltage_v <= current.voltage_v;
            match self
                .journal_point(target, log, shared, board, shutdown)
                .await?
            {
                JournalDecision::Durable => {}
                JournalDecision::Cancelled => return Ok(()),
                JournalDecision::Superseded => bail!("Native owner journal token was superseded"),
                JournalDecision::Unavailable(error) if !lowering => {
                    tracing::warn!(error=%format!("{error:#}"), "Operating-point journal unavailable; keeping the current setting");
                    return Ok(());
                }
                JournalDecision::Unavailable(_) => {}
            }
            control
                .change_with_logs(target, 5, log, shared, board)
                .await
                .context("Applying the setting for a new engine count")?
        } else {
            current
        };
        self.policy = Policy::new(
            board.automatic.clone(),
            actual,
            now,
            facts.frequencies.clone(),
        );
        if let Some(entry) = &learned {
            self.policy.restore(entry);
        }
        self.policy.synchronize_recoveries(
            shared.runtime.lock().expect("runtime").boards[&board.id].recoveries,
        );
        self.learned = learned;
        self.engines = engines;
        self.raw_window = Default::default();
        self.last_record_unix = now;
        let mut state = shared.runtime.lock().expect("runtime");
        let hardware = state.boards.entry(board.id.clone()).or_default();
        hardware.frequency_mhz = Some(actual.frequency_mhz);
        hardware.voltage_v = Some(actual.voltage_v);
        Ok(())
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
        let engines = shared.engines(&board.id);
        if engines == 0 {
            self.policy.interrupt_trial(now);
            self.experiment.take();
            self.publish(shared, board, "idle", "No chains routed to this board");
            return Ok(());
        }
        if self.cautious {
            self.experiment.take();
            self.publish(
                shared,
                board,
                "cautious",
                "Started cautiously; returning to the saved setting after verified work",
            );
            return Ok(());
        }
        if engines != self.engines {
            self.experiment.take();
            self.change_engine_count(control, shared, board, log, shutdown, engines)
                .await?;
        }
        let gate = shared.runtime.lock().expect("runtime").experiments.clone();
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
            let chains: Vec<_> = crate::hardware::continuous::routed_assignments(shared, &state)
                .iter()
                .filter(|a| a.board_id == board.id)
                .map(|a| a.chain)
                .collect();
            // Only this board's own proofs are evidence about its setting.
            let own =
                |o: &&crate::runtime::Observation| o.board_id.as_deref() == Some(board.id.as_str());
            let latest = state
                .observations
                .iter()
                .rev()
                .filter(own)
                .find(|o| chains.contains(&o.chain))
                .map(|o| o.timestamp_unix);
            let score = automatic::phase_score(
                state.observations.iter().filter(own),
                &chains,
                self.policy
                    .epoch
                    .max(now.saturating_sub(self.policy.settings.recovery_window_seconds)),
                self.policy.settings.minimum_proofs_per_chain,
            );
            automatic::Input {
                exploration_allowed: exploration_allowed(
                    &state,
                    &board.id,
                    now,
                    self.experiment.is_some(),
                ),
                healthy: state.phase == "running"
                    && ["ready", "authenticated"].contains(&state.node.as_str())
                    && latest.is_some_and(|t| now.saturating_sub(t) <= 30)
                    && now.saturating_sub(hardware.temperature_unix) <= 30,
                recoveries: hardware.recoveries,
                score,
                temperature: hardware.temperature_c,
                temperature_limit: board.temperature_limit_c,
                paused: state.tuning_paused,
                disturbed: state.disturbed(now),
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
                self.policy.commit_retest();
                tracing::info!(board=%board.name, frequency_mhz=point.frequency_mhz, voltage_v=point.voltage_v, %reason, "Applying automatic setting");
                let actual = control
                    .change_with_logs(point, 5, log, shared, board)
                    .await
                    .context("Applying automatic setting")?;
                if let Some(metrics) = shared.metrics() {
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
                }
                self.policy
                    .applied(actual, crate::unix_now(), reason.message);
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

    #[test]
    fn exploration_needs_a_serving_board_its_own_trial_and_no_disturbance() {
        use crate::board_lifecycle::{BoardLifecycle, Fault, Phase};
        let mut state = crate::runtime::RuntimeState::default();
        state.boards.insert(
            "healthy".into(),
            crate::runtime::HardwareState {
                lifecycle: BoardLifecycle::fixture(Phase::Connected, true, None, 1),
                temperature_unix: 100,
                ..Default::default()
            },
        );
        state.boards.insert(
            "restricted".into(),
            crate::runtime::HardwareState {
                lifecycle: BoardLifecycle::fixture(
                    Phase::InvalidProof,
                    false,
                    Some(Fault::InvalidProof),
                    1,
                ),
                ..Default::default()
            },
        );
        let gate = state.experiments.clone();
        let trial = gate.trial().unwrap();
        assert!(exploration_allowed(&state, "healthy", 100, true));
        assert!(!exploration_allowed(&state, "restricted", 100, true));
        assert!(!exploration_allowed(&state, "missing", 100, true));
        assert!(!exploration_allowed(&state, "healthy", 131, true));
        assert!(!exploration_allowed(
            &state,
            "healthy",
            100,
            gate.trial().is_some()
        ));
        // Another board starting or retiring holds exploration everywhere.
        state.disturb(100, 20);
        assert!(!exploration_allowed(&state, "healthy", 100, true));
        state.disturbed_until_unix = 0;
        state
            .boards
            .get_mut("healthy")
            .unwrap()
            .lifecycle
            .testing()
            .fault(Fault::Transport, "own failure".into(), 100);
        assert!(!exploration_allowed(&state, "healthy", 100, true));
        drop(trial);
    }
    #[tokio::test]
    async fn critical_journal_wait_still_drains_thermal_logs_and_retires_native_owner() {
        let dir = std::env::temp_dir().join(format!("tl-journal-thermal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("client");
        crate::test_scripts::write_executable_script(
            &binary,
            r#"#!/bin/sh
read command version id frequency voltage
printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
sleep 0.2
printf '%s\n' 'ASIC Temp = 60 C; Frequency = 1450 MHz' >&2
cat >/dev/null
printf '%s\n' 'Hardware shutdown: engines_paused=1 safe_point_restored=1' >&2
"#,
        );
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
        let shared = crate::Shared::new(
            if config
                .hardware
                .boards
                .iter()
                .any(|b| b.profile == ProfileMode::Automatic)
            {
                Some(Facts::load(&config).await.unwrap())
            } else {
                None
            },
            config,
            Arc::new(std::sync::Mutex::new({
                let mut state = crate::runtime::RuntimeState::default();
                state.safety = Some(store.clone());
                state
            })),
            crate::operational::State::fixture(),
            0,
            0,
        )
        .routed_by_config();
        let startup = startup(&shared, &board).await.unwrap();
        let mut child = launch(&shared, &board, &startup).await.unwrap();
        let mut owner = NativeOwner::attach(&board, &mut child).unwrap();
        let mut log = ClientLog::new(child.stderr.take().unwrap());
        owner
            .initialize(&shared, &board, &mut log, &startup)
            .await
            .unwrap();
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
            cautious: false,
            learned: None,
        };
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

#[cfg(test)]
#[path = "tuning/tests.rs"]
mod control_tests;

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
        store.quarantine(serial, fault.into())?;
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
pub(super) async fn launch_prepared(
    shared: &Shared,
    board: &BoardConfig,
    startup: &native::Startup,
) -> Result<Child> {
    match launch(shared, board, startup).await {
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
        let observed = crate::inventory::board_observation(board, &devices);
        shared
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .get_mut(&board.id)
            .expect("board")
            .lifecycle
            .inventory(None, observed);
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

mod attempt;
use attempt::PreparedAttempt;

/// The unique task for this configured serial. Each prepared attempt owns the
/// serial lease and initialization guards until its native owner retires.
struct BoardSupervisor {
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    board: BoardConfig,
    ready: Option<oneshot::Sender<()>>,
    shutdown: watch::Receiver<bool>,
    budget: RestartBackoff,
    safety: Option<Arc<crate::safety::Store>>,
    previous_failure: crate::observability::RestartReason,
}
pub(super) async fn supervise_board(
    shared: Arc<Shared>,
    backend: Arc<crate::asic::AsicBackend>,
    board: BoardConfig,
    ready: oneshot::Sender<()>,
    shutdown: watch::Receiver<bool>,
    budget: RestartBackoff,
) {
    let safety = shared.runtime.lock().expect("runtime").safety.clone();
    BoardSupervisor {
        shared,
        backend,
        board,
        ready: Some(ready),
        shutdown,
        budget,
        safety,
        previous_failure: crate::observability::RestartReason::Topology,
    }
    .run()
    .await;
}
impl BoardSupervisor {
    async fn run(mut self) {
        while !*self.shutdown.borrow() {
            if self.hold_for_policy() {
                self.ready.take();
                tokio::select! { _=self.shutdown.changed()=>return, _=tokio::time::sleep(Duration::from_secs(5))=>{} }
                continue;
            }
            if self.safety.is_none()
                && self
                    .shared
                    .runtime
                    .lock()
                    .expect("runtime")
                    .boards
                    .get(&self.board.id)
                    .is_some_and(|board| board.lifecycle.status().latched_fault)
            {
                return;
            }
            let started = tokio::time::Instant::now();
            let started_unix = crate::unix_now();
            let result = self.attempt().await;
            if *self.shutdown.borrow() || result.is_ok() {
                return;
            }
            let Some(delay) = self
                .recover(result.unwrap_err(), started, started_unix)
                .await
            else {
                return;
            };
            tokio::select! { _=self.shutdown.changed()=>return, _=tokio::time::sleep(delay)=>{} }
        }
    }
    fn hold_for_policy(&self) -> bool {
        let policy = self
            .safety
            .as_ref()
            .map(|store| store.policy(&self.board))
            .unwrap_or_default();
        self.shared
            .runtime
            .lock()
            .expect("runtime")
            .boards
            .entry(self.board.id.clone())
            .or_default()
            .lifecycle
            .restore_policy(policy, crate::unix_now());
        let reason = policy.hold_reason(crate::unix_now());
        if let Some(reason) = &reason {
            let mut state = self.shared.runtime.lock().expect("runtime");
            let hardware = state.boards.entry(self.board.id.clone()).or_default();
            hardware.recovery_reason = Some(reason.clone());
            hardware.retry_after_unix = Some(policy.retry_after_unix);
        }
        reason.is_some()
    }
    async fn attempt(&mut self) -> Result<()> {
        PreparedAttempt::start(self).await
    }
    async fn recover(
        &mut self,
        error: anyhow::Error,
        started: tokio::time::Instant,
        started_unix: u64,
    ) -> Option<Duration> {
        let shared = &self.shared;
        let board = &self.board;
        let safety = &self.safety;
        let backend = &self.backend;
        self.previous_failure = crate::observability::restart_reason(&error);
        let fault = classify(&error);
        let thermal = fault == crate::board_lifecycle::Fault::Thermal;
        let stable = established(&self.shared, &self.board, started_unix, started.elapsed());
        let delay = self.budget.next(stable);
        let stop = {
            let mut state = shared.runtime.lock().expect("runtime");
            if thermal {
                native::record_thermal_stop(&mut state, shared, board, error.to_string());
            }
            let hardware = state.boards.entry(board.id.clone()).or_default();
            // Missing USB/sensor data is recoverable. Explicit overtemperature and
            // invalid verified proofs remain latched. Old ambiguous guards fail closed.
            hardware
                .lifecycle
                .fault(fault, error.to_string(), crate::unix_now());
            hardware.lifecycle.retry();
            hardware.raw_speeds.clear();
            hardware.temperature_c = None;
            hardware.temperature_unix = 0;
            hardware.fault(&error);
            hardware.recovery_reason = Some(error.to_string());
            hardware.retry_after_unix = Some(crate::unix_now().saturating_add(delay.as_secs()));
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
        tracing::warn!(board=%board.name, %error, attempt=self.budget.used, retry_seconds=delay.as_secs(), stop, "ASIC client stopped; other boards remain supervised");
        if thermal
            && let (Some(store), Some(serial)) = (&safety, &board.serial)
            && let Err(saved) = store.quarantine(serial, crate::safety::Reason::Thermal)
        {
            tracing::error!(%saved,"Thermal fault could not be checkpointed");
        }
        if stop && safety.is_none() {
            return None;
        }
        Some(delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_sensor_data_is_not_a_thermal_rejection() {
        for reason in [
            "temperature_unavailable",
            "initial_readback attempts=3",
            "ASIC temperature telemetry stopped",
            "USB open failed",
        ] {
            assert_eq!(
                classify(&anyhow::anyhow!(
                    "Native control rejected request 2: {reason}"
                )),
                crate::board_lifecycle::Fault::Transport
            );
        }
        for reason in ["temperature_limit", "temperature_guard"] {
            let error = anyhow::Error::new(crate::hardware::NativeFailure::rejection(2, reason))
                .context("Board A: failed");
            assert_eq!(classify(&error), crate::board_lifecycle::Fault::Thermal);
            assert_eq!(
                classify(&anyhow::anyhow!("unrelated text mentioning {reason}")),
                crate::board_lifecycle::Fault::Transport
            );
        }
    }

    #[tokio::test]
    async fn live_automatic_temperature_guard_records_one_thermal_stop() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tl-live-thermal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("client");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
if [ "$1" = --control-frequencies ]; then echo '[1430,1450,1460]'; exit 0; fi
while read command version id frequency voltage; do
  if [ "$id" = 2 ]; then printf 'TLERR 1 %s temperature_guard temperature=58\n' "$id"; continue; fi
  printf 'TLACK 1 %s %s %s\n' "$id" "$frequency" "$voltage"
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let board: BoardConfig = toml::from_str("id='thermal-live'\nname='Thermal live'\nserial='fixture'\nclient_port=18110\ndevice_location=1\nprofile='automatic'").unwrap();
        let mut config = Config::default();
        config.hardware.boards = vec![board.clone()];
        config.hardware.client_binary = Some(binary);
        let shared = Arc::new(Shared {
            native: Some(Facts::load(&config).await.unwrap()),
            assignments: config.assignments().unwrap(),
            config,
            clients: Default::default(),
            process: crate::operational::State::fixture(),
            scope: crate::runtime::RuntimeScope::Production,
            runtime: Default::default(),
            started_unix: crate::unix_now(),
            service_started_unix: crate::unix_now(),
        });
        let mut child = Command::new(
            shared
                .config
                .hardware
                .client_binary
                .as_ref()
                .unwrap()
                .clone(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
        let mut owner = NativeOwner::attach(&board, &mut child).unwrap();
        let startup = startup(&shared, &board).await.unwrap();
        let (ready, _ready) = oneshot::channel();
        let mut log = ClientLog::new(child.stderr.take().unwrap(), ready);
        owner
            .initialize(&shared, &board, &mut log, startup)
            .await
            .unwrap();
        let (_stop, mut shutdown) = watch::channel(false);
        shared
            .runtime
            .lock()
            .unwrap()
            .boards
            .entry(board.id.clone())
            .or_default()
            .recoveries = 80;
        owner.tuning.as_mut().unwrap().policy.applied(
            Point {
                frequency_mhz: 1470.,
                voltage_v: 1.,
            },
            crate::unix_now().saturating_sub(10),
            "settled fixture point".into(),
        );
        let error = owner
            .tick(&shared, &board, &mut log, &mut shutdown)
            .await
            .unwrap_err();
        assert_eq!(classify(&error), crate::board_lifecycle::Fault::Thermal);
        assert_eq!(
            shared
                .runtime
                .lock()
                .unwrap()
                .events
                .iter()
                .filter(|event| {
                    event.board_id.as_deref() == Some(board.id.as_str())
                        && event.kind == "thermal_stop"
                })
                .count(),
            0,
            "the log reader saw no over-limit telemetry"
        );
        let (backend, _offers) = crate::asic::AsicBackend::new(
            &shared.config,
            &shared.assignments,
            shared.runtime.clone(),
            shared.process.clone(),
        )
        .unwrap();
        let mut supervisor = BoardSupervisor {
            shared: shared.clone(),
            backend: Arc::new(backend),
            board: board.clone(),
            ready: None,
            shutdown: watch::channel(false).1,
            budget: RestartBackoff::default(),
            safety: None,
            previous_failure: crate::observability::RestartReason::Topology,
        };
        supervisor
            .recover(error, tokio::time::Instant::now(), crate::unix_now())
            .await;
        {
            let state = shared.runtime.lock().unwrap();
            let status = state.boards[&board.id].lifecycle.status();
            assert_eq!(status.fault, Some(crate::board_lifecycle::Fault::Thermal));
            assert_eq!(status.phase, crate::board_lifecycle::Phase::ThermalStop);
            assert_eq!(
                state
                    .events
                    .iter()
                    .filter(|event| {
                        event.board_id.as_deref() == Some(board.id.as_str())
                            && event.kind == "thermal_stop"
                    })
                    .count(),
                1
            );
        }
        crate::hardware::native::terminate(&mut child).await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn transferred_qualified_unassigned_board_keeps_ticket_without_opening_or_retesting() {
        let dir = std::env::temp_dir().join(format!("tl-admitted-transfer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let serial = format!("wait-{:x}", std::process::id());
        let board: BoardConfig = toml::from_str(&format!("id='waiting'\nname='Waiting'\nserial='{serial}'\nclient_port=18110\ndevice_location=1\nprofile='manual'\nfrequency_mhz=1450.0\nvoltage_v=1.0")).unwrap();
        let mut config = Config::default();
        config.hardware.boards = vec![board.clone()];
        // Readable fingerprint only: neither native launch nor D2XX discovery is
        // possible in this fixture. Reuse must stay entirely on the closed path.
        let client = dir.join("must-not-launch");
        std::fs::write(&client, "fixture client identity").unwrap();
        config.hardware.client_binary = Some(client);
        let store = Arc::new(crate::safety::Store::open(&dir.join("config.toml")).unwrap());
        let mut runtime = {
            let mut state = crate::runtime::RuntimeState::new(false);
            state.safety = Some(store.clone());
            state
        };
        let hardware = runtime.boards.entry(board.id.clone()).or_default();
        hardware.lifecycle.bind(Some(&serial));
        hardware.lifecycle.admitted(0);
        let seed = Shared {
            native: if config
                .hardware
                .boards
                .iter()
                .any(|b| b.profile == ProfileMode::Automatic)
            {
                Some(Facts::load(&config).await.unwrap())
            } else {
                None
            },
            config: config.clone(),
            assignments: vec![],
            process: crate::operational::State::fixture(),
            scope: crate::runtime::RuntimeScope::Production,
            runtime: Arc::new(std::sync::Mutex::new(runtime)),
            clients: Default::default(),
            started_unix: 1,
            service_started_unix: 1,
        };
        let q = admission::qualification(&seed, &board, 3).unwrap();
        store.qualify(&serial, q.clone()).unwrap();
        let mut transferred = seed.runtime.lock().unwrap().boards.clone();
        for _ in 0..2 {
            let mut runtime = {
                let mut state = crate::runtime::RuntimeState::new(false);
                state.safety = Some(store.clone());
                state
            };
            runtime.boards = transferred;
            restore_lifecycles(&config, &mut runtime.boards, Some(&store));
            let runtime = Arc::new(std::sync::Mutex::new(runtime));
            let shared = Arc::new(Shared {
                native: if config
                    .hardware
                    .boards
                    .iter()
                    .any(|b| b.profile == ProfileMode::Automatic)
                {
                    Some(Facts::load(&config).await.unwrap())
                } else {
                    None
                },
                config: config.clone(),
                assignments: vec![],
                process: crate::operational::State::fixture(),
                scope: crate::runtime::RuntimeScope::Production,
                runtime: runtime.clone(),
                clients: Default::default(),
                started_unix: 1,
                service_started_unix: 1,
            });
            let (backend, _) = crate::asic::AsicBackend::new(
                &config,
                &[],
                runtime.clone(),
                crate::operational::State::fixture(),
            )
            .unwrap();
            let (stop, rx) = watch::channel(false);
            let task = tokio::spawn(run(shared.clone(), Arc::new(backend), rx));
            tokio::time::timeout(Duration::from_secs(3), async {
                while store.lease(&serial).is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            {
                let mut state = runtime.lock().unwrap();
                let lifecycle = &mut state.boards.get_mut(&board.id).unwrap().lifecycle;
                assert_eq!(lifecycle.admitted_inventory_epoch(), Some(0));
                assert_eq!(lifecycle.status().generation, 0);
                lifecycle.observe_inventory(0, crate::board_lifecycle::Inventory::Missing);
                assert!(!lifecycle.capacity(1, true).usable);
                lifecycle.observe_inventory(0, crate::board_lifecycle::Inventory::Present);
                assert!(lifecycle.capacity(1, true).usable);
            }
            let record = store.board(&serial).unwrap();
            assert_eq!(record.owner_epoch, 0);
            assert!(!record.owner_active);
            assert_eq!(record.qualification, Some(q.clone()));
            assert!(record.quarantine.is_none());
            stop.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(store.lease(&serial).is_ok());
            transferred = runtime.lock().unwrap().boards.clone();
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
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

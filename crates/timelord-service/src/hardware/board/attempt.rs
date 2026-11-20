//! Owned preparation/admission/native stages. Closed qualified boards never
//! manufacture a native lifetime merely because their current assignment is zero.
use super::*;
use timelord_supervisor::{
    InstanceLock,
    progress::{BoardSlot, Guard, Resource, Stage},
};

type Initialization = tokio::sync::MutexGuard<'static, ()>;
pub(super) struct PreparedAttempt {
    state: BoardAttempt,
    initialization: Initialization,
    preparing: Guard,
}
struct BoardAttempt {
    shared: Arc<Shared>,
    board: BoardConfig,
    serial_lease: Option<InstanceLock>,
    ready: oneshot::Sender<()>,
    engines: usize,
}
#[derive(Debug, PartialEq, Eq)]
enum AdmissionDecision {
    Check,
    Reuse,
}
impl AdmissionDecision {
    fn for_qualification(
        store: &crate::safety::Store,
        serial: &str,
        qualification: &crate::safety::Qualification,
    ) -> Self {
        if store.needs_self_test(serial, qualification) {
            Self::Check
        } else {
            Self::Reuse
        }
    }
}
enum AdmissionPlan {
    Unmanaged,
    Managed {
        store: Arc<crate::safety::Store>,
        qualification: crate::safety::Qualification,
        decision: AdmissionDecision,
    },
}
impl PreparedAttempt {
    fn progress(shared: &Shared, board: &BoardConfig) -> Guard {
        let progress = shared.process.progress.clone();
        let index = shared
            .config
            .hardware
            .boards
            .iter()
            .position(|configured| configured.id == board.id)
            .unwrap_or(0);
        progress.begin(
            Resource::BoardPreparation(
                BoardSlot::from_index(index).expect("at most three configured boards"),
            ),
            0,
            0,
            Stage::BoardPreparation,
        )
    }
    pub(super) async fn start(supervisor: &mut BoardSupervisor) -> Result<()> {
        let state = BoardAttempt::claim(supervisor)?;
        if state.shared.persistent_owners() {
            let Some(prepared) = Self::reacquire(state, &mut supervisor.shutdown).await? else {
                return Ok(());
            };
            supervisor.board = prepared.state.board.clone();
            return prepared
                .run_native(supervisor, supervisor.safety.clone())
                .await;
        }
        let plan = state.admission_plan(supervisor.safety.clone())?;
        if state.engines == 0
            && let AdmissionPlan::Managed {
                store,
                decision: AdmissionDecision::Reuse,
                ..
            } = &plan
        {
            state.mark_admitted(
                store,
                state.board.serial.as_deref().expect("managed serial"),
            )?;
            return state.park(&mut supervisor.shutdown).await;
        }
        let Some(prepared) = Self::reacquire(state, &mut supervisor.shutdown).await? else {
            return Ok(());
        };
        supervisor.board = prepared.state.board.clone();
        prepared.admit(supervisor, plan).await
    }
    async fn reacquire(
        mut state: BoardAttempt,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<Option<Self>> {
        let Some(initialization) = acquire_initialization(shutdown).await else {
            return Ok(None);
        };
        let preparing = Self::progress(&state.shared, &state.board);
        tokio::select! { result=locate(&state.shared, &mut state.board)=>result?, _=shutdown.changed()=>return Ok(None) }
        Ok(Some(Self {
            state,
            initialization,
            preparing,
        }))
    }
    async fn admit(self, supervisor: &mut BoardSupervisor, plan: AdmissionPlan) -> Result<()> {
        let AdmissionPlan::Managed {
            store,
            qualification: q,
            decision,
        } = plan
        else {
            return self.run_native(supervisor, None).await;
        };
        if decision == AdmissionDecision::Reuse {
            return self.run_native(supervisor, Some(store)).await;
        }
        let Self {
            state,
            initialization,
            preparing,
        } = self;
        let serial = state.board.serial.clone().expect("managed serial");
        state.begin_attempt();
        store.begin_owner(&serial, q.point)?;
        {
            let mut runtime = state.shared.runtime.lock().expect("runtime");
            runtime
                .boards
                .entry(state.board.id.clone())
                .or_default()
                .lifecycle
                .checking();
            runtime.event(Some(state.board.id.clone()), "admission_check");
        }
        let checked = admission::check(
            &state.shared,
            &state.board,
            &q,
            initialization,
            admission::Rules::default(),
            supervisor.shutdown.clone(),
            Some(preparing),
        )
        .await;
        if matches!(checked, Ok(admission::Outcome::Cancelled)) {
            store.closed(&serial)?;
            return Ok(());
        }
        if let Err(error) = checked {
            if let Err(saved) = store.quarantine(&serial, classify(&error).into()) {
                tracing::error!(%saved,"Admission fault could not be checkpointed");
            }
            return Err(error);
        }
        store.qualify(&serial, q.clone())?;
        state.mark_admitted(&store, &serial)?;
        if state.engines == 0 {
            return state.park(&mut supervisor.shutdown).await;
        }
        let Some(prepared) = Self::reacquire(state, &mut supervisor.shutdown).await? else {
            return Ok(());
        };
        prepared.run_native(supervisor, Some(store)).await
    }
    async fn run_native(
        self,
        supervisor: &mut BoardSupervisor,
        admission: Option<Arc<crate::safety::Store>>,
    ) -> Result<()> {
        let Self {
            state,
            initialization,
            preparing,
        } = self;
        if *supervisor.shutdown.borrow() {
            return Ok(());
        }
        let startup = startup(&state.shared, &state.board).await?;
        if let Some(store) = admission {
            store.begin_owner(
                state.board.serial.as_deref().expect("managed serial"),
                startup.point,
            )?;
        }
        state.begin_attempt();
        let lease = state.serial_lease;
        let child = launch_prepared(&state.shared, &state.board, &startup).await?;
        if let Some(metrics) = state.shared.metrics() {
            metrics.client_started(
                &crate::observability::board(&state.shared.config, &state.board.id),
                supervisor.previous_failure,
            );
        }
        let result = supervise(
            state.shared.clone(),
            state.board.clone(),
            child,
            state.ready,
            supervisor.shutdown.clone(),
            initialization,
            Some(preparing),
            startup,
        )
        .await;
        let checkpoint = match (&supervisor.safety, state.board.serial.as_deref()) {
            (Some(store), Some(serial)) => {
                checkpoint_retired(&state.shared, &state.board, store, serial)
            }
            _ => Ok(()),
        };
        if let (Err(error), Err(saved)) = (&result, &checkpoint) {
            tracing::error!(%error, %saved, "Native retirement could not be checkpointed");
        }
        drop(lease);
        // The recovery path needs the typed native fault even if persistence failed.
        result.and(checkpoint)
    }
}
impl BoardAttempt {
    fn claim(supervisor: &mut BoardSupervisor) -> Result<Self> {
        let serial_lease = match (&supervisor.safety, supervisor.board.serial.as_deref()) {
            (Some(store), Some(serial)) => Some(store.lease(serial)?),
            (Some(_), None) => bail!("Managed ASIC needs a persistent serial identity"),
            _ => None,
        };
        let engines = supervisor
            .shared
            .assignments
            .iter()
            .filter(|a| a.board_id == supervisor.board.id)
            .count();
        Ok(Self {
            shared: supervisor.shared.clone(),
            board: supervisor.board.clone(),
            serial_lease,
            ready: supervisor
                .ready
                .take()
                .unwrap_or_else(|| oneshot::channel().0),
            engines,
        })
    }
    fn admission_plan(&self, safety: Option<Arc<crate::safety::Store>>) -> Result<AdmissionPlan> {
        let Some(store) = safety else {
            return Ok(AdmissionPlan::Unmanaged);
        };
        let serial = self
            .board
            .serial
            .as_deref()
            .context("Managed ASIC needs a persistent serial identity")?;
        let qualification =
            admission::qualification(&self.shared, &self.board, self.prospective_engines()?)?;
        let decision = AdmissionDecision::for_qualification(&store, serial, &qualification);
        Ok(AdmissionPlan::Managed {
            store,
            qualification,
            decision,
        })
    }

    fn prospective_engines(&self) -> Result<usize> {
        if self.engines > 0 {
            return Ok(self.engines);
        }
        let blocked = unavailable(&self.shared);
        let mut candidate = self.shared.config.clone();
        candidate
            .hardware
            .boards
            .retain(|board| board.id == self.board.id || !blocked.contains(&board.id));
        candidate.hardware.chain_assignments = None;
        Ok(crate::assignment::resolve(&candidate)?
            .iter()
            .filter(|a| a.board_id == self.board.id)
            .count())
    }
    fn begin_attempt(&self) {
        let mut state = self.shared.runtime.lock().expect("runtime");
        let hardware = state.boards.entry(self.board.id.clone()).or_default();
        hardware.lifecycle.begin_attempt(crate::unix_now());
        hardware.raw_speeds.clear();
        hardware.temperature_unix = 0;
        hardware.temperature_c = None;
    }
    fn mark_admitted(&self, store: &crate::safety::Store, serial: &str) -> Result<()> {
        let epoch = store
            .board(serial)
            .context("Admitted board missing")?
            .owner_epoch;
        let mut state = self.shared.runtime.lock().expect("runtime");
        let lifecycle = &mut state
            .boards
            .entry(self.board.id.clone())
            .or_default()
            .lifecycle;
        if lifecycle.admitted_inventory_epoch() != Some(epoch) {
            lifecycle.admitted(epoch);
        }
        Ok(())
    }
    async fn park(self, shutdown: &mut watch::Receiver<bool>) -> Result<()> {
        let lease = self.serial_lease;
        let _ = self.ready.send(());
        let _ = shutdown.changed().await;
        drop(lease);
        Ok(())
    }
}

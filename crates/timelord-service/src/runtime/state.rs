//! Cached observations and board lifecycle state, independent of node sessions.
use crate::{asic::ProofTiming, unix_now};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use timelord_core::Chain;

#[derive(Clone, Serialize, Deserialize)]
pub struct Observation {
    pub id: u64,
    pub chain: Chain,
    pub timestamp_unix: u64,
    pub work_started_unix: u64,
    pub ips: f64,
    /// The board and physical engine that produced the proof. Older
    /// checkpoints lack these; routing changes never relabel a sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<u8>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: u64,
    pub timestamp_unix: u64,
    pub board_id: Option<String>,
    pub kind: String,
    pub end_unix: Option<u64>,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RawState {
    Running,
    Sampling,
    Idle,
}
#[derive(Clone, Serialize)]
pub struct RawSpeed {
    pub state: RawState,
    pub engine: usize,
    pub ips: f64,
    pub timestamp_unix: u64,
}
#[derive(Clone, Default, Serialize)]
pub struct HardwareState {
    #[serde(flatten)]
    pub lifecycle: crate::board_lifecycle::BoardLifecycle,
    pub device_location: Option<u32>,
    #[serde(skip)]
    pub fault_kind: Option<crate::hardware::NativeFailureKind>,
    pub raw_speeds: Vec<RawSpeed>,
    pub temperature_c: Option<f64>,
    pub frequency_mhz: Option<f64>,
    pub voltage_v: Option<f64>,
    pub recoveries: u64,
    pub temperature_unix: u64,
    #[serde(skip)]
    pub temperature_samples: u64,
    pub automatic: Option<crate::automatic::Status>,
    pub recovery_reason: Option<String>,
    pub retry_after_unix: Option<u64>,
    pub native_pid: Option<u32>,
    /// Lane errors for one native generation; repeated errors retire the owner.
    #[serde(skip)]
    pub lane_errors: (u64, std::collections::VecDeque<u64>),
    /// The current owner reached its best-known point after a cautious start.
    #[serde(skip)]
    pub cautious: bool,
    /// An operator asked to retry now; the supervisor's wait ends early.
    #[serde(skip)]
    pub retry_requested: bool,
}
impl HardwareState {
    pub fn fault(&mut self, error: &anyhow::Error) {
        self.fault_kind = error
            .downcast_ref::<crate::hardware::NativeFailure>()
            .map(|e| e.kind);
    }
    pub fn raw_sample(&mut self, engine: usize, ips: f64, state: RawState, now: u64) {
        if let Some(sample) = self.raw_speeds.iter_mut().find(|s| s.engine == engine) {
            sample.state = state;
            if state == RawState::Running {
                sample.ips = ips;
                sample.timestamp_unix = now;
            }
        } else {
            self.raw_speeds.push(RawSpeed {
                engine,
                ips,
                state,
                timestamp_unix: now,
            });
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeScope {
    Production,
    Admission,
}
impl RuntimeScope {
    pub(crate) fn io(
        self,
        lane: timelord_supervisor::progress::ProofLane,
    ) -> timelord_supervisor::progress::Resource {
        use timelord_supervisor::progress::Resource;
        match self {
            Self::Production => Resource::ProofIo(lane),
            Self::Admission => Resource::AdmissionIo(lane),
        }
    }
    pub(crate) fn arithmetic(
        self,
        lane: timelord_supervisor::progress::ProofLane,
    ) -> timelord_supervisor::progress::Resource {
        use timelord_supervisor::progress::Resource;
        match self {
            Self::Production => Resource::Arithmetic(lane),
            Self::Admission => Resource::AdmissionArithmetic(lane),
        }
    }
}
pub struct RuntimeState {
    pub experiments: Arc<crate::experiments::Gate>,
    pub safety: Option<Arc<crate::safety::Store>>,
    /// Live chain routes. Every native owner keeps three engine sockets; only
    /// these assignments receive work.
    pub routing: Vec<timelord_core::Assignment>,
    /// Incremented when the node accepts a new peak. Optional rebalancing waits
    /// for this boundary so it never cancels in-flight work for an improvement.
    pub peak_epoch: u64,
    /// Any board starting, retiring or being rerouted holds Automatic tuning on
    /// every board until this time. Shared USB disturbances are not evidence
    /// about a healthy board's operating point.
    pub disturbed_until_unix: u64,
    pub prover: std::sync::Weak<crate::asic::AsicBackend>,
    scope: RuntimeScope,
    pub metric_epoch: u64,
    pub pending: [u32; 3],
    pub scheduler_initialized: bool,
    pub scheduler_height: Option<u32>,
    pub scheduler_deficit: Option<u8>,
    pub phase: String,
    pub node: String,
    pub message: String,
    pub observations: crate::history_buffer::History<Observation>,
    pub estimated_observations: crate::history_buffer::History<Observation>,
    pub events: crate::history_buffer::History<Event>,
    pub proofs_delivered: u64,
    pub failed_proofs: u64,
    pub tuning_paused: bool,
    pub(crate) events_created: u64,
    pub(crate) observations_created: u64,
    pub(crate) archive: Option<crate::history_store::Snapshot>,
    pub boards: std::collections::HashMap<String, HardwareState>,
}
impl RuntimeState {
    pub fn new(tuning_paused: bool) -> Self {
        Self {
            scope: RuntimeScope::Production,
            experiments: Default::default(),
            safety: None,
            routing: Vec::new(),
            peak_epoch: 0,
            disturbed_until_unix: 0,
            prover: Default::default(),
            metric_epoch: 0,
            pending: [0; 3],
            scheduler_initialized: false,
            scheduler_height: None,
            scheduler_deficit: None,
            phase: "setup_required".into(),
            node: "not_configured".into(),
            message: "Connect a full node to get started.".into(),
            observations: Default::default(),
            estimated_observations: Default::default(),
            events: Default::default(),
            proofs_delivered: 0,
            failed_proofs: 0,
            tuning_paused,
            events_created: 0,
            observations_created: 0,
            archive: None,
            boards: Default::default(),
        }
    }
}
#[cfg(test)]
impl Default for RuntimeState {
    fn default() -> Self {
        Self::new(false)
    }
}
impl RuntimeState {
    pub(crate) fn admission() -> Self {
        let mut state = Self::new(false);
        state.scope = RuntimeScope::Admission;
        state
    }
    pub(crate) fn scope(&self) -> RuntimeScope {
        self.scope
    }
    /// Hold Automatic tuning on every board until `now + seconds`.
    pub fn disturb(&mut self, now: u64, seconds: u64) {
        self.disturbed_until_unix = self.disturbed_until_unix.max(now.saturating_add(seconds));
    }
    pub fn disturbed(&self, now: u64) -> bool {
        now < self.disturbed_until_unix
    }
    /// A different node profile starts a new chain context. Board state,
    /// routes and per-board history are retained.
    pub fn reset_node_context(&mut self) {
        self.scheduler_initialized = false;
        self.scheduler_height = None;
        self.scheduler_deficit = None;
        self.pending = [0; 3];
        self.archive = None;
        // Session proof counters belong to the node they were sent to: the
        // configuration checkpoint must not treat proofs sent to the previous
        // node as evidence that the new profile works.
        self.proofs_delivered = 0;
        self.failed_proofs = 0;
        self.transition(
            "waiting_for_node",
            "connecting",
            "Switching full-node profile; ASIC boards keep running.",
        );
    }
    pub fn transition(&mut self, phase: &str, node: &str, message: &str) {
        self.phase = phase.into();
        self.node = node.into();
        self.message = message.into();
    }
    pub fn event(&mut self, board_id: Option<String>, kind: &str) {
        self.events_created += 1;
        self.events.push(Event {
            id: self.events_created,
            timestamp_unix: unix_now(),
            board_id,
            kind: kind.into(),
            end_unix: None,
        });
        self.events
            .expire_front(|e| e.timestamp_unix < unix_now().saturating_sub(86400));
    }
    pub(super) fn connected(&mut self) {
        for event in self.events.iter_mut() {
            if event.kind == "node_unavailable" && event.end_unix.is_none() {
                event.end_unix = Some(unix_now());
            }
        }
    }
    pub fn estimated(
        &mut self,
        chain: Chain,
        ips: f64,
        work_started_unix: u64,
        board_id: &str,
        engine: u8,
    ) {
        self.observations_created += 1;
        self.estimated_observations.push(Observation {
            id: self.observations_created,
            chain,
            ips,
            timestamp_unix: unix_now(),
            work_started_unix,
            board_id: Some(board_id.to_owned()),
            engine: Some(engine),
        });
        self.estimated_observations
            .expire_front(|o| o.timestamp_unix < unix_now().saturating_sub(86400));
    }
    pub(super) fn delivered(&mut self, speeds: Vec<(Chain, ProofTiming)>) {
        for (chain, timing) in speeds {
            let ips = timing.delivered_ips();
            self.observations_created += 1;
            self.observations.push(Observation {
                id: self.observations_created,
                chain,
                ips,
                timestamp_unix: unix_now(),
                work_started_unix: timing.started_unix(),
                board_id: Some(timing.board.clone()),
                engine: Some(timing.engine),
            });
            self.proofs_delivered += 1;
        }
        self.observations
            .expire_front(|e| e.timestamp_unix < unix_now().saturating_sub(86400));
        self.phase = "running".into();
        self.message = "Producing verified proofs for your full node.".into();
    }
}

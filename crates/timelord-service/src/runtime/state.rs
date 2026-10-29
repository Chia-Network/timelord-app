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
    pub raw_speeds: Vec<RawSpeed>,
    pub temperature_c: Option<f64>,
    pub frequency_mhz: Option<f64>,
    pub voltage_v: Option<f64>,
    pub recoveries: u64,
    pub temperature_unix: u64,
    #[serde(skip)]
    pub temperature_samples: u64,
    pub automatic: Option<crate::automatic::Status>,
}
impl HardwareState {
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
pub struct RuntimeState {
    pub experiments: Arc<crate::experiments::Gate>,
    pub safety: Option<Arc<crate::safety::Store>>,
    pub progress: Option<Arc<timelord_supervisor::progress::Progress>>,
    pub metrics: Option<Arc<crate::observability::Metrics>>,
    pub metric_epoch: u64,
    pub pending: [u32; 3],
    pub scheduler_initialized: bool,
    pub scheduler_height: Option<u32>,
    pub scheduler_deficit: Option<u8>,
    pub phase: String,
    pub node: String,
    pub message: String,
    pub observations: Vec<Observation>,
    pub estimated_observations: Vec<Observation>,
    pub events: Vec<Event>,
    pub proofs_delivered: u64,
    pub failed_proofs: u64,
    pub tuning_paused: bool,
    pub(crate) events_created: u64,
    pub(crate) observations_created: u64,
    pub(crate) archive: Option<crate::history_store::Snapshot>,
    pub boards: std::collections::HashMap<String, HardwareState>,
}
impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            experiments: Default::default(),
            safety: None,
            progress: None,
            metrics: None,
            metric_epoch: 0,
            pending: [0; 3],
            scheduler_initialized: false,
            scheduler_height: None,
            scheduler_deficit: None,
            phase: "setup_required".into(),
            node: "not_configured".into(),
            message: "Connect a full node to get started.".into(),
            observations: vec![],
            estimated_observations: vec![],
            events: vec![],
            proofs_delivered: 0,
            failed_proofs: 0,
            tuning_paused: false,
            events_created: 0,
            observations_created: 0,
            archive: None,
            boards: Default::default(),
        }
    }
}
impl RuntimeState {
    pub fn with_tuning_paused(tuning_paused: bool) -> Self {
        Self {
            tuning_paused,
            ..Default::default()
        }
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
            .retain(|e| e.timestamp_unix >= unix_now().saturating_sub(86400));
    }
    pub(super) fn connected(&mut self) {
        for event in &mut self.events {
            if event.kind == "node_unavailable" && event.end_unix.is_none() {
                event.end_unix = Some(unix_now());
            }
        }
    }
    pub fn estimated(&mut self, chain: Chain, ips: f64, work_started_unix: u64) {
        self.observations_created += 1;
        self.estimated_observations.push(Observation {
            id: self.observations_created,
            chain,
            ips,
            timestamp_unix: unix_now(),
            work_started_unix,
        });
        self.estimated_observations
            .retain(|o| o.timestamp_unix >= unix_now().saturating_sub(86400));
        let excess = self.estimated_observations.len().saturating_sub(86400);
        self.estimated_observations.drain(..excess);
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
            });
            self.proofs_delivered += 1;
        }
        if self.observations.len() > 86400 {
            self.observations.drain(..self.observations.len() - 86400);
        }
        self.observations
            .retain(|e| e.timestamp_unix >= unix_now().saturating_sub(86400));
        self.phase = "running".into();
        self.message = "Producing verified proofs for your full node.".into();
    }
}

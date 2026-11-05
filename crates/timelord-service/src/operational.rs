//! Process-lifetime supervision, independent of metrics and topology generations.
use std::sync::{Arc, Mutex};
use timelord_supervisor::progress::Progress;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Running,
    Reconfiguring,
    Stopping,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub phase: Phase,
    pub failure: Option<Failure>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Observation,
    Essential,
}
impl Snapshot {
    pub fn healthy(self) -> bool {
        self.failure.is_none() && self.phase != Phase::Stopping
    }
    pub fn ready(self) -> bool {
        self.healthy() && self.phase == Phase::Running
    }
    pub fn producing(self) -> bool {
        self.phase == Phase::Running && self.failure != Some(Failure::Essential)
    }
}
pub struct State {
    lifecycle: Mutex<Snapshot>,
    pub progress: Arc<Progress>,
    pub metrics: Arc<crate::observability::Metrics>,
}
impl State {
    pub fn new(metrics: Arc<crate::observability::Metrics>) -> Arc<Self> {
        Arc::new(Self {
            lifecycle: Mutex::new(Snapshot::default()),
            progress: Arc::default(),
            metrics,
        })
    }
    #[cfg(test)]
    pub fn fixture() -> Arc<Self> {
        Self::new(crate::observability::Metrics::new(timelord_core::Network::Mainnet).unwrap())
    }

    pub fn snapshot(&self) -> Snapshot {
        *self.lifecycle.lock().expect("process lifecycle")
    }
    /// A projection failure affects health while productive work may continue.
    pub fn fail(&self) {
        self.lifecycle
            .lock()
            .expect("process lifecycle")
            .failure
            .get_or_insert(Failure::Observation);
    }
    pub fn essential_failure(&self) {
        self.lifecycle.lock().expect("process lifecycle").failure = Some(Failure::Essential);
        self.progress.fail();
    }
    pub fn reconfigure(&self) {
        self.phase(Phase::Reconfiguring);
    }
    pub fn resume(&self) {
        self.phase(Phase::Running);
    }
    pub fn stop(&self) {
        self.phase(Phase::Stopping);
    }
    fn phase(&self, phase: Phase) {
        let mut lifecycle = self.lifecycle.lock().expect("process lifecycle");
        if lifecycle.phase != Phase::Stopping {
            lifecycle.phase = phase;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_generations_cannot_clear_process_failure() {
        let process = State::fixture();
        process.essential_failure();
        let _replacement = crate::runtime::RuntimeState::new(true);
        assert!(!process.snapshot().healthy());
        assert!(process.progress.snapshot(1, 1).unwrap().failed);
    }
    #[test]
    fn reconfiguration_never_clears_failure_or_revives_shutdown() {
        let process = State::fixture();
        process.reconfigure();
        assert!(process.snapshot().healthy());
        assert!(!process.snapshot().ready());
        process.fail();
        process.resume();
        assert!(!process.snapshot().healthy());
        assert!(
            process.snapshot().producing(),
            "observer failure does not stop proof work"
        );
        process.stop();
        process.resume();
        assert_eq!(process.snapshot().phase, Phase::Stopping);
        assert!(!process.snapshot().producing());
    }
}

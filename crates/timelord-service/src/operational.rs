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
#[derive(Default)]
pub struct State {
    lifecycle: Mutex<Snapshot>,
    pub progress: Arc<Progress>,
}
impl State {
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
    fn topology_transitions_never_clear_failure_or_revive_a_stopping_process() {
        let process = Arc::new(State::default());
        process.reconfigure();
        assert!(process.snapshot().healthy());
        assert!(!process.snapshot().ready());
        process.fail();
        process.resume();
        assert_eq!(process.snapshot().phase, Phase::Running);
        assert!(!process.snapshot().healthy());
        process.stop();
        process.resume();
        assert_eq!(process.snapshot().phase, Phase::Stopping);
    }
    #[test]
    fn runtime_replacement_keeps_the_process_owner_and_failure_latch() {
        let process = Arc::new(State::default());
        let first = crate::runtime::RuntimeState {
            process: process.clone(),
            ..Default::default()
        };
        first.process.essential_failure();
        let replacement = crate::runtime::RuntimeState {
            process: process.clone(),
            ..Default::default()
        };
        assert!(Arc::ptr_eq(&first.process, &replacement.process));
        assert!(!replacement.process.snapshot().healthy());
        assert!(replacement.process.progress.snapshot(1, 1).unwrap().failed);
    }
}

//! One exploratory clock trial at a time across all boards.
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub struct Gate {
    permit: Arc<Semaphore>,
}
impl Default for Gate {
    fn default() -> Self {
        Self {
            permit: Arc::new(Semaphore::new(1)),
        }
    }
}
impl Gate {
    pub fn trial(&self) -> Option<OwnedSemaphorePermit> {
        self.permit.clone().try_acquire_owned().ok()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_one_trial_runs_at_a_time() {
        let gate = Gate::default();
        let trial = gate.trial().unwrap();
        assert!(gate.trial().is_none());
        drop(trial);
        assert!(gate.trial().is_some());
    }
}

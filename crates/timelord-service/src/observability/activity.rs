//! Scheduled request lifetime, independent of retry attempt lifetimes.
use super::{ProofStage, index};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};
use timelord_core::Chain;

/// A request remains pending while its board recovers. Dropping it records cancellation.
pub struct Activity {
    runtime: Arc<Mutex<crate::runtime::RuntimeState>>,
    chain: Chain,
    metrics: Option<Arc<super::Metrics>>,
    started: Instant,
    finished: bool,
}
impl Activity {
    pub fn new(
        runtime: Arc<Mutex<crate::runtime::RuntimeState>>,
        c: Chain,
        metrics: Option<Arc<super::Metrics>>,
    ) -> Self {
        {
            let mut state = runtime.lock().expect("runtime");
            state.pending[index(c)] += 1;
        }
        Self {
            runtime,
            chain: c,
            metrics,
            started: Instant::now(),
            finished: false,
        }
    }
    pub fn finish(&mut self) {
        self.finished = true;
        if let Some(metrics) = &self.metrics {
            metrics.duration(
                self.chain,
                ProofStage::RequestToVerified,
                self.started.elapsed(),
            );
        }
    }
}
impl Drop for Activity {
    fn drop(&mut self) {
        let mut state = self.runtime.lock().expect("runtime");
        state.pending[index(self.chain)] = state.pending[index(self.chain)].saturating_sub(1);
        drop(state);
        if !self.finished
            && let Some(metrics) = &self.metrics
        {
            metrics.cancelled(self.chain);
        }
    }
}

//! Scheduled request lifetime, independent of retry attempt lifetimes.
use super::{ProofStage, index};
use std::{sync::Mutex, time::Instant};
use timelord_core::Chain;

/// A request remains pending while its board recovers. Dropping it records cancellation.
pub struct Activity<'a> {
    runtime: &'a Mutex<crate::runtime::RuntimeState>,
    chain: Chain,
    metrics: Option<&'a super::Metrics>,
    started: Instant,
    finished: bool,
}
impl<'a> Activity<'a> {
    pub fn new(
        runtime: &'a Mutex<crate::runtime::RuntimeState>,
        chain: Chain,
        metrics: Option<&'a super::Metrics>,
    ) -> Self {
        {
            let mut state = runtime.lock().expect("runtime");
            state.pending[index(chain)] += 1;
        }
        Self {
            runtime,
            chain,
            metrics,
            started: Instant::now(),
            finished: false,
        }
    }
    pub fn finish(&mut self) {
        self.finished = true;
        if let Some(metrics) = self.metrics {
            metrics.duration(
                self.chain,
                ProofStage::RequestToVerified,
                self.started.elapsed(),
            );
        }
    }
}
impl Drop for Activity<'_> {
    fn drop(&mut self) {
        let mut state = self.runtime.lock().expect("runtime");
        state.pending[index(self.chain)] = state.pending[index(self.chain)].saturating_sub(1);
        drop(state);
        if !self.finished
            && let Some(metrics) = self.metrics
        {
            metrics.cancelled(self.chain);
        }
    }
}

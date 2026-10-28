//! One exploratory clock trial or offline admission at a time. Pending admission
//! takes priority; a trial can yield without changing its current hardware point.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub struct Gate {
    permit: Arc<Semaphore>,
    pending: AtomicUsize,
}
impl Default for Gate {
    fn default() -> Self {
        Self {
            permit: Arc::new(Semaphore::new(1)),
            pending: AtomicUsize::new(0),
        }
    }
}
impl Gate {
    pub fn admission_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) > 0
    }
    pub fn trial(&self) -> Option<OwnedSemaphorePermit> {
        if self.admission_pending() {
            return None;
        }
        let permit = self.permit.clone().try_acquire_owned().ok()?;
        if self.admission_pending() {
            return None;
        }
        Some(permit)
    }
    pub async fn admission(&self) -> OwnedSemaphorePermit {
        struct Pending<'a>(&'a AtomicUsize);
        impl Drop for Pending<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        self.pending.fetch_add(1, Ordering::AcqRel);
        let _pending = Pending(&self.pending);
        self.permit
            .clone()
            .acquire_owned()
            .await
            .expect("experiment gate stays open")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn admissions_take_priority_and_cancelled_waiters_leave_no_reservation() {
        let gate = Arc::new(Gate::default());
        let trial = gate.trial().unwrap();
        assert!(gate.trial().is_none());
        let clone = gate.clone();
        let waiter = tokio::spawn(async move { clone.admission().await });
        while !gate.admission_pending() {
            tokio::task::yield_now().await;
        }
        assert!(gate.trial().is_none());
        waiter.abort();
        let _ = waiter.await;
        assert!(!gate.admission_pending());
        drop(trial);
        let admission = gate.admission().await;
        assert!(gate.trial().is_none());
        drop(admission);
        assert!(gate.trial().is_some());
    }
}

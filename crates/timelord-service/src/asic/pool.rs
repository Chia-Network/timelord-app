//! Cancellation-safe physical engine queues. Unselected engines remain parked.
use super::Lease;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

struct State {
    parked: Vec<Lease>,
    closed: bool,
}
struct Shared {
    state: Mutex<State>,
    notify: Notify,
    #[cfg(test)]
    interleave: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
/// Listener side of one board queue; dropping it ends the stream.
pub(crate) struct Offers(Arc<Shared>);
/// Waiter side of one board queue.
#[derive(Clone)]
pub(crate) struct Pool(Arc<Shared>);

pub(crate) fn pair() -> (Offers, Pool) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            parked: Vec::new(),
            closed: false,
        }),
        notify: Notify::new(),
        #[cfg(test)]
        interleave: Mutex::new(None),
    });
    (Offers(shared.clone()), Pool(shared))
}

impl Offers {
    pub fn offer(&self, lease: Lease) {
        self.0.state.lock().expect("ASIC pool").parked.push(lease);
        self.0.notify.notify_waiters();
    }
}
impl Drop for Offers {
    fn drop(&mut self) {
        self.0.state.lock().expect("ASIC pool").closed = true;
        self.0.notify.notify_waiters();
    }
}

impl Pool {
    pub async fn receive(&self, slot: Option<u8>, generation: u64) -> Option<Lease> {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // The `Notified` is created before the recheck, so a later
            // `notify_waiters` cannot be missed; `enable()` is only defensive
            // under this signalling.
            notified.as_mut().enable();
            let (found, pruned) = {
                let mut state = self.0.state.lock().expect("ASIC pool");
                // A newer generation must not stay pinned by an older waiter.
                let (pruned, kept) = std::mem::take(&mut state.parked)
                    .into_iter()
                    .partition(|lease| lease.generation < generation);
                state.parked = kept;
                let index = state.parked.iter().position(|lease| {
                    lease.generation == generation && slot.is_none_or(|s| s == lease.slot)
                });
                let found = match index {
                    Some(index) => Some(Some(state.parked.swap_remove(index))),
                    None if state.closed => Some(None),
                    None => None,
                };
                (found, pruned)
            };
            drop(pruned); // releases stale client claims outside the lock
            match found {
                Some(lease) => return lease,
                None => {
                    #[cfg(test)]
                    {
                        let hook = self
                            .0
                            .interleave
                            .lock()
                            .expect("ASIC pool interleave")
                            .take();
                        if let Some(hook) = hook {
                            hook();
                        }
                    }
                    notified.await
                }
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn on_wait(&self, hook: impl FnOnce() + Send + 'static) {
        *self.0.interleave.lock().expect("ASIC pool interleave") = Some(Box::new(hook));
    }
    pub fn clear(&self) {
        let parked = std::mem::take(&mut self.0.state.lock().expect("ASIC pool").parked);
        drop(parked);
    }
}

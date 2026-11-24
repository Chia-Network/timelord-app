//! Cancellation-safe physical engine queues. Unselected engines remain parked.
use super::Lease;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    task::AbortHandle,
};

struct State {
    parked: Vec<Lease>,
    /// In-flight `receive` calls per generation. A parked lease may only be
    /// discarded when its generation has no waiter that could still take it.
    waiting: HashMap<u64, usize>,
    closed: bool,
}

/// Registers a generation for the whole life of one `receive` future, including
/// cancellation and timeout, so other waiters never discard its live leases.
struct Waiter<'a> {
    state: &'a Mutex<State>,
    generation: u64,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        let mut state = self.state.lock().expect("ASIC pool");
        if let Some(count) = state.waiting.get_mut(&self.generation) {
            *count -= 1;
        }
        state.waiting.retain(|_, count| *count > 0);
    }
}

/// One background owner of the listener channel. Waiters only touch the short
/// parked-lease critical section, so a slow handshake on one engine cannot stall
/// another engine on the same board.
pub(super) struct Pool {
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    clear: mpsc::UnboundedSender<oneshot::Sender<()>>,
    pump: AbortHandle,
}

impl Pool {
    pub fn new(receiver: mpsc::Receiver<Lease>) -> Self {
        let state = Arc::new(Mutex::new(State {
            parked: Vec::new(),
            waiting: HashMap::new(),
            closed: false,
        }));
        let notify = Arc::new(Notify::new());
        let (clear, commands) = mpsc::unbounded_channel();
        let pump = tokio::spawn(route(receiver, state.clone(), notify.clone(), commands));
        Self {
            state,
            notify,
            clear,
            pump: pump.abort_handle(),
        }
    }

    pub async fn receive(&self, slot: Option<u8>, generation: u64) -> Option<Lease> {
        {
            let mut state = self.state.lock().expect("ASIC pool");
            *state.waiting.entry(generation).or_insert(0) += 1;
        }
        let _waiter = Waiter {
            state: &self.state,
            generation,
        };
        loop {
            // Register before rechecking so a lease parked by the router between
            // the check and the await cannot miss this waiter.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state.lock().expect("ASIC pool");
                let State {
                    parked,
                    waiting,
                    closed,
                } = &mut *state;
                // Keep leases another in-flight waiter may still take, plus
                // generations newer than this waiter: an old generation must
                // never discard a lease from a generation that has not had its
                // chance to wait yet. Only provably stale leases are dropped.
                parked.retain(|lease| {
                    waiting.contains_key(&lease.generation) || lease.generation > generation
                });
                if let Some(index) = parked.iter().position(|lease| {
                    lease.generation == generation && slot.is_none_or(|s| s == lease.slot)
                }) {
                    return Some(parked.swap_remove(index));
                }
                if *closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    pub async fn clear(&self) {
        let (acknowledge, cleared) = oneshot::channel();
        if self.clear.send(acknowledge).is_err() {
            return;
        }
        let _ = cleared.await;
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

async fn route(
    mut receiver: mpsc::Receiver<Lease>,
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    mut commands: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
) {
    let mut open = true;
    loop {
        if !open {
            match commands.recv().await {
                Some(acknowledge) => {
                    state.lock().expect("ASIC pool").parked.clear();
                    let _ = acknowledge.send(());
                }
                None => return,
            }
            continue;
        }
        tokio::select! {
            biased;
            Some(acknowledge) = commands.recv() => {
                let mut state = state.lock().expect("ASIC pool");
                state.parked.clear();
                while receiver.try_recv().is_ok() {}
                drop(state);
                let _ = acknowledge.send(());
                notify.notify_waiters();
            }
            lease = receiver.recv() => match lease {
                Some(lease) => {
                    state.lock().expect("ASIC pool").parked.push(lease);
                    notify.notify_waiters();
                }
                None => {
                    open = false;
                    state.lock().expect("ASIC pool").closed = true;
                    notify.notify_waiters();
                }
            },
        }
    }
}

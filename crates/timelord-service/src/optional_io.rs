//! Optional disk writes retain caller-owned capacity without holding runtime exit.
use anyhow::{Context, Result};
use tokio::sync::oneshot;

// Test-only bookkeeping so fixture teardown can wait for a detached final
// history write instead of racing its history.json atomic-write artifacts.
#[cfg(test)]
static HISTORY_WRITERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn history_writer_name(name: &str) -> bool {
    name == "history-checkpoint"
}

#[cfg(test)]
struct HistoryWriterGuard(&'static str);
#[cfg(test)]
impl Drop for HistoryWriterGuard {
    fn drop(&mut self) {
        if history_writer_name(self.0) {
            HISTORY_WRITERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// Number of detached history checkpoint writers still running. Tests use this
/// to wait for a late final flush before deleting its fixture directory.
#[cfg(test)]
pub fn history_writers_in_flight() -> usize {
    HISTORY_WRITERS.load(std::sync::atomic::Ordering::SeqCst)
}

// Tokio runtime destruction joins its blocking pool. A stalled optional fsync
// must instead retain its permit on a detached thread until it really finishes.
// Callers move owned data/permits into the closure and bound their async wait.
pub fn spawn<T: Send + 'static>(
    name: &'static str,
    write: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<oneshot::Receiver<Result<T>>> {
    let (tx, rx) = oneshot::channel();
    #[cfg(test)]
    let counted = history_writer_name(name);
    #[cfg(test)]
    if counted {
        HISTORY_WRITERS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    let spawned = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            #[cfg(test)]
            let _guard = HistoryWriterGuard(name);
            let _ = tx.send(write());
        });
    if spawned.is_err() {
        #[cfg(test)]
        if counted {
            HISTORY_WRITERS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    spawned.context("Starting optional disk writer")?;
    Ok(rx)
}

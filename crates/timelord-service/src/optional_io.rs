//! Optional disk writes retain caller-owned capacity without holding runtime exit.
use anyhow::{Context, Result};
use tokio::sync::oneshot;

// Tokio runtime destruction joins its blocking pool. A stalled optional fsync
// must instead retain its permit on a detached thread until it really finishes.
// Callers move owned data/permits into the closure and bound their async wait.
pub fn spawn<T: Send + 'static>(
    name: &'static str,
    write: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<oneshot::Receiver<Result<T>>> {
    let (tx, rx) = oneshot::channel();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _ = tx.send(write());
        })
        .context("Starting optional disk writer")?;
    Ok(rx)
}

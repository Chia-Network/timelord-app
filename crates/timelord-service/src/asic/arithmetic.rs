//! One real arithmetic job per chain, including work whose async caller was
//! cancelled. Process-wide permits survive replacement backend/topology objects.
use anyhow::Result;
use std::sync::{Arc, LazyLock};
use timelord_core::Chain;
use tokio::{sync::Semaphore, task::JoinHandle};

static CAPACITY: LazyLock<[Arc<Semaphore>; 3]> =
    LazyLock::new(|| std::array::from_fn(|_| Arc::new(Semaphore::new(1))));

pub(super) async fn spawn<T: Send + 'static>(
    chain: Chain,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<JoinHandle<T>> {
    let index = match chain {
        Chain::Challenge => 0,
        Chain::Reward => 1,
        Chain::InfusedChallenge => 2,
    };
    // Wait before entering the blocking pool. Cancellation of this wait queues
    // no CPU work; cancellation after spawn leaves the permit in the real job.
    let permit = CAPACITY[index].clone().acquire_owned().await?;
    Ok(tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    #[tokio::test]
    async fn cancelling_callers_cannot_queue_work_behind_a_running_job() {
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let caller = tokio::spawn(async move {
            spawn(Chain::Reward, move || {
                let _ = started.send(());
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            })
            .await
            .unwrap()
            .await
            .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .unwrap()
            .unwrap();
        caller.abort();
        let _ = caller.await;
        let executed = Arc::new(AtomicUsize::new(0));
        let mut waiters = Vec::new();
        for _ in 0..16 {
            let executed = executed.clone();
            waiters.push(tokio::spawn(async move {
                spawn(Chain::Reward, move || {
                    executed.fetch_add(1, Ordering::SeqCst)
                })
                .await
                .unwrap()
                .await
                .unwrap();
            }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(executed.load(Ordering::SeqCst), 0);
        for waiter in waiters {
            assert!(!waiter.is_finished());
            waiter.abort();
            let _ = waiter.await;
        }
        // Other chains retain independent capacity.
        assert_eq!(
            spawn(Chain::InfusedChallenge, || 7)
                .await
                .unwrap()
                .await
                .unwrap(),
            7
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            spawn(Chain::Reward, || 11).await.unwrap().await.unwrap()
        })
        .await
        .unwrap();
        assert_eq!(
            executed.load(Ordering::SeqCst),
            0,
            "cancelled requests queued blocking jobs"
        );
    }
}

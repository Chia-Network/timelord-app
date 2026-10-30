//! Real arithmetic jobs use the process owner's typed-resource capacity, including
//! jobs whose async callers were cancelled. Replacing a backend cannot reset it.
use anyhow::Result;
use std::sync::Arc;
use timelord_supervisor::progress::{Progress, Resource, Stage};
use tokio::task::JoinHandle;

pub(super) async fn spawn<T: Send + 'static>(
    progress: Arc<Progress>,
    resource: Resource,
    generation: u64,
    iterations: u64,
    stage: Stage,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<JoinHandle<T>> {
    // Waiting for capacity queues no blocking work. Both the permit and progress
    // guard belong to the actual job, including after caller cancellation.
    let permit = progress.arithmetic(resource).acquire_owned().await?;
    Ok(tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _stage = progress.begin(resource, generation, iterations, stage);
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
    use timelord_supervisor::progress::ProofLane;

    #[tokio::test]
    async fn cancelled_callers_cannot_queue_jobs_across_runtime_generations() {
        let process = Arc::new(crate::operational::State::default());
        let first = crate::runtime::RuntimeState {
            process: process.clone(),
            ..Default::default()
        };
        let replacement = crate::runtime::RuntimeState {
            process: process.clone(),
            ..Default::default()
        };
        assert!(Arc::ptr_eq(&first.process, &replacement.process));
        let resource = Resource::Arithmetic(ProofLane::Reward);
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let caller = tokio::spawn(async move {
            spawn(
                first.process.progress.clone(),
                resource,
                1,
                360,
                Stage::Preparing,
                move || {
                    let _ = started.send(());
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                },
            )
            .await
            .unwrap()
            .await
            .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .unwrap()
            .unwrap();
        let snapshot = process.progress.snapshot(1, 1).unwrap();
        let holder = snapshot
            .work
            .iter()
            .flatten()
            .find(|w| w.stage == Stage::Preparing)
            .unwrap()
            .id;
        caller.abort();
        let _ = caller.await;
        let executed = Arc::new(AtomicUsize::new(0));
        let mut waiters = Vec::new();
        for _ in 0..16 {
            let executed = executed.clone();
            let progress = replacement.process.progress.clone();
            waiters.push(tokio::spawn(async move {
                spawn(progress, resource, 2, 360, Stage::Verifying, move || {
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
        let snapshot = process.progress.snapshot(2, 2).unwrap();
        let active: Vec<_> = snapshot.work.iter().flatten().collect();
        assert_eq!(
            active.len(),
            1,
            "queued jobs must not claim actual-job progress"
        );
        assert_eq!(active[0].id, holder);
        for waiter in waiters {
            assert!(!waiter.is_finished());
            waiter.abort();
            let _ = waiter.await;
        }
        // Other typed resources retain independent capacity on the same process.
        assert_eq!(
            spawn(
                process.progress.clone(),
                Resource::AdmissionArithmetic(ProofLane::Reward),
                2,
                360,
                Stage::Preparing,
                || 7
            )
            .await
            .unwrap()
            .await
            .unwrap(),
            7
        );
        release.send(()).unwrap();
        let progress = replacement.process.progress.clone();
        let value = tokio::time::timeout(Duration::from_secs(2), async move {
            spawn(progress, resource, 2, 360, Stage::Verifying, || 11)
                .await
                .unwrap()
                .await
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(value, 11);
        assert_eq!(
            executed.load(Ordering::SeqCst),
            0,
            "cancelled requests queued blocking jobs"
        );
    }
}

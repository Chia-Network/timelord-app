//! Retirement cause, grace budget and the state transitions that apply a stop.
use super::{Lifecycle, Published, SupervisorState, Worker, retry_delay};
use anyhow::Result;
use std::{io::Write, time::Duration};
use timelord_supervisor::{
    Mode, NORMAL_GRACEFUL_STOP, WATCHDOG_GRACEFUL_STOP,
    progress::{Failure, FailureKind},
};
use tokio::{sync::watch, time::Instant};

pub(crate) type Retirement =
    tokio::task::JoinHandle<(Box<Worker>, Result<std::process::ExitStatus>)>;

/// Why a worker is being retired decides what follows a successful reap and how
/// long the process group gets to honour SIGTERM.
pub(crate) enum Cause {
    /// Pause, Reload, or Resume of a running worker.
    Requested,
    /// The group was already SIGKILLed but did not reap; still send SIGTERM to
    /// the leader, skip the SIGTERM wait, then group-SIGKILL and reap.
    CleanupRetry,
    /// Crash or watchdog failure: recorded as a recovery and backed off.
    Failed { watchdog: Option<Failure> },
}
impl Cause {
    pub(crate) fn grace(&self) -> Duration {
        match self {
            Self::Failed {
                watchdog:
                    Some(Failure {
                        kind: FailureKind::Unresponsive,
                        ..
                    }),
            } => WATCHDOG_GRACEFUL_STOP,
            Self::Failed {
                watchdog:
                    Some(Failure {
                        kind: FailureKind::Reported,
                        ..
                    }),
            } => NORMAL_GRACEFUL_STOP,
            Self::CleanupRetry => Duration::ZERO,
            Self::Requested | Self::Failed { watchdog: None } => NORMAL_GRACEFUL_STOP,
        }
    }
}
pub(crate) struct Retiring {
    pub(crate) pid: u32,
    pub(crate) uptime: Duration,
    pub(crate) cause: Cause,
    /// An explicit Reload/Resume arrived while stopping: start the
    /// replacement without crash backoff once the old family is reaped.
    pub(crate) restart: bool,
    pub(crate) task: Retirement,
}

impl SupervisorState {
    /// Hand a live or cleanup-failed worker to a background retirement task.
    /// Returns without waiting: the loop keeps serving controls, progress and
    /// ticks, and `poll_retirement` applies the outcome.
    pub(crate) fn begin_retirement(&mut self, cause: Cause) {
        let mut worker = match std::mem::replace(&mut self.lifecycle, Lifecycle::Starting) {
            Lifecycle::Running(worker) | Lifecycle::CleanupFailed { worker, .. } => worker,
            other => {
                self.lifecycle = other;
                return;
            }
        };
        let pid = worker.process.pid();
        let uptime = worker.started.elapsed();
        let graceful = cause.grace();
        let task = tokio::spawn(async move {
            let result = worker.stop(graceful).await;
            (worker, result)
        });
        self.lifecycle = Lifecycle::Stopping(Box::new(Retiring {
            pid,
            uptime,
            cause,
            restart: false,
            task,
        }));
    }
    /// Apply a finished retirement. With `wait`, block until it finishes
    /// (shutdown and tests); otherwise return at once if it is still running.
    pub(crate) async fn poll_retirement(
        &mut self,
        snapshots: &watch::Sender<Published>,
        wait: bool,
    ) {
        if !matches!(&self.lifecycle, Lifecycle::Stopping(retiring) if wait || retiring.task.is_finished())
        {
            return;
        }
        let Lifecycle::Stopping(retiring) =
            std::mem::replace(&mut self.lifecycle, Lifecycle::Starting)
        else {
            unreachable!()
        };
        let Retiring {
            pid,
            uptime,
            cause,
            restart,
            task,
        } = *retiring;
        let joined = task.await;
        // Read intent after the task: a Pause accepted while stopping wins.
        let paused = self.intent.mode == Mode::Paused;
        self.lifecycle = match joined {
            Ok((worker, Ok(exit))) => {
                drop(worker); // Reaped: the group identity is released.
                match cause {
                    Cause::Failed { watchdog } => {
                        let reason = watchdog
                            .map(|failure| failure.reason)
                            .unwrap_or_else(|| format!("Worker exited: {exit}"));
                        self.record_recovery(Some(pid), &reason);
                        if paused {
                            Lifecycle::Paused
                        } else if restart {
                            Lifecycle::Starting
                        } else {
                            let delay = retry_delay(&mut self.failures, Some(uptime));
                            Lifecycle::Backoff {
                                next: Instant::now() + Duration::from_secs(delay),
                                error: format!("{reason}; retry in {delay}s"),
                            }
                        }
                    }
                    Cause::Requested | Cause::CleanupRetry if paused => Lifecycle::Paused,
                    Cause::Requested | Cause::CleanupRetry => Lifecycle::Starting,
                }
            }
            Ok((worker, Err(error))) => {
                if let Cause::Failed { watchdog } = cause {
                    let reason = watchdog
                        .map(|failure| failure.reason)
                        .unwrap_or_else(|| format!("Worker exited; cleanup failed: {error:#}"));
                    self.record_recovery(Some(pid), &reason);
                }
                // Keep the child handle and reserved process-group identity:
                // neither durable Pause nor a retry grants retirement evidence.
                Lifecycle::CleanupFailed {
                    worker,
                    next: Instant::now() + Duration::from_secs(120),
                    error: format!("Worker cleanup failed: {error:#}"),
                }
            }
            Err(error) => {
                // The task dropped the worker, whose owner SIGKILLed the group.
                // Hold off a replacement rather than assume immediate exit.
                let error = format!("Worker retirement task failed: {error}");
                let _ = writeln!(&mut self.log, "{error}");
                let delay = retry_delay(&mut self.failures, None);
                Lifecycle::Backoff {
                    next: Instant::now() + Duration::from_secs(delay),
                    error,
                }
            }
        };
        self.publish(snapshots);
    }
    /// Retire synchronously, finishing any retirement already in progress.
    /// Used at shutdown, where blocking the (exiting) loop is intended.
    pub(crate) async fn retire_now(&mut self, snapshots: &watch::Sender<Published>) -> Result<()> {
        self.poll_retirement(snapshots, true).await;
        if self.lifecycle.worker().is_some() {
            let cause = self.cleanup_cause();
            self.begin_retirement(cause);
            self.publish(snapshots);
            self.poll_retirement(snapshots, true).await;
        }
        if let Lifecycle::CleanupFailed { error, .. } = &self.lifecycle {
            anyhow::bail!("{error}");
        }
        Ok(())
    }
    /// A failed cleanup only skips the SIGTERM wait when the group was already
    /// SIGKILLed; otherwise the worker may still be retiring natively.
    pub(crate) fn cleanup_cause(&self) -> Cause {
        match &self.lifecycle {
            Lifecycle::CleanupFailed { worker, .. } if worker.killed => Cause::CleanupRetry,
            _ => Cause::Requested,
        }
    }
}

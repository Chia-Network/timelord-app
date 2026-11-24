use crate::Worker;
use anyhow::Result;
use std::time::Duration;
use timelord_supervisor::progress::{Failure, FailureKind};
use timelord_supervisor::{NORMAL_GRACEFUL_STOP, WATCHDOG_GRACEFUL_STOP};

pub(crate) type Retirement =
    tokio::task::JoinHandle<(Box<Worker>, Result<std::process::ExitStatus>)>;

/// Why a worker is being retired decides what follows a successful reap and how
/// long the process group gets to honour SIGTERM.
pub(crate) enum Cause {
    /// Pause, Reload, or Resume of a running worker.
    Requested,
    /// The group was already SIGKILLed but did not reap; another SIGTERM wait
    /// cannot help.
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
            Self::CleanupRetry => Duration::ZERO,
            Self::Requested
            | Self::Failed { watchdog: None }
            | Self::Failed { watchdog: Some(_) } => NORMAL_GRACEFUL_STOP,
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

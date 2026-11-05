//! Typed operational observations and their read-only metric/HTTP projections.
mod activity;
mod current;
mod domain;
mod http;
mod metrics;
mod snapshot;

pub use activity::Activity;
use domain::index;
pub use domain::{
    OperatingPointReason, PeakResult, ProofStage, RestartReason, SessionReason, SubmissionKind,
    board, chain, proof_lane, restart_reason,
};
pub use http::{HttpState, routes};
pub use metrics::Metrics;
pub use snapshot::ready;
use snapshot::snapshot;
pub(crate) use snapshot::work_ready;

#[cfg(test)]
mod tests;

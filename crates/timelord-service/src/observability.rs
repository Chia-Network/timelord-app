//! Typed operational observations and their read-only metric/HTTP projections.
mod activity;
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
use snapshot::snapshot;
pub use snapshot::{Readiness, ready};

#[cfg(test)]
mod tests;

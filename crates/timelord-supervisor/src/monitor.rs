//! Read-only deployment endpoint survives worker exit and durable Pause.
use crate::Snapshot;
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
#[derive(Clone)]
struct Monitor {
    state: Snapshot,
    tick: Arc<Mutex<Instant>>,
    network: String,
}
fn ready(status: &timelord_supervisor::Status) -> bool {
    status.mode == timelord_supervisor::Mode::Running
        && status.phase == "worker_started"
        && status.progress_age_seconds.is_some_and(|age| age <= 3)
        && status.progress.as_ref().is_some_and(|p| {
            Some(p.worker_pid) == status.worker_pid
                && p.generation == status.worker_generation
                && p.ready
                && !p.failed
        })
}
async fn health(State(m): State<Monitor>) -> impl IntoResponse {
    let healthy = m.tick.lock().expect("supervisor tick").elapsed() <= Duration::from_secs(30);
    (
        if healthy {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(serde_json::json!({"healthy":healthy})),
    )
}
async fn readiness(State(m): State<Monitor>) -> impl IntoResponse {
    let status = m.state.lock().expect("status").clone();
    let ready = ready(&status)
        && m.tick.lock().expect("supervisor tick").elapsed() <= Duration::from_secs(30);
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(serde_json::json!({"ready":ready,"mode":status.mode,"phase":status.phase})),
    )
}
async fn metrics(State(m): State<Monitor>) -> impl IntoResponse {
    let status = m.state.lock().expect("status").clone();
    let tick_age = m.tick.lock().expect("supervisor tick").elapsed();
    let healthy = tick_age <= Duration::from_secs(30);
    let mut body = String::new();
    for (name, kind, value) in [
        (
            "desired_running",
            "gauge",
            u64::from(status.mode == timelord_supervisor::Mode::Running),
        ),
        ("worker_up", "gauge", u64::from(status.worker_pid.is_some())),
        (
            "worker_ready",
            "gauge",
            u64::from(ready(&status) && healthy),
        ),
        ("worker_restarts_total", "counter", status.restarts),
        (
            "log_dropped_bytes_total",
            "counter",
            status.logging.dropped_bytes,
        ),
        (
            "log_write_errors_total",
            "counter",
            status.logging.write_errors,
        ),
        (
            "sleep_inhibited",
            "gauge",
            u64::from(status.sleep_inhibited),
        ),
        (
            "using_last_good_configuration",
            "gauge",
            u64::from(
                status
                    .progress
                    .as_ref()
                    .is_some_and(|p| p.configuration_fallback),
            ),
        ),
        (
            "configuration_checkpointed",
            "gauge",
            u64::from(
                status
                    .progress
                    .as_ref()
                    .is_some_and(|p| p.configuration_checkpointed),
            ),
        ),
        ("healthy", "gauge", u64::from(healthy)),
    ] {
        body.push_str(&format!("# HELP chia_timelord_supervisor_{name} Supervisor {name}\n# TYPE chia_timelord_supervisor_{name} {kind}\nchia_timelord_supervisor_{name}{{network=\"{}\"}} {value}\n",m.network));
    }
    if let Some(age) = status.progress_age_seconds {
        let age = age.saturating_add(tick_age.as_secs());
        body.push_str(&format!("# HELP chia_timelord_supervisor_progress_age_seconds Seconds since accepted worker report\n# TYPE chia_timelord_supervisor_progress_age_seconds gauge\nchia_timelord_supervisor_progress_age_seconds{{network=\"{}\"}} {age}\n",m.network));
    }
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}
pub(crate) fn routes(state: Snapshot, tick: Arc<Mutex<Instant>>, network: String) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        .route("/metrics", get(metrics))
        .with_state(Monitor {
            state,
            tick,
            network,
        })
}

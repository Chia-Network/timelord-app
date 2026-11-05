//! Read-only deployment endpoint survives worker exit and durable Pause.
use crate::Snapshot;
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use std::time::Instant;
use timelord_supervisor::Network;
#[derive(Clone)]
struct Monitor {
    state: Snapshot,
    network: Network,
}
async fn health(State(m): State<Monitor>) -> impl IntoResponse {
    let healthy = m.state.borrow().healthy(Instant::now());
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
    let published = m.state.borrow().clone();
    let ready = published.ready(Instant::now());
    let status = &published.status;
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
    let published = m.state.borrow().clone();
    let now = Instant::now();
    let status = published.status_at(now);
    let healthy = published.healthy(now);
    let mut body = String::new();
    for (name, kind, value) in [
        (
            "desired_running",
            "gauge",
            u64::from(status.mode == timelord_supervisor::Mode::Running),
        ),
        ("worker_up", "gauge", u64::from(status.worker_pid.is_some())),
        ("worker_ready", "gauge", u64::from(published.ready(now))),
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
        body.push_str(&format!("# HELP chia_timelord_supervisor_{name} Supervisor {name}\n# TYPE chia_timelord_supervisor_{name} {kind}\nchia_timelord_supervisor_{name}{{network=\"{}\"}} {value}\n",m.network.label()));
    }
    if let Some(age) = status.progress_age_seconds {
        body.push_str(&format!("# HELP chia_timelord_supervisor_progress_age_seconds Seconds since accepted worker report\n# TYPE chia_timelord_supervisor_progress_age_seconds gauge\nchia_timelord_supervisor_progress_age_seconds{{network=\"{}\"}} {age}\n",m.network.label()));
    }
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}
pub(crate) fn routes(state: Snapshot, network: Network) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        .route("/metrics", get(metrics))
        .with_state(Monitor { state, network })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Published, SupervisorState};
    use std::time::Duration;
    use timelord_supervisor::{Intent, progress};

    fn publication(progress_age: u64, loop_age: u64) -> Published {
        let now = Instant::now();
        let mut status = SupervisorState::new(
            Ok(Intent::default()),
            timelord_supervisor::logging::Writer::stderr(),
        )
        .snapshot();
        status.worker_pid = Some(123);
        status.worker_generation = 7;
        status.phase = timelord_supervisor::SupervisorPhase::WorkerStarted;
        status.progress_age_seconds = Some(0); // Deliberately stale cached age.
        status.progress = Some(progress::Snapshot {
            worker_pid: 123,
            generation: 7,
            sequence: 1,
            ready: true,
            failed: false,
            configuration_fallback: false,
            configuration_checkpointed: false,
            work: [None; progress::SLOTS],
        });
        Published {
            status,
            published_at: now - Duration::from_secs(loop_age),
            progress_received_at: Some(now - Duration::from_secs(progress_age)),
        }
    }
    #[tokio::test]
    async fn monitor_listener_failure_preserves_the_live_worker_and_controls() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tl-monitor-failure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let helper = dir.join("worker");
        std::fs::write(&helper, "#!/bin/sh\nexec /bin/sleep 60\n").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = dir.join("config.toml");
        let progress = dir.join("progress.sock");
        let log = timelord_supervisor::logging::Writer::stderr();
        let mut state = SupervisorState::new(Ok(Intent::default()), log.clone());
        let worker = crate::Worker::spawn(&helper, &config, &progress, 1, &log).unwrap();
        let pid = worker.process.pid();
        state.lifecycle = crate::Lifecycle::Running(Box::new(worker));
        state.generation = 1;
        let (snapshots, current) = tokio::sync::watch::channel(state.published());
        let mut listener = Some(tokio::spawn(async {
            Err(std::io::Error::other("injected scrape listener failure"))
        }));
        crate::monitoring_exit(&mut listener, &mut state, &snapshots).await;
        assert!(listener.is_none());
        state
            .tick(&helper, &config, &progress, &snapshots)
            .await
            .unwrap();
        assert_eq!(state.snapshot().worker_pid, Some(pid));
        assert_eq!(state.generation, 1);
        assert_eq!(state.intent.mode, timelord_supervisor::Mode::Running);
        assert!(
            current
                .borrow()
                .status
                .error
                .as_deref()
                .unwrap()
                .contains("monitoring")
        );
        let paths = timelord_supervisor::Paths::for_config(&config).unwrap();
        state
            .control(
                &paths,
                &helper,
                timelord_supervisor::Request {
                    command: timelord_supervisor::Control::Pause,
                    expected_revision: None,
                },
                &snapshots,
            )
            .await
            .unwrap();
        assert!(state.snapshot().worker_pid.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn http_and_metric_readiness_use_current_progress_and_loop_age() {
        for (progress_age, loop_age, expected_ready, expected_healthy) in [
            (0, 0, true, true),
            (5, 0, false, true),
            (0, 31, false, false),
            (31, 31, false, false),
        ] {
            let published = publication(progress_age, loop_age);
            assert_eq!(
                published.status_at(Instant::now()).progress_age_seconds,
                Some(progress_age)
            );
            let (_sender, receiver) = tokio::sync::watch::channel(published);
            let monitor = Monitor {
                state: receiver,
                network: Network::Testnet11,
            };
            let response = readiness(State(monitor.clone())).await.into_response();
            assert_eq!(response.status() == StatusCode::OK, expected_ready);
            let response = health(State(monitor.clone())).await.into_response();
            assert_eq!(response.status() == StatusCode::OK, expected_healthy);
            let response = metrics(State(monitor)).await.into_response();
            let bytes = axum::body::to_bytes(response.into_body(), 65536)
                .await
                .unwrap();
            let body = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(body.contains(&format!(
                "chia_timelord_supervisor_worker_ready{{network=\"testnet11\"}} {}",
                u8::from(expected_ready)
            )));
            assert!(body.contains(&format!("chia_timelord_supervisor_progress_age_seconds{{network=\"testnet11\"}} {progress_age}")));
        }
    }
    #[test]
    fn stopping_or_mismatched_generation_never_reuses_cached_readiness() {
        let mut published = publication(0, 0);
        assert!(published.ready(Instant::now()));
        published.status.phase = timelord_supervisor::SupervisorPhase::Stopping;
        assert!(!published.ready(Instant::now()));
        published.status.phase = timelord_supervisor::SupervisorPhase::WorkerStarted;
        published.status.worker_generation += 1;
        assert!(!published.ready(Instant::now()));
    }
}

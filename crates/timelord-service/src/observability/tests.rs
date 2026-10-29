use super::*;
use crate::runtime::{HardwareState, RuntimeState};
use crate::{Shared, runtime::RawState};
use axum::http::header;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};
use timelord_core::{Chain, Config, Network};
use tokio::sync::watch;
use tower::ServiceExt;
fn fixture() -> (Arc<Shared>, Arc<Metrics>, HttpState) {
    let mut config = Config::default();
    config.node.certificate = Some("/fixture/no-io.crt".into());
    config.node.private_key = Some("/fixture/no-io.key".into());
    config.node.ca_certificate = Some("/fixture/no-io-ca.crt".into());
    config.hardware.client_binary = Some("/fixture/no-io-client".into());
    config.hardware.boards=vec![toml::from_str("id='a'\nname='A'\nserial='SERIAL-A'\nclient_port=18110\ndevice_location=1\nprofile='manual'").unwrap()];
    let metrics = Metrics::new(Network::Mainnet).unwrap();
    metrics.register_boards(&config);
    let now = crate::unix_now();
    let mut runtime = RuntimeState {
        metrics: Some(metrics.clone()),
        metric_epoch: metrics.begin_session(),
        scheduler_initialized: true,
        node: "authenticated".into(),
        ..Default::default()
    };
    runtime.boards.insert(
        "a".into(),
        HardwareState {
            generation: 1,
            state: "connected".into(),
            native_initialized: true,
            temperature_c: Some(40.),
            temperature_unix: now,
            frequency_mhz: Some(1500.),
            voltage_v: Some(1.),
            ..Default::default()
        },
    );
    let shared = Arc::new(Shared {
        assignments: config.assignments().unwrap(),
        config,
        clients: Arc::new(Mutex::new(std::collections::BTreeSet::from([
            ("a".into(), 0),
            ("a".into(), 1),
        ]))),
        runtime: Arc::new(Mutex::new(runtime)),
        started_unix: now,
        service_started_unix: now,
    });
    let (_, current) = watch::channel(shared.clone());
    let http = HttpState {
        current,
        metrics: metrics.clone(),
    };
    (shared, metrics, http)
}
fn value(body: &str, name: &str, labels: &[&str]) -> Option<f64> {
    body.lines()
        .find(|line| {
            !line.starts_with('#')
                && (line.starts_with(&format!("{name}{{")) || line.starts_with(&format!("{name} ")))
                && labels.iter().all(|label| line.contains(label))
        })
        .and_then(|line| line.rsplit_once(' '))
        .and_then(|(_, v)| v.parse().ok())
}
async fn response(http: HttpState, path: &str) -> (StatusCode, String, String) {
    let response = routes()
        .with_state(http)
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content = response
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let body = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    (status, content, body)
}
#[tokio::test]
async fn probes_distinguish_internal_failure_dependencies_and_normal_idle() {
    let (shared, metrics, http) = fixture();
    assert_eq!(response(http.clone(), "/healthz").await.0, StatusCode::OK);
    // Two clients can cover CC/RC while ICC has no work. Tuning pause is independent.
    shared.runtime.lock().unwrap().tuning_paused = true;
    assert_eq!(response(http.clone(), "/readyz").await.0, StatusCode::OK);
    shared.runtime.lock().unwrap().pending[2] = 1;
    assert_eq!(
        response(http.clone(), "/readyz").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    shared.runtime.lock().unwrap().pending[2] = 0;
    shared.runtime.lock().unwrap().node = "disconnected".into();
    assert_eq!(response(http.clone(), "/healthz").await.0, StatusCode::OK);
    assert!(
        response(http.clone(), "/readyz")
            .await
            .2
            .contains("waiting_for_node")
    );
    metrics.failed.store(true, Ordering::Relaxed);
    assert_eq!(
        response(http.clone(), "/healthz").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(
        response(http.clone(), "/readyz")
            .await
            .2
            .contains("service_unavailable")
    );
    metrics.failed.store(false, Ordering::Relaxed);
    metrics.stopping.store(true, Ordering::Relaxed);
    assert_eq!(
        response(http, "/healthz").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
#[tokio::test]
async fn readiness_requires_initialization_fresh_managed_telemetry_or_verified_generation() {
    let (mut shared, metrics, mut http) = fixture();
    shared.runtime.lock().unwrap().scheduler_initialized = false;
    assert!(
        response(http.clone(), "/readyz")
            .await
            .2
            .contains("waiting_for_chain_state")
    );
    shared.runtime.lock().unwrap().scheduler_initialized = true;
    shared
        .runtime
        .lock()
        .unwrap()
        .boards
        .get_mut("a")
        .unwrap()
        .temperature_unix = 1;
    assert_eq!(
        response(http.clone(), "/readyz").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    // No filesystem access: a missing/unmanaged native binary changes the readiness source.
    let mut config = shared.config.clone();
    config.hardware.client_binary = None;
    shared = Arc::new(Shared {
        config,
        assignments: shared.assignments.clone(),
        clients: shared.clients.clone(),
        runtime: shared.runtime.clone(),
        started_unix: shared.started_unix,
        service_started_unix: shared.service_started_unix,
    });
    let (_, current) = watch::channel(shared.clone());
    http.current = current;
    assert_eq!(
        response(http.clone(), "/readyz").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    shared
        .runtime
        .lock()
        .unwrap()
        .boards
        .get_mut("a")
        .unwrap()
        .verified_generation = Some(1);
    shared
        .runtime
        .lock()
        .unwrap()
        .boards
        .get_mut("a")
        .unwrap()
        .verified_slots
        .extend([0, 1]);
    assert_eq!(response(http.clone(), "/readyz").await.0, StatusCode::OK);
    // A reused socket slot needs a new proof; a verified predecessor is insufficient.
    shared
        .runtime
        .lock()
        .unwrap()
        .boards
        .get_mut("a")
        .unwrap()
        .verified_slots
        .remove(&1);
    assert_eq!(
        response(http.clone(), "/readyz").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    shared
        .runtime
        .lock()
        .unwrap()
        .boards
        .get_mut("a")
        .unwrap()
        .verified_slots
        .insert(1);
    shared
        .runtime
        .lock()
        .unwrap()
        .boards
        .get_mut("a")
        .unwrap()
        .generation = 2;
    assert_eq!(
        response(http.clone(), "/readyz").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    metrics.reconfiguring.store(true, Ordering::Relaxed);
    assert!(
        response(http.clone(), "/readyz")
            .await
            .2
            .contains("reconfiguring")
    );
    let body = response(http, "/metrics").await.2;
    assert!(!body.contains("chia_timelord_chain_assignment{"));
}
#[test]
fn compatibility_uses_last_completion_from_any_chain_and_drops_old_session_gauges() {
    let (shared, metrics, _) = fixture();
    let epoch = shared.runtime.lock().unwrap().metric_epoch;
    let initial = metrics.render(&shared, crate::unix_now()).unwrap();
    assert_eq!(value(&initial, "chia_timelord_estimated_ips", &[]), None);
    metrics.verified(epoch, Chain::Challenge, 1_000_000.9);
    metrics.verified(epoch, Chain::Reward, 900_000.1);
    metrics.verified(epoch, Chain::InfusedChallenge, 800_000.3);
    for stage in [
        ProofStage::Discriminant,
        ProofStage::ClientWait,
        ProofStage::RequestToVerified,
        ProofStage::Verification,
        ProofStage::BundleWait,
    ] {
        metrics.duration(Chain::Reward, stage, Duration::from_millis(2));
    }
    metrics.submitted(SubmissionKind::Signage, Duration::from_millis(1));
    let body = metrics.render(&shared, crate::unix_now()).unwrap();
    for stage in [
        ProofStage::Discriminant,
        ProofStage::ClientWait,
        ProofStage::RequestToVerified,
        ProofStage::Verification,
        ProofStage::BundleWait,
    ] {
        assert_eq!(
            value(
                &body,
                "chia_timelord_proof_duration_seconds_count",
                &[&format!("stage=\"{}\"", stage.label())]
            ),
            Some(1.)
        );
    }
    assert_eq!(
        value(&body, "chia_timelord_estimated_ips", &[]),
        Some(800_000.3)
    );
    if let Ok(path) = std::env::var("TIMELORD_TEST_EXPOSITION") {
        std::fs::write(path, &body).unwrap();
    }
    assert_eq!(
        value(
            &body,
            "chia_timelord_chain_estimated_ips",
            &["chain=\"challenge\""]
        ),
        Some(1_000_000.9)
    );
    assert_eq!(
        value(
            &body,
            "chia_timelord_proofs_verified_total",
            &["chain=\"reward\""]
        ),
        Some(1.)
    );
    assert!(
        body.lines()
            .filter(|l| !l.starts_with('#') && l.starts_with("chia_"))
            .all(|l| l.contains("network=\"mainnet\""))
    );
    metrics.disconnect(epoch);
    assert_eq!(
        value(
            &metrics.render(&shared, crate::unix_now()).unwrap(),
            "chia_timelord_estimated_ips",
            &[]
        ),
        None
    );
    let next = metrics.begin_session();
    shared.runtime.lock().unwrap().metric_epoch = next;
    metrics.verified(epoch, Chain::Reward, 2_000_000.);
    let body = metrics.render(&shared, crate::unix_now()).unwrap();
    assert_eq!(value(&body, "chia_timelord_estimated_ips", &[]), None);
    assert_eq!(
        value(
            &body,
            "chia_timelord_proofs_verified_total",
            &["chain=\"reward\""]
        ),
        Some(2.)
    );
    metrics.verified(next, Chain::Reward, 990_000.);
    assert_eq!(
        value(
            &metrics.render(&shared, crate::unix_now()).unwrap(),
            "chia_timelord_estimated_ips",
            &[]
        ),
        Some(990_000.)
    );
}
#[test]
fn raw_temperature_and_assignments_are_current_and_use_stable_identity() {
    let (shared, metrics, _) = fixture();
    let now = crate::unix_now();
    shared
        .runtime
        .lock()
        .unwrap()
        .boards
        .get_mut("a")
        .unwrap()
        .raw_sample(1, 1_200_000., RawState::Running, now);
    let body = metrics.render(&shared, now).unwrap();
    assert_eq!(
        value(
            &body,
            "chia_timelord_raw_ips",
            &["board=\"SERIAL-A\"", "engine=\"1\""]
        ),
        Some(1_200_000.)
    );
    assert_eq!(
        value(&body, "chia_timelord_board_frequency_hertz", &[]),
        Some(1_500_000_000.)
    );
    let raw_line = body
        .lines()
        .find(|l| l.starts_with("chia_timelord_raw_ips{"))
        .unwrap();
    assert!(!raw_line.contains("chain="));
    assert_eq!(
        value(
            &metrics.render(&shared, now + 6).unwrap(),
            "chia_timelord_raw_ips",
            &[]
        ),
        None
    );
    assert_eq!(
        value(
            &metrics.render(&shared, now + 31).unwrap(),
            "chia_timelord_board_temperature_celsius",
            &[]
        ),
        None
    );
    shared
        .runtime
        .lock()
        .unwrap()
        .boards
        .get_mut("a")
        .unwrap()
        .state = "reconnecting".into();
    let body = metrics.render(&shared, now).unwrap();
    assert_eq!(value(&body, "chia_timelord_raw_ips", &[]), None);
    assert_eq!(value(&body, "chia_timelord_board_up", &[]), Some(0.));
}
#[test]
fn serial_identity_survives_owner_change_and_obsolete_gauges_disappear() {
    let (shared, metrics, _) = fixture();
    let now = crate::unix_now();
    let mut config = shared.config.clone();
    config.hardware.boards[0].id = "new-owner".into();
    let hardware = shared.runtime.lock().unwrap().boards.remove("a").unwrap();
    shared
        .runtime
        .lock()
        .unwrap()
        .boards
        .insert("new-owner".into(), hardware);
    metrics.register_boards(&config);
    let moved = Shared {
        assignments: config.assignments().unwrap(),
        config,
        clients: Arc::new(Mutex::new(std::collections::BTreeSet::from([
            ("new-owner".into(), 0),
            ("new-owner".into(), 1),
        ]))),
        runtime: shared.runtime.clone(),
        started_unix: now,
        service_started_unix: now,
    };
    let body = metrics.render(&moved, now).unwrap();
    assert_eq!(
        value(&body, "chia_timelord_board_up", &["board=\"SERIAL-A\""]),
        Some(1.)
    );
    assert!(!body.contains("board=\"new-owner\""));
    moved
        .runtime
        .lock()
        .unwrap()
        .boards
        .get_mut("new-owner")
        .unwrap()
        .temperature_c = None;
    let body = metrics.render(&moved, now).unwrap();
    assert_eq!(value(&body, "chia_timelord_ready", &[]), Some(0.));
    assert_eq!(
        value(&body, "chia_timelord_board_temperature_celsius", &[]),
        None
    );
    let removed = Shared {
        config: Config::default(),
        assignments: vec![],
        clients: Default::default(),
        runtime: moved.runtime.clone(),
        started_unix: now,
        service_started_unix: now,
    };
    let body = metrics.render(&removed, now).unwrap();
    assert!(!body.contains("chia_timelord_chain_assignment{"));
    assert!(!body.contains("chia_timelord_board_state{"));
    assert!(body.contains("chia_timelord_asic_recoveries_total{"));
}
#[test]
fn process_totals_survive_history_pruning_and_runtime_replacement() {
    let (shared, metrics, _) = fixture();
    metrics.asic_recovered("SERIAL-A", Some(1));
    metrics.topology_changed();
    metrics.connected();
    metrics.connected();
    metrics.submitted(SubmissionKind::Infusion, Duration::from_millis(2));
    metrics.duration(
        Chain::Reward,
        ProofStage::Verification,
        Duration::from_millis(3),
    );
    metrics.client_started("SERIAL-A", RestartReason::Topology);
    metrics.client_started("SERIAL-A", RestartReason::Usb);
    assert_eq!(
        metrics.counter("client_restarts_total", &["SERIAL-A", "usb"]),
        1
    );
    assert_eq!(
        metrics.counter("asic_recoveries_total", &["SERIAL-A", "1"]),
        1
    );
    let before = metrics.render(&shared, crate::unix_now()).unwrap();
    let mut state = shared.runtime.lock().unwrap();
    *state = RuntimeState {
        metrics: Some(metrics.clone()),
        metric_epoch: metrics.begin_session(),
        ..Default::default()
    };
    drop(state);
    let after = metrics.render(&shared, crate::unix_now()).unwrap();
    for name in [
        "chia_timelord_asic_recoveries_total",
        "chia_timelord_topology_changes_total",
        "chia_timelord_node_reconnects_total",
        "chia_timelord_submissions_total",
    ] {
        assert_eq!(value(&before, name, &[]), value(&after, name, &[]));
    }
    assert_eq!(
        value(
            &after,
            "chia_timelord_proof_duration_seconds_count",
            &["stage=\"verification\""]
        ),
        Some(1.)
    );
    assert_eq!(
        value(&before, "chia_timelord_process_start_time_seconds", &[]),
        value(&after, "chia_timelord_process_start_time_seconds", &[])
    );
    let fresh = Metrics::new(Network::Mainnet)
        .unwrap()
        .render(&shared, crate::unix_now())
        .unwrap();
    assert_eq!(
        value(
            &fresh,
            "chia_timelord_submissions_total",
            &["kind=\"infusion\""]
        ),
        Some(0.)
    );
}
#[test]
fn pending_activity_counts_cancellation_but_not_completed_work() {
    let (shared, metrics, _) = fixture();
    let activity = Activity::new(shared.runtime.clone(), Chain::Challenge);
    assert_eq!(shared.runtime.lock().unwrap().pending[0], 1);
    drop(activity);
    let mut completed = Activity::new(shared.runtime.clone(), Chain::Challenge);
    completed.finish();
    drop(completed);
    assert_eq!(shared.runtime.lock().unwrap().pending[0], 0);
    let body = metrics.render(&shared, crate::unix_now()).unwrap();
    assert_eq!(
        value(
            &body,
            "chia_timelord_work_cancelled_total",
            &["chain=\"challenge\""]
        ),
        Some(1.)
    );
    assert_eq!(
        value(
            &body,
            "chia_timelord_proof_duration_seconds_count",
            &["stage=\"request_to_verified\""]
        ),
        Some(1.)
    );
}
#[tokio::test]
async fn exposition_isolated_routes_and_concurrent_scrapes_are_consistent() {
    let (shared, metrics, http) = fixture();
    let epoch = shared.runtime.lock().unwrap().metric_epoch;
    metrics.verified(epoch, Chain::Challenge, 999_999.9);
    let producing = tokio::spawn({
        let metrics = metrics.clone();
        let runtime = shared.runtime.clone();
        async move {
            let mut activity = Activity::new(runtime, Chain::Challenge);
            for _ in 0..50 {
                metrics.verified(epoch, Chain::Challenge, 999_999.9);
                tokio::task::yield_now().await;
            }
            activity.finish();
        }
    });
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let http = http.clone();
        requests.spawn(async move { response(http, "/metrics").await });
    }
    while let Some(result) = requests.join_next().await {
        let (status, content, body) = result.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert!(content.starts_with("text/plain; version=0.0.4"));
        assert_eq!(
            value(&body, "chia_timelord_estimated_ips", &[]),
            Some(999_999.9)
        );
    }
    producing.await.unwrap();
    assert_eq!(metrics.counter("proofs_verified_total", &["challenge"]), 51);
    let status = routes()
        .with_state(http)
        .oneshot(
            Request::builder()
                .uri("/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::NOT_FOUND);
}
#[test]
fn labels_are_bounded_and_exclude_raw_failure_messages() {
    for (text, reason) in [
        ("temperature_limit", RestartReason::Thermal),
        ("initial_readback voltage=0.1", RestartReason::Readback),
        ("temperature telemetry unavailable", RestartReason::Sensor),
        ("Invalid verification hash 123", RestartReason::Verification),
        ("FTDI device 999 not found", RestartReason::Usb),
        ("arbitrary /secret/path token=abc", RestartReason::Process),
    ] {
        assert_eq!(restart_reason(text), reason);
    }
}

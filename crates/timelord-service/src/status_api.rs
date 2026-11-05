//! Versioned HTTP snapshots of the current topology; never owns service lifecycle.
use crate::{automatic, history_store, observability, runtime, unix_now};
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use timelord_core::{Chain, Network};

pub fn router(http: observability::HttpState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/status", get(status))
        .route("/v3/history", get(history))
        .merge(observability::routes())
        .with_state(http)
}

#[derive(Serialize)]
struct Status {
    api_version: u32,
    service_pid: u32,
    service_state: &'static str,
    timelord_state: String,
    message: String,
    production_enabled: bool,
    started_unix: u64,
    service_started_unix: u64,
    timestamp_unix: u64,
    full_node: NodeStatus,
    boards: Vec<BoardStatus>,
    connected_client_count: usize,
    latest_proof_unix: Option<u64>,
    delivered_ips: Option<f64>,
    proofs_delivered: u64,
    failed_proofs: u64,
    tuning_paused: bool,
}
#[derive(Serialize)]
struct NodeStatus {
    host: String,
    port: u16,
    network: Network,
    state: String,
}
#[derive(Serialize)]
struct BoardStatus {
    raw_speeds: Vec<runtime::RawSpeed>,
    id: String,
    name: String,
    client_port: u16,
    assigned_chains: Vec<Chain>,
    connected_clients: usize,
    hardware_state: String,
    temperature_c: Option<f64>,
    temperature_limit_c: f64,
    frequency_mhz: Option<f64>,
    voltage_v: Option<f64>,
    profile: &'static str,
    device_location: Option<u32>,
    serial: Option<String>,
    recoveries: u64,
    fault_kind: Option<crate::hardware::NativeFailureKind>,
    automatic: Option<automatic::Status>,
    latest_proof_unix: Option<u64>,
}

async fn health(State(http): State<observability::HttpState>) -> Json<serde_json::Value> {
    let shared = http.current.borrow().clone();
    Json(
        serde_json::json!({"service":"timelord-service","api_version":1,"production_enabled":shared.config.node.certificate.is_some()}),
    )
}

async fn status(State(http): State<observability::HttpState>) -> Json<Status> {
    let shared = http.current.borrow().clone();
    let clients = shared.clients.lock().expect("client state lock");
    let runtime = shared.runtime.lock().expect("runtime");
    Json(Status {
        api_version: 1,
        service_pid: std::process::id(),
        service_state: "ready",
        timelord_state: runtime.phase.clone(),
        message: runtime.message.clone(),
        production_enabled: shared.config.node.certificate.is_some(),
        started_unix: shared.started_unix,
        service_started_unix: shared.service_started_unix,
        timestamp_unix: unix_now(),
        full_node: NodeStatus {
            host: shared.config.node.host.clone(),
            port: shared.config.node.port,
            network: shared.config.node.network,
            state: if shared.config.node.certificate.is_none() {
                "not_connected".into()
            } else {
                runtime.node.clone()
            },
        },
        boards: shared
            .config
            .hardware
            .boards
            .iter()
            .map(|b| BoardStatus {
                id: b.id.clone(),
                name: b.name.clone(),
                client_port: b.client_port,
                assigned_chains: shared
                    .assignments
                    .iter()
                    .filter(|a| a.board_id == b.id)
                    .map(|a| a.chain)
                    .collect(),
                connected_clients: clients.iter().filter(|(id, _)| id == &b.id).count(),
                hardware_state: runtime
                    .boards
                    .get(&b.id)
                    .map(|b| b.state.clone())
                    .unwrap_or_else(|| "not_inspected".into()),
                raw_speeds: runtime
                    .boards
                    .get(&b.id)
                    .map(|b| b.raw_speeds.clone())
                    .unwrap_or_default(),
                temperature_c: runtime.boards.get(&b.id).and_then(|b| b.temperature_c),
                temperature_limit_c: b.temperature_limit_c,
                frequency_mhz: runtime.boards.get(&b.id).and_then(|b| b.frequency_mhz),
                voltage_v: runtime.boards.get(&b.id).and_then(|b| b.voltage_v),
                profile: b.profile_name(),
                device_location: runtime
                    .boards
                    .get(&b.id)
                    .and_then(|h| h.device_location)
                    .or(b.device_location),
                serial: b.serial.clone(),
                recoveries: runtime.boards.get(&b.id).map(|b| b.recoveries).unwrap_or(0),
                fault_kind: runtime.boards.get(&b.id).and_then(|b| b.fault_kind),
                automatic: runtime.boards.get(&b.id).and_then(|b| b.automatic.clone()),
                latest_proof_unix: runtime
                    .observations
                    .iter()
                    .rev()
                    .find(|o| {
                        o.work_started_unix
                            >= runtime
                                .boards
                                .get(&b.id)
                                .map(|h| h.started_unix)
                                .unwrap_or(0)
                            && shared
                                .assignments
                                .iter()
                                .any(|a| a.board_id == b.id && a.chain == o.chain)
                    })
                    .map(|o| o.timestamp_unix),
            })
            .collect(),
        connected_client_count: clients.len(),
        latest_proof_unix: runtime.observations.last().map(|o| o.timestamp_unix),
        delivered_ips: runtime
            .observations
            .iter()
            .rev()
            .find(|o| o.chain == Chain::Challenge && o.timestamp_unix + 90 >= unix_now())
            .map(|o| o.ips),
        proofs_delivered: runtime.proofs_delivered,
        failed_proofs: runtime.failed_proofs,
        tuning_paused: runtime.tuning_paused,
    })
}

async fn history(State(http): State<observability::HttpState>) -> Json<serde_json::Value> {
    let shared = http.current.borrow().clone();
    let scope =
        history_store::scope(&shared.config, &shared.assignments).expect("validated history scope");
    let saved = {
        let runtime = shared.runtime.lock().expect("runtime");
        history_store::Snapshot::capture(scope, &runtime, shared.started_unix, unix_now())
    };
    let saved = saved.pruned(unix_now());
    // Formatting a day of chart records must not hold up verified-proof updates.
    Json(
        serde_json::json!({"api_version":3,"service_pid":std::process::id(),"timestamp_unix":unix_now(),"capture_intervals":saved.capture_intervals,"estimated_observations":saved.estimated_observations,"events":saved.events}),
    )
}

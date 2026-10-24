//! Process-owned counters and read-only, bounded snapshots of the current runtime.
use crate::{Shared, runtime::RawState};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use prometheus::{
    Encoder, GaugeVec, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry,
    TextEncoder, core::Collector,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use timelord_core::{Chain, Config, Network};
use tokio::sync::watch;

pub fn chain(c: Chain) -> &'static str {
    match c {
        Chain::Challenge => "challenge",
        Chain::Reward => "reward",
        Chain::InfusedChallenge => "infused_challenge",
    }
}
fn index(c: Chain) -> usize {
    match c {
        Chain::Challenge => 0,
        Chain::Reward => 1,
        Chain::InfusedChallenge => 2,
    }
}
pub fn board(config: &Config, id: &str) -> String {
    config
        .hardware
        .boards
        .iter()
        .find(|b| b.id == id)
        .map(|b| b.serial.as_deref().unwrap_or(&b.id).to_owned())
        .unwrap_or_else(|| id.to_owned())
}
pub fn restart_reason(message: &str) -> &'static str {
    let message = message.to_ascii_lowercase();
    if message.contains("temperature_limit")
        || message.contains("temperature limit")
        || message.contains("temperature_guard")
    {
        "thermal"
    } else if message.contains("readback") {
        "readback"
    } else if message.contains("temperature") {
        "sensor"
    } else if message.contains("verif") {
        "verification"
    } else if ["usb", "ftdi", "device", "disconnected"]
        .iter()
        .any(|s| message.contains(s))
    {
        "usb"
    } else {
        "process"
    }
}
#[derive(Default)]
struct Last {
    epoch: u64,
    ips: Option<f64>,
    chains: [Option<f64>; 3],
}
pub struct Metrics {
    registry: Registry,
    counters: BTreeMap<&'static str, IntCounterVec>,
    timestamps: BTreeMap<&'static str, IntGaugeVec>,
    proof_duration: HistogramVec,
    submission_duration: HistogramVec,
    last: Mutex<Last>,
    ever_connected: AtomicBool,
    clients_started: Mutex<BTreeSet<String>>,
    pub failed: AtomicBool,
    pub stopping: AtomicBool,
    pub reconfiguring: AtomicBool,
}
impl Metrics {
    pub fn new(network: Network) -> Result<Arc<Self>> {
        let network = match network {
            Network::Mainnet => "mainnet",
            Network::Testnet11 => "testnet11",
        };
        let registry = Registry::new_custom(
            Some("chia_timelord".into()),
            Some(HashMap::from([("network".into(), network.into())])),
        )?;
        let mut counters = BTreeMap::new();
        for (name, labels) in [
            ("proofs_verified_total", vec!["chain"]),
            ("proof_verification_failures_total", vec!["chain"]),
            ("submissions_total", vec!["kind"]),
            ("topology_changes_total", vec![]),
            ("asic_recoveries_total", vec!["board", "engine"]),
            ("client_restarts_total", vec!["board", "reason"]),
            ("thermal_stops_total", vec!["board"]),
            ("node_reconnects_total", vec![]),
            ("peak_updates_total", vec!["result"]),
            ("work_cancelled_total", vec!["chain"]),
            ("vdf_sessions_started_total", vec!["chain", "reason"]),
            ("operating_point_changes_total", vec!["board", "reason"]),
        ] {
            let metric = IntCounterVec::new(Opts::new(name, name.replace('_', " ")), &labels)?;
            registry.register(Box::new(metric.clone()))?;
            if labels.is_empty() {
                metric.with_label_values(&[]);
            }
            counters.insert(name, metric);
        }
        let mut timestamps = BTreeMap::new();
        for (name, labels) in [
            ("last_verified_proof_timestamp_seconds", vec!["chain"]),
            ("last_submission_timestamp_seconds", vec!["kind"]),
            ("last_node_message_timestamp_seconds", vec![]),
            ("last_peak_received_timestamp_seconds", vec![]),
        ] {
            let metric = IntGaugeVec::new(Opts::new(name, name.replace('_', " ")), &labels)?;
            registry.register(Box::new(metric.clone()))?;
            timestamps.insert(name, metric);
        }
        for name in ["version", "build_info"] {
            let labels = if name == "version" {
                vec!["version"]
            } else {
                vec!["version", "revision"]
            };
            let metric =
                IntGaugeVec::new(Opts::new(name, "Running timelord-service build"), &labels)?;
            let mut values = vec![env!("CARGO_PKG_VERSION")];
            if name == "build_info" {
                values.push(option_env!("TIMELORD_BUILD_REVISION").unwrap_or("unknown"));
            }
            metric.with_label_values(&values).set(1);
            registry.register(Box::new(metric))?;
        }
        let started =
            prometheus::IntGauge::new("process_start_time_seconds", "Service process start time")?;
        started.set(crate::unix_now() as i64);
        registry.register(Box::new(started))?;
        let buckets = vec![
            0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1., 5., 15., 60., 300.,
        ];
        let proof_duration = HistogramVec::new(
            HistogramOpts::new(
                "proof_duration_seconds",
                "Observed proof pipeline stage duration; stages can overlap",
            )
            .buckets(buckets.clone()),
            &["chain", "stage"],
        )?;
        let submission_duration = HistogramVec::new(
            HistogramOpts::new(
                "submission_duration_seconds",
                "Time to write a verified bundle to the node",
            )
            .buckets(buckets),
            &["kind"],
        )?;
        registry.register(Box::new(proof_duration.clone()))?;
        registry.register(Box::new(submission_duration.clone()))?;
        let metrics = Arc::new(Self {
            registry,
            counters,
            timestamps,
            proof_duration,
            submission_duration,
            last: Mutex::new(Last::default()),
            ever_connected: AtomicBool::new(false),
            clients_started: Mutex::new(BTreeSet::new()),
            failed: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            reconfiguring: AtomicBool::new(false),
        });
        for c in Chain::ALL {
            for name in [
                "proofs_verified_total",
                "proof_verification_failures_total",
                "work_cancelled_total",
            ] {
                metrics.counters[name].with_label_values(&[chain(c)]);
            }
            for reason in ["initial", "input_change", "reconnect"] {
                metrics.counters["vdf_sessions_started_total"]
                    .with_label_values(&[chain(c), reason]);
            }
        }
        for result in ["accepted", "ignored", "rejected"] {
            metrics.counters["peak_updates_total"].with_label_values(&[result]);
        }
        for kind in ["signage", "infusion", "end_of_sub_slot"] {
            metrics.counters["submissions_total"].with_label_values(&[kind]);
        }
        Ok(metrics)
    }
    #[cfg(test)]
    pub fn counter(&self, name: &'static str, labels: &[&str]) -> u64 {
        self.counters[name].with_label_values(labels).get()
    }
    pub fn inc(&self, name: &'static str, labels: &[&str]) {
        self.counters[name].with_label_values(labels).inc();
    }
    pub fn stamp(&self, name: &'static str, labels: &[&str]) {
        self.timestamps[name]
            .with_label_values(labels)
            .set(crate::unix_now() as i64);
    }
    pub fn begin_session(&self) -> u64 {
        let mut last = self.last.lock().expect("metrics");
        last.epoch += 1;
        last.ips = None;
        last.chains = [None; 3];
        last.epoch
    }
    pub fn disconnect(&self, epoch: u64) {
        let mut last = self.last.lock().expect("metrics");
        if last.epoch == epoch {
            last.ips = None;
            last.chains = [None; 3];
        }
    }
    pub fn connected(&self) {
        if self.ever_connected.swap(true, Ordering::Relaxed) {
            self.inc("node_reconnects_total", &[]);
        }
    }
    pub fn verified(&self, epoch: u64, c: Chain, ips: f64) {
        self.inc("proofs_verified_total", &[chain(c)]);
        self.stamp("last_verified_proof_timestamp_seconds", &[chain(c)]);
        let mut last = self.last.lock().expect("metrics");
        if last.epoch == epoch {
            last.ips = Some(ips);
            last.chains[index(c)] = Some(ips);
        }
    }
    pub fn duration(&self, c: Chain, stage: &str, elapsed: Duration) {
        self.proof_duration
            .with_label_values(&[chain(c), stage])
            .observe(elapsed.as_secs_f64());
    }
    pub fn submitted(&self, kind: &str, elapsed: Duration) {
        self.inc("submissions_total", &[kind]);
        self.stamp("last_submission_timestamp_seconds", &[kind]);
        self.submission_duration
            .with_label_values(&[kind])
            .observe(elapsed.as_secs_f64());
    }
    pub fn client_started(&self, board: &str, reason: &str) {
        if !self
            .clients_started
            .lock()
            .expect("metrics")
            .insert(board.to_owned())
        {
            self.inc("client_restarts_total", &[board, reason]);
        }
    }
    pub fn register_boards(&self, config: &Config) {
        for b in &config.hardware.boards {
            let label = board(config, &b.id);
            self.counters["thermal_stops_total"].with_label_values(&[&label]);
            for engine in ["0", "1", "2", "unknown"] {
                self.counters["asic_recoveries_total"].with_label_values(&[&label, engine]);
            }
            for reason in [
                "thermal",
                "readback",
                "sensor",
                "verification",
                "usb",
                "process",
                "topology",
            ] {
                self.counters["client_restarts_total"].with_label_values(&[&label, reason]);
            }
            for reason in ["trial", "backoff", "restore"] {
                self.counters["operating_point_changes_total"].with_label_values(&[&label, reason]);
            }
        }
    }
    pub fn healthy(&self) -> bool {
        !self.failed.load(Ordering::Relaxed) && !self.stopping.load(Ordering::Relaxed)
    }
    pub fn render(&self, shared: &Shared, now: u64) -> Result<String> {
        let (mut samples, readiness, epoch) = snapshot(shared, now);
        let network = match shared.config.node.network {
            Network::Mainnet => "mainnet",
            Network::Testnet11 => "testnet11",
        };
        let last = self.last.lock().expect("metrics");
        if last.epoch == epoch && !self.reconfiguring.load(Ordering::Relaxed) {
            if let Some(ips) = last.ips {
                samples.add("estimated_ips", &[], &[], ips);
            }
            for c in Chain::ALL {
                if let Some(ips) = last.chains[index(c)] {
                    samples.add("chain_estimated_ips", &["chain"], &[chain(c)], ips);
                }
            }
        }
        drop(last);
        let ready =
            readiness.ready && self.healthy() && !self.reconfiguring.load(Ordering::Relaxed);
        samples.add("ready", &[], &[], f64::from(ready));
        samples.add("healthy", &[], &[], f64::from(self.healthy()));
        let mut families = self.registry.gather();
        for (name, group) in samples.0 {
            let mut keys = vec!["network"];
            keys.extend(group.keys.iter().copied());
            let metric = GaugeVec::new(
                Opts::new(format!("chia_timelord_{name}"), name.replace('_', " ")),
                &keys,
            )?;
            for (labels, value) in group.values {
                let labels = vec![network.to_owned()]
                    .into_iter()
                    .chain(labels)
                    .collect::<Vec<_>>();
                metric
                    .with_label_values(&labels.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                    .set(value);
            }
            families.extend(metric.collect());
        }
        let mut encoded = Vec::new();
        TextEncoder::new().encode(&families, &mut encoded)?;
        Ok(String::from_utf8(encoded)?)
    }
}
#[derive(Default)]
struct Samples(BTreeMap<&'static str, Group>);
struct Group {
    keys: Vec<&'static str>,
    values: Vec<(Vec<String>, f64)>,
}
impl Samples {
    fn add(&mut self, name: &'static str, keys: &[&'static str], labels: &[&str], value: f64) {
        self.0
            .entry(name)
            .or_insert_with(|| Group {
                keys: keys.to_vec(),
                values: vec![],
            })
            .values
            .push((labels.iter().map(|s| (*s).to_owned()).collect(), value));
    }
}
#[derive(Serialize)]
pub struct Readiness {
    pub ready: bool,
    pub reason: &'static str,
}
fn board_usable(h: &crate::runtime::HardwareState, managed: bool, now: u64) -> bool {
    !h.latched_fault
        && if managed {
            h.state == "connected"
                && h.native_initialized
                && h.temperature_c.is_some_and(f64::is_finite)
                && h.temperature_unix <= now
                && now - h.temperature_unix <= 30
        } else {
            h.verified_generation == Some(h.generation)
        }
}
fn snapshot(shared: &Shared, now: u64) -> (Samples, Readiness, u64) {
    // Same lock order as /v1/status; no chart history is traversed or copied.
    let clients = shared.clients.lock().expect("clients");
    let state = shared.runtime.lock().expect("runtime");
    let mut samples = Samples::default();
    if state.metrics.as_ref().is_some_and(|m| {
        m.reconfiguring.load(Ordering::Relaxed) || m.stopping.load(Ordering::Relaxed)
    }) {
        samples.add("node_connected", &[], &[], 0.);
        return (
            samples,
            Readiness {
                ready: false,
                reason: "reconfiguring",
            },
            0,
        );
    }
    samples.add(
        "clients_configured",
        &[],
        &[],
        shared.assignments.len() as f64,
    );
    samples.add("clients_connected", &[], &[], clients.len() as f64);
    let connected = ["authenticated", "ready"].contains(&state.node.as_str());
    samples.add("node_connected", &[], &[], f64::from(connected));
    samples.add("tuning_paused", &[], &[], f64::from(state.tuning_paused));
    if let Some(height) = state.scheduler_height {
        samples.add("scheduler_peak_height", &[], &[], height as f64);
    }
    if let Some(deficit) = state.scheduler_deficit {
        samples.add("deficit", &[], &[], f64::from(deficit));
    }
    let mut required = vec![Chain::Challenge, Chain::Reward];
    for c in Chain::ALL {
        let pending = state.pending[index(c)] > 0;
        samples.add("work_pending", &["chain"], &[chain(c)], f64::from(pending));
        if pending && !required.contains(&c) {
            required.push(c);
        }
    }
    for c in Chain::ALL {
        let available = shared
            .assignments
            .iter()
            .find(|a| a.chain == c)
            .is_some_and(|a| {
                let sockets = clients
                    .iter()
                    .filter(|(id, slot)| {
                        id == &a.board_id
                            && (shared.config.hardware.client_binary.is_some()
                                || state
                                    .boards
                                    .get(id)
                                    .is_some_and(|h| h.verified_slots.contains(slot)))
                    })
                    .count();
                let needed = shared
                    .assignments
                    .iter()
                    .filter(|other| other.board_id == a.board_id && required.contains(&other.chain))
                    .count();
                state.boards.get(&a.board_id).is_some_and(|h| {
                    board_usable(h, shared.config.hardware.client_binary.is_some(), now)
                }) && sockets >= needed.max(1)
            });
        samples.add(
            "chain_required",
            &["chain"],
            &[chain(c)],
            f64::from(required.contains(&c)),
        );
        samples.add(
            "chain_available",
            &["chain"],
            &[chain(c)],
            f64::from(available),
        );
    }
    let mut usable = true;
    for b in &shared.config.hardware.boards {
        let label = b.serial.as_deref().unwrap_or(&b.id);
        let hardware = state.boards.get(&b.id);
        let needed = shared
            .assignments
            .iter()
            .filter(|a| a.board_id == b.id && required.contains(&a.chain))
            .count();
        let sockets = clients
            .iter()
            .filter(|(id, slot)| {
                id == &b.id
                    && (shared.config.hardware.client_binary.is_some()
                        || hardware.is_some_and(|h| h.verified_slots.contains(slot)))
            })
            .count();
        let managed = shared.config.hardware.client_binary.is_some();
        let up =
            hardware.is_some_and(|h| board_usable(h, managed, now)) && (managed || sockets > 0);
        if needed > 0 && (!up || sockets < needed) {
            usable = false;
        }
        samples.add("board_up", &["board"], &[label], f64::from(up));
        let state_name = hardware
            .map(|h| h.state.as_str())
            .unwrap_or("not_inspected");
        let state_name = if [
            "starting",
            "connected",
            "reconnecting",
            "fault",
            "thermal_stop",
            "not_inspected",
        ]
        .contains(&state_name)
        {
            state_name
        } else {
            "other"
        };
        samples.add("board_state", &["board", "state"], &[label, state_name], 1.);
        for assignment in shared.assignments.iter().filter(|a| a.board_id == b.id) {
            samples.add(
                "chain_assignment",
                &["board", "chain"],
                &[label, chain(assignment.chain)],
                1.,
            );
        }
        if let Some(h) = hardware {
            if h.state == "connected" && !h.latched_fault {
                if h.temperature_unix <= now && now - h.temperature_unix <= 30 {
                    if let Some(value) = h.temperature_c.filter(|v| v.is_finite()) {
                        samples.add("board_temperature_celsius", &["board"], &[label], value);
                    }
                    if let Some(value) = h.frequency_mhz {
                        samples.add(
                            "board_frequency_hertz",
                            &["board"],
                            &[label],
                            value * 1_000_000.,
                        );
                    }
                    if let Some(value) = h.voltage_v {
                        samples.add("board_voltage_volts", &["board"], &[label], value);
                    }
                }
                for raw in &h.raw_speeds {
                    if raw.state == RawState::Running
                        && raw.ips.is_finite()
                        && raw.ips >= 0.
                        && raw.timestamp_unix <= now
                        && now - raw.timestamp_unix < 6
                    {
                        let engine = raw.engine.to_string();
                        samples.add("raw_ips", &["board", "engine"], &[label, &engine], raw.ips);
                        samples.add(
                            "raw_sample_timestamp_seconds",
                            &["board", "engine"],
                            &[label, &engine],
                            raw.timestamp_unix as f64,
                        );
                    }
                }
            }
            if let Some(auto) = &h.automatic {
                let name = if [
                    "learning",
                    "testing",
                    "cooldown",
                    "monitoring",
                    "paused",
                    "waiting",
                ]
                .contains(&auto.state.as_str())
                {
                    auto.state.as_str()
                } else {
                    "other"
                };
                samples.add("tuning_state", &["board", "state"], &[label, name], 1.);
            } else if b.profile == timelord_core::ProfileMode::Manual {
                samples.add("tuning_state", &["board", "state"], &[label, "manual"], 1.);
            }
        }
    }
    let reason = if shared.config.node.certificate.is_none() {
        "setup_required"
    } else if shared.assignments.is_empty() {
        "waiting_for_clients"
    } else if !connected {
        "waiting_for_node"
    } else if !state.scheduler_initialized {
        "waiting_for_chain_state"
    } else if !usable {
        "waiting_for_clients"
    } else {
        "ready"
    };
    (
        samples,
        Readiness {
            ready: reason == "ready",
            reason,
        },
        state.metric_epoch,
    )
}
#[derive(Clone)]
pub struct HttpState {
    pub current: watch::Receiver<Arc<Shared>>,
    pub metrics: Arc<Metrics>,
}
pub fn routes() -> Router<HttpState> {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
}
async fn metrics(State(state): State<HttpState>) -> impl IntoResponse {
    let shared = state.current.borrow().clone();
    match state.metrics.render(&shared, crate::unix_now()) {
        Ok(body) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            body,
        ),
        Err(error) => {
            tracing::error!(%error,"Metrics encoding failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "Metrics unavailable\n".into(),
            )
        }
    }
}
async fn healthz(State(state): State<HttpState>) -> impl IntoResponse {
    if state.metrics.healthy() {
        (StatusCode::OK, "Ok\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "Unavailable\n")
    }
}
async fn readyz(State(state): State<HttpState>) -> impl IntoResponse {
    let shared = state.current.borrow().clone();
    let (_, mut ready, _) = snapshot(&shared, crate::unix_now());
    if !state.metrics.healthy() {
        ready = Readiness {
            ready: false,
            reason: "service_unavailable",
        };
    } else if state.metrics.reconfiguring.load(Ordering::Relaxed) {
        ready = Readiness {
            ready: false,
            reason: "reconfiguring",
        };
    }
    (
        if ready.ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(ready),
    )
}
/// A request remains pending while its board recovers. Dropping it records cancellation.
pub struct Activity {
    runtime: Arc<Mutex<crate::runtime::RuntimeState>>,
    chain: Chain,
    started: Instant,
    finished: bool,
}
impl Activity {
    pub fn new(runtime: Arc<Mutex<crate::runtime::RuntimeState>>, c: Chain) -> Self {
        runtime.lock().expect("runtime").pending[index(c)] += 1;
        Self {
            runtime,
            chain: c,
            started: Instant::now(),
            finished: false,
        }
    }
    pub fn finish(&mut self) {
        self.finished = true;
        if let Some(metrics) = &self.runtime.lock().expect("runtime").metrics {
            metrics.duration(self.chain, "request_to_verified", self.started.elapsed());
        }
    }
}
impl Drop for Activity {
    fn drop(&mut self) {
        let mut state = self.runtime.lock().expect("runtime");
        state.pending[index(self.chain)] = state.pending[index(self.chain)].saturating_sub(1);
        if !self.finished
            && let Some(metrics) = &state.metrics
        {
            metrics.inc("work_cancelled_total", &[chain(self.chain)]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{HardwareState, RuntimeState};
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
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
                    && (line.starts_with(&format!("{name}{{"))
                        || line.starts_with(&format!("{name} ")))
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
            "discriminant",
            "client_wait",
            "request_to_verified",
            "verification",
            "bundle_wait",
        ] {
            metrics.duration(Chain::Reward, stage, Duration::from_millis(2));
        }
        metrics.submitted("signage", Duration::from_millis(1));
        let body = metrics.render(&shared, crate::unix_now()).unwrap();
        for stage in [
            "discriminant",
            "client_wait",
            "request_to_verified",
            "verification",
            "bundle_wait",
        ] {
            assert_eq!(
                value(
                    &body,
                    "chia_timelord_proof_duration_seconds_count",
                    &[&format!("stage=\"{stage}\"")]
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
        };
        let body = metrics.render(&removed, now).unwrap();
        assert!(!body.contains("chia_timelord_chain_assignment{"));
        assert!(!body.contains("chia_timelord_board_state{"));
        assert!(body.contains("chia_timelord_asic_recoveries_total{"));
    }
    #[test]
    fn process_totals_survive_history_pruning_and_runtime_replacement() {
        let (shared, metrics, _) = fixture();
        metrics.inc("asic_recoveries_total", &["SERIAL-A", "1"]);
        metrics.inc("topology_changes_total", &[]);
        metrics.connected();
        metrics.connected();
        metrics.submitted("infusion", Duration::from_millis(2));
        metrics.duration(Chain::Reward, "verification", Duration::from_millis(3));
        metrics.client_started("SERIAL-A", "topology");
        metrics.client_started("SERIAL-A", "usb");
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
            ("temperature_limit", "thermal"),
            ("initial_readback voltage=0.1", "readback"),
            ("temperature telemetry unavailable", "sensor"),
            ("Invalid verification hash 123", "verification"),
            ("FTDI device 999 not found", "usb"),
            ("arbitrary /secret/path token=abc", "process"),
        ] {
            assert_eq!(restart_reason(text), reason);
        }
    }
}

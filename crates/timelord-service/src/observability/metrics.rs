//! Process-owned metric handles and Prometheus encoding.
use super::{
    OperatingPointReason, PeakResult, ProofStage, RestartReason, SessionReason, SubmissionKind,
    board, chain, index, snapshot,
};
use crate::Shared;
use anyhow::Result;
use prometheus::{
    Encoder, GaugeVec, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry,
    TextEncoder, core::Collector,
};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use timelord_core::{Chain, Config, Network};

#[derive(Default)]
struct Last {
    epoch: u64,
    ips: Option<f64>,
    chains: [Option<f64>; 3],
}
pub struct Metrics {
    registry: Registry,
    proofs_verified_total: IntCounterVec,
    proof_verification_failures_total: IntCounterVec,
    submissions_total: IntCounterVec,
    topology_changes_total: IntCounterVec,
    asic_recoveries_total: IntCounterVec,
    client_restarts_total: IntCounterVec,
    thermal_stops_total: IntCounterVec,
    node_reconnects_total: IntCounterVec,
    peak_updates_total: IntCounterVec,
    work_cancelled_total: IntCounterVec,
    vdf_sessions_started_total: IntCounterVec,
    operating_point_changes_total: IntCounterVec,
    last_verified_proof_timestamp_seconds: IntGaugeVec,
    last_submission_timestamp_seconds: IntGaugeVec,
    last_node_message_timestamp_seconds: IntGaugeVec,
    last_peak_received_timestamp_seconds: IntGaugeVec,
    proof_duration: HistogramVec,
    submission_duration: HistogramVec,
    last: Mutex<Last>,
    ever_connected: AtomicBool,
    clients_started: Mutex<BTreeSet<String>>,
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
        let proofs_verified_total = IntCounterVec::new(
            Opts::new("proofs_verified_total", "proofs verified total"),
            &["chain"],
        )?;
        registry.register(Box::new(proofs_verified_total.clone()))?;
        let proof_verification_failures_total = IntCounterVec::new(
            Opts::new(
                "proof_verification_failures_total",
                "proof verification failures total",
            ),
            &["chain"],
        )?;
        registry.register(Box::new(proof_verification_failures_total.clone()))?;
        let submissions_total = IntCounterVec::new(
            Opts::new("submissions_total", "submissions total"),
            &["kind"],
        )?;
        registry.register(Box::new(submissions_total.clone()))?;
        let topology_changes_total = IntCounterVec::new(
            Opts::new("topology_changes_total", "topology changes total"),
            &[],
        )?;
        registry.register(Box::new(topology_changes_total.clone()))?;
        topology_changes_total.with_label_values(&[]);
        let asic_recoveries_total = IntCounterVec::new(
            Opts::new("asic_recoveries_total", "asic recoveries total"),
            &["board", "engine"],
        )?;
        registry.register(Box::new(asic_recoveries_total.clone()))?;
        let client_restarts_total = IntCounterVec::new(
            Opts::new("client_restarts_total", "client restarts total"),
            &["board", "reason"],
        )?;
        registry.register(Box::new(client_restarts_total.clone()))?;
        let thermal_stops_total = IntCounterVec::new(
            Opts::new("thermal_stops_total", "thermal stops total"),
            &["board"],
        )?;
        registry.register(Box::new(thermal_stops_total.clone()))?;
        let node_reconnects_total = IntCounterVec::new(
            Opts::new("node_reconnects_total", "node reconnects total"),
            &[],
        )?;
        registry.register(Box::new(node_reconnects_total.clone()))?;
        node_reconnects_total.with_label_values(&[]);
        let peak_updates_total = IntCounterVec::new(
            Opts::new("peak_updates_total", "peak updates total"),
            &["result"],
        )?;
        registry.register(Box::new(peak_updates_total.clone()))?;
        let work_cancelled_total = IntCounterVec::new(
            Opts::new("work_cancelled_total", "work cancelled total"),
            &["chain"],
        )?;
        registry.register(Box::new(work_cancelled_total.clone()))?;
        let vdf_sessions_started_total = IntCounterVec::new(
            Opts::new("vdf_sessions_started_total", "vdf sessions started total"),
            &["chain", "reason"],
        )?;
        registry.register(Box::new(vdf_sessions_started_total.clone()))?;
        let operating_point_changes_total = IntCounterVec::new(
            Opts::new(
                "operating_point_changes_total",
                "operating point changes total",
            ),
            &["board", "reason"],
        )?;
        registry.register(Box::new(operating_point_changes_total.clone()))?;
        let last_verified_proof_timestamp_seconds = IntGaugeVec::new(
            Opts::new(
                "last_verified_proof_timestamp_seconds",
                "last verified proof timestamp seconds",
            ),
            &["chain"],
        )?;
        registry.register(Box::new(last_verified_proof_timestamp_seconds.clone()))?;
        let last_submission_timestamp_seconds = IntGaugeVec::new(
            Opts::new(
                "last_submission_timestamp_seconds",
                "last submission timestamp seconds",
            ),
            &["kind"],
        )?;
        registry.register(Box::new(last_submission_timestamp_seconds.clone()))?;
        let last_node_message_timestamp_seconds = IntGaugeVec::new(
            Opts::new(
                "last_node_message_timestamp_seconds",
                "last node message timestamp seconds",
            ),
            &[],
        )?;
        registry.register(Box::new(last_node_message_timestamp_seconds.clone()))?;
        let last_peak_received_timestamp_seconds = IntGaugeVec::new(
            Opts::new(
                "last_peak_received_timestamp_seconds",
                "last peak received timestamp seconds",
            ),
            &[],
        )?;
        registry.register(Box::new(last_peak_received_timestamp_seconds.clone()))?;
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
            proofs_verified_total,
            proof_verification_failures_total,
            submissions_total,
            topology_changes_total,
            asic_recoveries_total,
            client_restarts_total,
            thermal_stops_total,
            node_reconnects_total,
            peak_updates_total,
            work_cancelled_total,
            vdf_sessions_started_total,
            operating_point_changes_total,
            last_verified_proof_timestamp_seconds,
            last_submission_timestamp_seconds,
            last_node_message_timestamp_seconds,
            last_peak_received_timestamp_seconds,
            proof_duration,
            submission_duration,
            last: Mutex::new(Last::default()),
            ever_connected: AtomicBool::new(false),
            clients_started: Mutex::new(BTreeSet::new()),
        });
        for c in Chain::ALL {
            metrics.proofs_verified_total.with_label_values(&[chain(c)]);
            metrics
                .proof_verification_failures_total
                .with_label_values(&[chain(c)]);
            metrics.work_cancelled_total.with_label_values(&[chain(c)]);
            for reason in SessionReason::ALL {
                metrics
                    .vdf_sessions_started_total
                    .with_label_values(&[chain(c), reason.label()]);
            }
        }
        for result in PeakResult::ALL {
            metrics
                .peak_updates_total
                .with_label_values(&[result.label()]);
        }
        for kind in SubmissionKind::ALL {
            metrics.submissions_total.with_label_values(&[kind.label()]);
        }
        Ok(metrics)
    }
    #[cfg(test)]
    pub fn counter(&self, name: &'static str, labels: &[&str]) -> u64 {
        match name {
            "proofs_verified_total" => self.proofs_verified_total.with_label_values(labels).get(),
            "proof_verification_failures_total" => self
                .proof_verification_failures_total
                .with_label_values(labels)
                .get(),
            "submissions_total" => self.submissions_total.with_label_values(labels).get(),
            "topology_changes_total" => self.topology_changes_total.with_label_values(labels).get(),
            "asic_recoveries_total" => self.asic_recoveries_total.with_label_values(labels).get(),
            "client_restarts_total" => self.client_restarts_total.with_label_values(labels).get(),
            "thermal_stops_total" => self.thermal_stops_total.with_label_values(labels).get(),
            "node_reconnects_total" => self.node_reconnects_total.with_label_values(labels).get(),
            "peak_updates_total" => self.peak_updates_total.with_label_values(labels).get(),
            "work_cancelled_total" => self.work_cancelled_total.with_label_values(labels).get(),
            "vdf_sessions_started_total" => self
                .vdf_sessions_started_total
                .with_label_values(labels)
                .get(),
            "operating_point_changes_total" => self
                .operating_point_changes_total
                .with_label_values(labels)
                .get(),
            _ => panic!("Unknown test counter: {name}"),
        }
    }
    pub fn topology_changed(&self) {
        self.topology_changes_total.with_label_values(&[]).inc();
    }
    pub fn verification_failed(&self, c: Chain) {
        self.proof_verification_failures_total
            .with_label_values(&[chain(c)])
            .inc();
    }
    pub fn cancelled(&self, c: Chain) {
        self.work_cancelled_total
            .with_label_values(&[chain(c)])
            .inc();
    }
    pub fn peak_updated(&self, result: PeakResult) {
        self.peak_updates_total
            .with_label_values(&[result.label()])
            .inc();
    }
    pub fn session_started(&self, c: Chain, reason: SessionReason) {
        self.vdf_sessions_started_total
            .with_label_values(&[chain(c), reason.label()])
            .inc();
    }
    pub fn thermal_stopped(&self, board: &str) {
        self.thermal_stops_total.with_label_values(&[board]).inc();
    }
    pub fn asic_recovered(&self, board: &str, engine: Option<u8>) {
        let engine = engine
            .filter(|e| *e < 3)
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown".into());
        self.asic_recoveries_total
            .with_label_values(&[board, &engine])
            .inc();
    }
    pub fn operating_point_changed(&self, board: &str, reason: OperatingPointReason) {
        self.operating_point_changes_total
            .with_label_values(&[board, reason.label()])
            .inc();
    }
    pub fn node_message_received(&self) {
        self.last_node_message_timestamp_seconds
            .with_label_values(&[])
            .set(crate::unix_now() as i64);
    }
    pub fn peak_received(&self) {
        self.last_peak_received_timestamp_seconds
            .with_label_values(&[])
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
            self.node_reconnects_total.with_label_values(&[]).inc();
        }
    }
    pub fn verified(&self, epoch: u64, c: Chain, ips: f64) {
        self.proofs_verified_total
            .with_label_values(&[chain(c)])
            .inc();
        self.last_verified_proof_timestamp_seconds
            .with_label_values(&[chain(c)])
            .set(crate::unix_now() as i64);
        let mut last = self.last.lock().expect("metrics");
        if last.epoch == epoch {
            last.ips = Some(ips);
            last.chains[index(c)] = Some(ips);
        }
    }
    pub fn duration(&self, c: Chain, stage: ProofStage, elapsed: Duration) {
        self.proof_duration
            .with_label_values(&[chain(c), stage.label()])
            .observe(elapsed.as_secs_f64());
    }
    pub fn submitted(&self, kind: SubmissionKind, elapsed: Duration) {
        self.submissions_total
            .with_label_values(&[kind.label()])
            .inc();
        self.last_submission_timestamp_seconds
            .with_label_values(&[kind.label()])
            .set(crate::unix_now() as i64);
        self.submission_duration
            .with_label_values(&[kind.label()])
            .observe(elapsed.as_secs_f64());
    }
    pub fn client_started(&self, board: &str, reason: RestartReason) {
        if !self
            .clients_started
            .lock()
            .expect("metrics")
            .insert(board.to_owned())
        {
            self.client_restarts_total
                .with_label_values(&[board, reason.label()])
                .inc();
        }
    }
    pub fn register_boards(&self, config: &Config) {
        for b in &config.hardware.boards {
            let label = board(config, &b.id);
            self.thermal_stops_total.with_label_values(&[&label]);
            for engine in ["0", "1", "2", "unknown"] {
                self.asic_recoveries_total
                    .with_label_values(&[&label, engine]);
            }
            for reason in RestartReason::ALL {
                self.client_restarts_total
                    .with_label_values(&[&label, reason.label()]);
            }
            for reason in OperatingPointReason::ALL {
                self.operating_point_changes_total
                    .with_label_values(&[&label, reason.label()]);
            }
        }
    }
    pub fn render(&self, shared: &Shared, now: u64) -> Result<String> {
        let (mut samples, readiness, epoch) = snapshot(shared, now);
        let process = shared.runtime.lock().expect("runtime").process.snapshot();
        let network = match shared.config.node.network {
            Network::Mainnet => "mainnet",
            Network::Testnet11 => "testnet11",
        };
        let last = self.last.lock().expect("metrics");
        if last.epoch == epoch && process.phase == crate::operational::Phase::Running {
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
        let ready = readiness.ready && process.ready();
        samples.add("ready", &[], &[], f64::from(ready));
        samples.add("healthy", &[], &[], f64::from(process.healthy()));
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

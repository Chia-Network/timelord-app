//! Process-owned metric handles and Prometheus encoding.
use super::{
    OperatingPointReason, PeakResult, ProofStage, RestartReason, SessionReason, SubmissionKind,
    UnfinishedOutcome, UnfinishedSource, board, chain, index, snapshot,
};
use crate::Shared;
use crate::safety::JournalReset;
use anyhow::Result;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
    core::{Collector, Desc},
};
use std::{
    collections::BTreeSet,
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
    current: super::current::Current,
    proofs_verified_total: IntCounterVec,
    proof_verification_failures_total: IntCounterVec,
    submissions_total: IntCounterVec,
    unfinished_candidates_total: IntCounterVec,
    topology_changes_total: IntCounter,
    asic_recoveries_total: IntCounterVec,
    client_restarts_total: IntCounterVec,
    thermal_stops_total: IntCounterVec,
    node_reconnects_total: IntCounter,
    peak_updates_total: IntCounterVec,
    work_cancelled_total: IntCounterVec,
    vdf_sessions_started_total: IntCounterVec,
    operating_point_changes_total: IntCounterVec,
    last_verified_proof_timestamp_seconds: EventTimestamp,
    last_submission_timestamp_seconds: EventTimestamp,
    last_node_message_timestamp_seconds: EventTimestamp,
    last_peak_received_timestamp_seconds: EventTimestamp,
    proof_duration: HistogramVec,
    submission_duration: HistogramVec,
    last: Mutex<Last>,
    ever_connected: AtomicBool,
    clients_started: Mutex<BTreeSet<String>>,
    board_failures_total: IntCounterVec,
    safety_journal_resets_total: IntCounterVec,
    chain_moves_total: IntCounter,
    delayed_bundles_total: IntCounterVec,
    /// Applied to every family at encode time, so a live node-profile switch
    /// relabels the exposition without rebuilding counters.
    network: Mutex<String>,
}
fn registered<M: Collector + Clone + 'static>(registry: &Registry, metric: M) -> Result<M> {
    registry.register(Box::new(metric.clone()))?;
    Ok(metric)
}
fn counter_vec(
    registry: &Registry,
    name: &str,
    help: &str,
    labels: &[&str],
) -> Result<IntCounterVec> {
    registered(registry, IntCounterVec::new(Opts::new(name, help), labels)?)
}
/// Timestamp gauges stay absent until their first observation. An untouched
/// gauge vector has no series, which is deliberate: exporting 0 would claim
/// the event happened at the Unix epoch.
#[derive(Clone)]
struct EventTimestamp(IntGaugeVec);
impl EventTimestamp {
    fn new(name: &str, help: &str, labels: &[&str]) -> Result<Self> {
        Ok(Self(IntGaugeVec::new(Opts::new(name, help), labels)?))
    }
    fn record(&self, labels: &[&str]) {
        self.0
            .with_label_values(labels)
            .set(crate::unix_now() as i64);
    }
}
impl Collector for EventTimestamp {
    fn desc(&self) -> Vec<&Desc> {
        self.0.desc()
    }
    fn collect(&self) -> Vec<prometheus::proto::MetricFamily> {
        self.0.collect()
    }
}
impl Metrics {
    pub fn new(network: Network) -> Result<Arc<Self>> {
        let network = network.label().to_owned();
        let registry = Registry::new_custom(Some("chia_timelord".into()), None)?;
        let version = registered(
            &registry,
            IntGaugeVec::new(
                Opts::new("version", "Running timelord-service build"),
                &["version"],
            )?,
        )?;
        version
            .with_label_values(&[env!("CARGO_PKG_VERSION")])
            .set(1);
        let build_info = registered(
            &registry,
            IntGaugeVec::new(
                Opts::new("build_info", "Running timelord-service build"),
                &["version", "revision"],
            )?,
        )?;
        build_info
            .with_label_values(&[
                env!("CARGO_PKG_VERSION"),
                option_env!("TIMELORD_BUILD_REVISION").unwrap_or("unknown"),
            ])
            .set(1);
        let started = registered(
            &registry,
            IntGauge::new("process_start_time_seconds", "Service process start time")?,
        )?;
        started.set(crate::unix_now() as i64);
        let buckets = vec![
            0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1., 5., 15., 60., 300.,
        ];
        let metrics = Arc::new(Self {
            current: Default::default(),
            proofs_verified_total: counter_vec(
                &registry,
                "proofs_verified_total",
                "Native verifier outcomes for scheduled work; failures include priming checks",
                &["chain"],
            )?,
            proof_verification_failures_total: counter_vec(
                &registry,
                "proof_verification_failures_total",
                "Native verifier outcomes for scheduled work; failures include priming checks",
                &["chain"],
            )?,
            submissions_total: counter_vec(
                &registry,
                "submissions_total",
                "Successful node writes of signage, infusion or end-of-sub-slot bundles",
                &["kind"],
            )?,
            unfinished_candidates_total: counter_vec(
                &registry,
                "unfinished_candidates_total",
                "Unfinished-block admission outcomes from node frames or peak retries",
                &["outcome", "source"],
            )?,
            topology_changes_total: registered(
                &registry,
                IntCounter::new("topology_changes_total", "Applied chain-route changes")?,
            )?,
            asic_recoveries_total: counter_vec(
                &registry,
                "asic_recoveries_total",
                "Native recovery log events",
                &["board", "engine"],
            )?,
            client_restarts_total: counter_vec(
                &registry,
                "client_restarts_total",
                "Replacement native processes actually launched",
                &["board", "reason"],
            )?,
            thermal_stops_total: counter_vec(
                &registry,
                "thermal_stops_total",
                "New thermal guard stops, counted once per latch",
                &["board"],
            )?,
            node_reconnects_total: registered(
                &registry,
                IntCounter::new(
                    "node_reconnects_total",
                    "Reconnections after the first authenticated connection",
                )?,
            )?,
            peak_updates_total: counter_vec(
                &registry,
                "peak_updates_total",
                "Scheduler accepted, ignored or rejected peak updates",
                &["result"],
            )?,
            work_cancelled_total: counter_vec(
                &registry,
                "work_cancelled_total",
                "Adapter requests dropped without completion",
                &["chain"],
            )?,
            vdf_sessions_started_total: counter_vec(
                &registry,
                "vdf_sessions_started_total",
                "Native challenge initialization acknowledged; initial, input change or reconnect",
                &["chain", "reason"],
            )?,
            operating_point_changes_total: counter_vec(
                &registry,
                "operating_point_changes_total",
                "Acknowledged trial, backoff or restore writes",
                &["board", "reason"],
            )?,
            last_verified_proof_timestamp_seconds: registered(
                &registry,
                EventTimestamp::new(
                    "last_verified_proof_timestamp_seconds",
                    "Wall-clock time of the most recent verified scheduled completion",
                    &["chain"],
                )?,
            )?,
            last_submission_timestamp_seconds: registered(
                &registry,
                EventTimestamp::new(
                    "last_submission_timestamp_seconds",
                    "Wall-clock time of the most recent successful node write of signage, infusion or end-of-sub-slot bundles",
                    &["kind"],
                )?,
            )?,
            last_node_message_timestamp_seconds: registered(
                &registry,
                EventTimestamp::new(
                    "last_node_message_timestamp_seconds",
                    "Last observed node frame",
                    &[],
                )?,
            )?,
            last_peak_received_timestamp_seconds: registered(
                &registry,
                EventTimestamp::new(
                    "last_peak_received_timestamp_seconds",
                    "Last observed decoded peak",
                    &[],
                )?,
            )?,
            proof_duration: registered(
                &registry,
                HistogramVec::new(
                    HistogramOpts::new(
                        "proof_duration_seconds",
                        "Observed proof pipeline stage duration; stages can overlap",
                    )
                    .buckets(buckets.clone()),
                    &["chain", "stage"],
                )?,
            )?,
            submission_duration: registered(
                &registry,
                HistogramVec::new(
                    HistogramOpts::new(
                        "submission_duration_seconds",
                        "Time to write a verified bundle to the node",
                    )
                    .buckets(buckets),
                    &["kind"],
                )?,
            )?,
            last: Mutex::new(Last::default()),
            ever_connected: AtomicBool::new(false),
            clients_started: Mutex::new(BTreeSet::new()),
            board_failures_total: counter_vec(
                &registry,
                "board_failures_total",
                "Native owner failures counted against board reliability",
                &["board"],
            )?,
            safety_journal_resets_total: counter_vec(
                &registry,
                "safety_journal_resets_total",
                "Processes that found a previously written but now absent or unreadable board safety journal and started from cautious defaults; at most 1 per process, exported only by the process that reported the reset, so use the raw value, not increase()",
                &["reason"],
            )?,
            chain_moves_total: registered(
                &registry,
                IntCounter::new("chain_moves_total", "Chains moved between boards")?,
            )?,
            delayed_bundles_total: counter_vec(
                &registry,
                "delayed_bundles_total",
                "Verified chain proofs that waited over 10 seconds for their bundle partner",
                &["chain"],
            )?,
            network: Mutex::new(network),
            registry,
        });
        for c in Chain::ALL {
            metrics.delayed_bundles_total.with_label_values(&[chain(c)]);
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
        for outcome in UnfinishedOutcome::ALL {
            for source in UnfinishedSource::ALL {
                metrics
                    .unfinished_candidates_total
                    .with_label_values(&[outcome.label(), source.label()]);
            }
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
            "unfinished_candidates_total" => self
                .unfinished_candidates_total
                .with_label_values(labels)
                .get(),
            "topology_changes_total" => {
                assert!(labels.is_empty());
                self.topology_changes_total.get()
            }
            "asic_recoveries_total" => self.asic_recoveries_total.with_label_values(labels).get(),
            "client_restarts_total" => self.client_restarts_total.with_label_values(labels).get(),
            "thermal_stops_total" => self.thermal_stops_total.with_label_values(labels).get(),
            "node_reconnects_total" => {
                assert!(labels.is_empty());
                self.node_reconnects_total.get()
            }
            "peak_updates_total" => self.peak_updates_total.with_label_values(labels).get(),
            "work_cancelled_total" => self.work_cancelled_total.with_label_values(labels).get(),
            "vdf_sessions_started_total" => self
                .vdf_sessions_started_total
                .with_label_values(labels)
                .get(),
            "board_failures_total" => self.board_failures_total.with_label_values(labels).get(),
            "chain_moves_total" => {
                assert!(labels.is_empty());
                self.chain_moves_total.get()
            }
            "operating_point_changes_total" => self
                .operating_point_changes_total
                .with_label_values(labels)
                .get(),
            _ => panic!("Unknown test counter: {name}"),
        }
    }
    pub fn set_network(&self, network: &Network) {
        *self.network.lock().expect("metrics") = network.label().to_owned();
    }
    pub fn board_failed(&self, board: &str) {
        self.board_failures_total.with_label_values(&[board]).inc();
    }
    pub fn safety_journal_reset(&self, reset: JournalReset) {
        self.safety_journal_resets_total
            .with_label_values(&[reset.label()])
            .inc();
    }
    pub fn chains_moved(&self, count: u64) {
        self.chain_moves_total.inc_by(count);
    }
    pub fn bundle_delayed(&self, c: Chain) {
        self.delayed_bundles_total
            .with_label_values(&[chain(c)])
            .inc();
    }
    #[cfg(test)]
    pub fn histogram_count(&self, name: &'static str, labels: &[&str]) -> u64 {
        match name {
            "proof_duration_seconds" => self
                .proof_duration
                .with_label_values(labels)
                .get_sample_count(),
            _ => panic!("Unknown test histogram: {name}"),
        }
    }
    #[cfg(test)]
    pub fn session_starts(&self, c: Chain) -> u64 {
        SessionReason::ALL
            .iter()
            .map(|reason| {
                self.vdf_sessions_started_total
                    .with_label_values(&[chain(c), reason.label()])
                    .get()
            })
            .sum()
    }
    pub fn topology_changed(&self) {
        self.topology_changes_total.inc();
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
    pub fn asic_recovered(&self, board: &str, engine: u8) {
        let engine = if engine < 3 {
            engine.to_string()
        } else {
            "unknown".into()
        };
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
        self.last_node_message_timestamp_seconds.record(&[]);
    }
    pub fn peak_received(&self) {
        self.last_peak_received_timestamp_seconds.record(&[]);
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
            self.node_reconnects_total.inc();
        }
    }
    pub fn verified(&self, epoch: u64, c: Chain, ips: f64) {
        self.proofs_verified_total
            .with_label_values(&[chain(c)])
            .inc();
        self.last_verified_proof_timestamp_seconds
            .record(&[chain(c)]);
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
    pub fn unfinished_candidate(&self, outcome: UnfinishedOutcome, source: UnfinishedSource) {
        self.unfinished_candidates_total
            .with_label_values(&[outcome.label(), source.label()])
            .inc();
    }
    pub fn submitted(&self, kind: SubmissionKind, elapsed: Duration) {
        self.submissions_total
            .with_label_values(&[kind.label()])
            .inc();
        self.last_submission_timestamp_seconds
            .record(&[kind.label()]);
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
            self.board_failures_total.with_label_values(&[&label]);
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
        let snapshot = snapshot(shared, now);
        let mut samples = snapshot.samples;
        let network = self.network.lock().expect("metrics").clone();
        let last = self.last.lock().expect("metrics");
        if snapshot.epoch == Some(last.epoch) {
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
        samples.add("ready", &[], &[], f64::from(snapshot.readiness.ready));
        samples.add("healthy", &[], &[], f64::from(snapshot.healthy));
        let mut families = self.registry.gather();
        for family in &mut families {
            for metric in family.mut_metric() {
                let mut label = prometheus::proto::LabelPair::default();
                label.set_name("network".into());
                label.set_value(network.clone());
                let mut labels = metric.take_label();
                labels.insert(0, label);
                metric.set_label(labels);
            }
        }
        families.extend(self.current.collect(samples, &network)?);
        let mut encoded = Vec::new();
        TextEncoder::new().encode(&families, &mut encoded)?;
        Ok(String::from_utf8(encoded)?)
    }
}

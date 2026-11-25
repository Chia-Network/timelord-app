//! Reusable current-state gauge families. Counter ownership remains in Metrics.
use super::snapshot::Samples;
use anyhow::{Result, anyhow, ensure};
use prometheus::{GaugeVec, Opts, core::Collector, proto::MetricFamily};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    sync::Mutex,
};
#[derive(Default)]
pub(super) struct Current(Mutex<BTreeMap<&'static str, Family>>);
struct Family {
    keys: Vec<&'static str>,
    gauge: GaugeVec,
}
const HELP: &[(&str, &str)] = &[
    (
        "board_up",
        "Board passes managed/unmanaged readiness checks",
    ),
    ("board_state", "Current state as a one-hot value 1"),
    (
        "board_temperature_celsius",
        "Observed board temperature; omitted without current telemetry",
    ),
    (
        "board_frequency_hertz",
        "Observed board frequency; omitted without current telemetry",
    ),
    (
        "board_voltage_volts",
        "Observed board voltage; omitted without current telemetry",
    ),
    (
        "raw_ips",
        "Running engine's native rate; expires after six seconds",
    ),
    (
        "raw_sample_timestamp_seconds",
        "Running engine's observation time; expires after six seconds",
    ),
    ("chain_assignment", "Current assignment, value 1"),
    (
        "chain_estimated_ips",
        "Latest validated completion for each chain in this session",
    ),
    (
        "estimated_ips",
        "Latest validated completion from any chain in this node session",
    ),
    ("clients_configured", "Assigned client capacity"),
    (
        "clients_connected",
        "Observed client sockets (not proof readiness)",
    ),
    ("chain_required", "Required work roles, for alerts"),
    ("chain_available", "Usable assigned capacity, for alerts"),
    (
        "tuning_paused",
        "Automatic tuning paused, not proof service paused",
    ),
    ("tuning_state", "Current bounded tuning state"),
    (
        "scheduler_peak_height",
        "Accepted peak height, absent before a peak or after session replacement",
    ),
    (
        "deficit",
        "Accepted peak deficit, absent before a peak or after session replacement",
    ),
    (
        "work_pending",
        "Adapter has an unfinished scheduled request, 0 or 1",
    ),
    ("node_connected", "Authenticated node link"),
    ("healthy", "Probe state, 0 or 1"),
    ("ready", "Probe state, 0 or 1"),
];
fn help(name: &str) -> Result<&'static str> {
    HELP.iter()
        .find_map(|(metric, help)| (*metric == name).then_some(*help))
        .ok_or_else(|| anyhow!("Gauge {name} has no documented HELP text"))
}
impl Current {
    pub fn collect(&self, samples: Samples, network: &str) -> Result<Vec<MetricFamily>> {
        let mut families = self.0.lock().expect("current gauge families");
        for family in families.values() {
            family.gauge.reset();
        }
        for (name, group) in samples.0 {
            let family = match families.entry(name) {
                Entry::Vacant(entry) => {
                    let mut keys = vec!["network"];
                    keys.extend(group.keys.iter().copied());
                    let gauge = GaugeVec::new(
                        Opts::new(format!("chia_timelord_{name}"), help(name)?),
                        &keys,
                    )?;
                    entry.insert(Family {
                        keys: group.keys.clone(),
                        gauge,
                    })
                }
                Entry::Occupied(entry) => entry.into_mut(),
            };
            ensure!(
                family.keys == group.keys,
                "Gauge {name} changed its label contract"
            );
            for (labels, value) in group.values {
                let labels = std::iter::once(network)
                    .chain(labels.iter().map(String::as_str))
                    .collect::<Vec<_>>();
                family.gauge.with_label_values(&labels).set(value);
            }
        }
        Ok(families
            .values()
            .flat_map(|family| family.gauge.collect())
            .filter(|family| !family.get_metric().is_empty())
            .collect())
    }
}

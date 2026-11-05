//! Reusable current-state gauge families. Counter ownership remains in Metrics.
use super::snapshot::Samples;
use anyhow::{Result, ensure};
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
                        Opts::new(format!("chia_timelord_{name}"), name.replace('_', " ")),
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

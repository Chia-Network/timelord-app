//! Current availability policy and bounded gauge projection; never reads chart history.
use super::{Metrics, chain, index};
use crate::{Shared, runtime::RawState};
use serde::Serialize;
use std::{collections::BTreeMap, sync::atomic::Ordering};
use timelord_core::Chain;

#[derive(Default)]
pub(super) struct Samples(pub(super) BTreeMap<&'static str, Group>);
pub(super) struct Group {
    pub(super) keys: Vec<&'static str>,
    pub(super) values: Vec<(Vec<String>, f64)>,
}
impl Samples {
    pub(super) fn add(
        &mut self,
        name: &'static str,
        keys: &[&'static str],
        labels: &[&str],
        value: f64,
    ) {
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
pub fn ready(shared: &Shared, metrics: &Metrics) -> bool {
    metrics.healthy()
        && !metrics.reconfiguring.load(Ordering::Relaxed)
        && snapshot(shared, crate::unix_now()).1.ready
}
pub(super) fn snapshot(shared: &Shared, now: u64) -> (Samples, Readiness, u64) {
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

//! Current availability policy and bounded gauge projection; never reads chart history.
use super::{board, chain, index};
use crate::{Shared, runtime::RawState};
use serde::Serialize;
use std::collections::BTreeMap;
use timelord_core::Chain;

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Gauge {
    BoardUp,
    BoardState,
    BoardTemperatureCelsius,
    BoardFrequencyHertz,
    BoardVoltageVolts,
    RawIps,
    RawSampleTimestampSeconds,
    ChainAssignment,
    ChainEstimatedIps,
    EstimatedIps,
    ClientsConfigured,
    ClientsConnected,
    ChainRequired,
    ChainAvailable,
    TuningPaused,
    TuningState,
    SchedulerPeakHeight,
    Deficit,
    WorkPending,
    NodeConnected,
    Healthy,
    Ready,
}
impl Gauge {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::BoardUp => "board_up",
            Self::BoardState => "board_state",
            Self::BoardTemperatureCelsius => "board_temperature_celsius",
            Self::BoardFrequencyHertz => "board_frequency_hertz",
            Self::BoardVoltageVolts => "board_voltage_volts",
            Self::RawIps => "raw_ips",
            Self::RawSampleTimestampSeconds => "raw_sample_timestamp_seconds",
            Self::ChainAssignment => "chain_assignment",
            Self::ChainEstimatedIps => "chain_estimated_ips",
            Self::EstimatedIps => "estimated_ips",
            Self::ClientsConfigured => "clients_configured",
            Self::ClientsConnected => "clients_connected",
            Self::ChainRequired => "chain_required",
            Self::ChainAvailable => "chain_available",
            Self::TuningPaused => "tuning_paused",
            Self::TuningState => "tuning_state",
            Self::SchedulerPeakHeight => "scheduler_peak_height",
            Self::Deficit => "deficit",
            Self::WorkPending => "work_pending",
            Self::NodeConnected => "node_connected",
            Self::Healthy => "healthy",
            Self::Ready => "ready",
        }
    }

    pub(super) fn help(self) -> &'static str {
        match self {
            Self::BoardUp => "Board passes managed/unmanaged readiness checks",
            Self::BoardState => "Current state as a one-hot value 1",
            Self::BoardTemperatureCelsius => {
                "Observed board temperature; omitted without current telemetry"
            }
            Self::BoardFrequencyHertz => {
                "Observed board frequency; omitted without current telemetry"
            }
            Self::BoardVoltageVolts => "Observed board voltage; omitted without current telemetry",
            Self::RawIps => "Running engine's native rate; expires after six seconds",
            Self::RawSampleTimestampSeconds => {
                "Running engine's observation time; expires after six seconds"
            }
            Self::ChainAssignment => "Current assignment, value 1",
            Self::ChainEstimatedIps => "Latest validated completion for each chain in this session",
            Self::EstimatedIps => "Latest validated completion from any chain in this node session",
            Self::ClientsConfigured => "Assigned client capacity",
            Self::ClientsConnected => "Observed client sockets (not proof readiness)",
            Self::ChainRequired => "Required work roles, for alerts",
            Self::ChainAvailable => "Usable assigned capacity, for alerts",
            Self::TuningPaused => "Automatic tuning paused, not proof service paused",
            Self::TuningState => "Current bounded tuning state",
            Self::SchedulerPeakHeight => {
                "Accepted peak height, absent before a peak or after session replacement"
            }
            Self::Deficit => {
                "Accepted peak deficit, absent before a peak or after session replacement"
            }
            Self::WorkPending => "Adapter has an unfinished scheduled request, 0 or 1",
            Self::NodeConnected => "Authenticated node link",
            Self::Healthy | Self::Ready => "Probe state, 0 or 1",
        }
    }
}
#[derive(Default)]
pub(super) struct Samples(pub(super) BTreeMap<Gauge, Group>);
pub(super) struct Group {
    pub(super) keys: Vec<&'static str>,
    pub(super) values: Vec<(Vec<String>, f64)>,
}
impl Samples {
    pub(super) fn add(&mut self, gauge: Gauge, keys: &[&'static str], labels: &[&str], value: f64) {
        self.0
            .entry(gauge)
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
    !h.lifecycle.status().latched_fault
        && if managed {
            h.lifecycle.accepts_work()
                && h.temperature_c.is_some_and(f64::is_finite)
                && h.temperature_unix <= now
                && now - h.temperature_unix <= 30
        } else {
            h.lifecycle.verified_generation() == Some(h.lifecycle.status().generation)
        }
}
pub(super) struct Snapshot {
    pub samples: Samples,
    pub readiness: Readiness,
    pub healthy: bool,
    pub work_ready: bool,
    pub epoch: Option<u64>,
}
pub fn ready(shared: &Shared) -> bool {
    snapshot(shared, crate::unix_now()).readiness.ready
}
pub(crate) fn work_ready(shared: &Shared) -> bool {
    snapshot(shared, crate::unix_now()).work_ready
}
pub(super) fn snapshot(shared: &Shared, now: u64) -> Snapshot {
    // Capture process and runtime facts once; every projection uses this policy.
    let clients = shared.clients.lock().expect("clients");
    let state = shared.runtime.lock().expect("runtime");
    let assignments = crate::hardware::continuous::routed_assignments(shared, &state);
    let process = shared.process.snapshot();
    let mut samples = Samples::default();
    let incomplete = || {
        if process.phase == crate::operational::Phase::Stopping && process.failure.is_none() {
            "stopping"
        } else {
            "service_unavailable"
        }
    };
    if !process.producing() {
        samples.add(Gauge::NodeConnected, &[], &[], 0.);
        return Snapshot {
            samples,
            readiness: Readiness {
                ready: false,
                reason: incomplete(),
            },
            healthy: process.healthy(),
            work_ready: false,
            epoch: None,
        };
    }
    samples.add(Gauge::ClientsConfigured, &[], &[], assignments.len() as f64);
    samples.add(Gauge::ClientsConnected, &[], &[], clients.len() as f64);
    let connected = ["authenticated", "ready"].contains(&state.node.as_str());
    samples.add(Gauge::NodeConnected, &[], &[], f64::from(connected));
    samples.add(
        Gauge::TuningPaused,
        &[],
        &[],
        f64::from(state.tuning_paused),
    );
    if let Some(height) = state.scheduler_height {
        samples.add(Gauge::SchedulerPeakHeight, &[], &[], height as f64);
    }
    if let Some(deficit) = state.scheduler_deficit {
        samples.add(Gauge::Deficit, &[], &[], f64::from(deficit));
    }
    let managed = shared.config.hardware.client_binary.is_some();
    // Managed sockets are usable on connect; unmanaged ones once verified.
    let sockets = |board_id: &str| {
        clients
            .iter()
            .filter(|(id, slot)| {
                id == board_id
                    && (managed
                        || state
                            .boards
                            .get(id)
                            .is_some_and(|h| h.lifecycle.slot_verified(*slot)))
            })
            .count()
    };
    let mut required = vec![Chain::Challenge, Chain::Reward];
    for c in Chain::ALL {
        let pending = state.pending[index(c)] > 0;
        samples.add(
            Gauge::WorkPending,
            &["chain"],
            &[chain(c)],
            f64::from(pending),
        );
        if pending && !required.contains(&c) {
            required.push(c);
        }
    }
    for c in Chain::ALL {
        let available = assignments.iter().find(|a| a.chain == c).is_some_and(|a| {
            let needed = assignments
                .iter()
                .filter(|other| other.board_id == a.board_id && required.contains(&other.chain))
                .count();
            state
                .boards
                .get(&a.board_id)
                .is_some_and(|h| board_usable(h, managed, now))
                && sockets(&a.board_id) >= needed.max(1)
        });
        samples.add(
            Gauge::ChainRequired,
            &["chain"],
            &[chain(c)],
            f64::from(required.contains(&c)),
        );
        samples.add(
            Gauge::ChainAvailable,
            &["chain"],
            &[chain(c)],
            f64::from(available),
        );
    }
    let mut usable = true;
    for b in &shared.config.hardware.boards {
        let label = board(&shared.config, &b.id);
        let label = label.as_str();
        let hardware = state.boards.get(&b.id);
        let needed = assignments
            .iter()
            .filter(|a| a.board_id == b.id && required.contains(&a.chain))
            .count();
        let sockets = sockets(&b.id);
        let up =
            hardware.is_some_and(|h| board_usable(h, managed, now)) && (managed || sockets > 0);
        if needed > 0 && (!up || sockets < needed) {
            usable = false;
        }
        samples.add(Gauge::BoardUp, &["board"], &[label], f64::from(up));
        let state_name = hardware
            .map(|h| h.lifecycle.status().phase.as_str())
            .unwrap_or("not_inspected");
        samples.add(
            Gauge::BoardState,
            &["board", "state"],
            &[label, state_name],
            1.,
        );
        for assignment in assignments.iter().filter(|a| a.board_id == b.id) {
            samples.add(
                Gauge::ChainAssignment,
                &["board", "chain"],
                &[label, chain(assignment.chain)],
                1.,
            );
        }
        if let Some(h) = hardware {
            if h.lifecycle.status().phase == crate::board_lifecycle::Phase::Connected
                && !h.lifecycle.status().latched_fault
            {
                if h.temperature_unix <= now && now - h.temperature_unix <= 30 {
                    if let Some(value) = h.temperature_c.filter(|v| v.is_finite()) {
                        samples.add(Gauge::BoardTemperatureCelsius, &["board"], &[label], value);
                    }
                    if let Some(value) = h.frequency_mhz {
                        samples.add(
                            Gauge::BoardFrequencyHertz,
                            &["board"],
                            &[label],
                            value * 1_000_000.,
                        );
                    }
                    if let Some(value) = h.voltage_v {
                        samples.add(Gauge::BoardVoltageVolts, &["board"], &[label], value);
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
                        samples.add(
                            Gauge::RawIps,
                            &["board", "engine"],
                            &[label, &engine],
                            raw.ips,
                        );
                        samples.add(
                            Gauge::RawSampleTimestampSeconds,
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
                samples.add(Gauge::TuningState, &["board", "state"], &[label, name], 1.);
            } else if b.profile == timelord_core::ProfileMode::Manual {
                samples.add(
                    Gauge::TuningState,
                    &["board", "state"],
                    &[label, "manual"],
                    1.,
                );
            }
        }
    }
    let reason = if shared.config.node.certificate.is_none() {
        "setup_required"
    } else if assignments.is_empty() {
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
    let work_ready = reason == "ready";
    let reason = if !process.ready() {
        incomplete()
    } else {
        reason
    };
    Snapshot {
        samples,
        readiness: Readiness {
            ready: reason == "ready",
            reason,
        },
        healthy: process.healthy(),
        work_ready,
        epoch: connected.then_some(state.metric_epoch),
    }
}

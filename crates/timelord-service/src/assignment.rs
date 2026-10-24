//! Resolve per topology generation. Tuning alone never mutates chain ownership.
use crate::{
    automatic,
    runtime::{RawSpeed, RawState},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use timelord_core::{Assignment, Config, ProfileMode};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Performance {
    pub raw_ips: f64,
    pub samples: usize,
    pub observed_seconds: u64,
    pub recoveries_per_hour: f64,
}
impl Performance {
    fn usable(&self) -> bool {
        self.raw_ips.is_finite()
            && self.raw_ips > 0.
            && self.samples >= 12
            && self.observed_seconds >= 60
            && self.recoveries_per_hour.is_finite()
            && self.recoveries_per_hour >= 0.
    }
}

#[derive(Default)]
pub struct RawWindow {
    epoch: u64,
    samples: std::collections::VecDeque<(u64, f64, u64)>,
}
impl RawWindow {
    pub fn observe(&mut self, epoch: u64, now: u64, recoveries: u64, raw: &[RawSpeed]) {
        if self.epoch != epoch {
            *self = Self {
                epoch,
                ..Default::default()
            };
        }
        if let Some(sample) = raw.iter().find(|s| {
            s.engine == 1
                && s.state == RawState::Running
                && s.ips.is_finite()
                && s.ips > 0.
                && s.timestamp_unix >= epoch.saturating_add(10)
                && s.timestamp_unix <= now
                && now - s.timestamp_unix <= 10
        }) && self
            .samples
            .back()
            .is_none_or(|s| sample.timestamp_unix > s.0)
        {
            if self
                .samples
                .back()
                .is_some_and(|s| sample.timestamp_unix.saturating_sub(s.0) > 15)
            {
                self.samples.clear();
            }
            self.samples
                .push_back((sample.timestamp_unix, sample.ips, recoveries));
        }
        while self
            .samples
            .front()
            .is_some_and(|s| now.saturating_sub(s.0) > 300)
        {
            self.samples.pop_front();
        }
    }
    pub fn performance(&self) -> Option<Performance> {
        let span = self
            .samples
            .back()?
            .0
            .saturating_sub(self.samples.front()?.0);
        let mut values: Vec<_> = self.samples.iter().map(|s| s.1).collect();
        values.sort_by(f64::total_cmp);
        let middle = values.len() / 2;
        let raw_ips = if values.len() % 2 == 0 {
            (values[middle - 1] + values[middle]) / 2.
        } else {
            values[middle]
        };
        let value = Performance {
            raw_ips,
            samples: values.len(),
            observed_seconds: span,
            recoveries_per_hour: self
                .samples
                .back()?
                .2
                .saturating_sub(self.samples.front()?.2) as f64
                * 3600.
                / span.max(1) as f64,
        };
        value.usable().then_some(value)
    }
}

/// All three boards need comparable accepted engine-1 evidence. Otherwise keep
/// configuration order; an unknown board must not be treated as the slowest.
fn rank(mut plan: Vec<Assignment>, scores: &[Option<f64>]) -> Vec<Assignment> {
    if plan.len() != 3
        || scores.len() != 3
        || scores
            .iter()
            .any(|s| s.is_none_or(|s| !s.is_finite() || s <= 0.))
    {
        return plan;
    }
    let mut remaining = vec![0, 1, 2];
    let mut order = vec![];
    while !remaining.is_empty() {
        let fastest = remaining
            .iter()
            .map(|&i| scores[i].unwrap())
            .fold(0., f64::max);
        // Within 2% of the fastest remaining board, keep configuration order.
        // Grouping avoids a non-transitive approximate-equality sort comparator.
        let tied: Vec<_> = remaining
            .iter()
            .copied()
            .filter(|&i| scores[i].unwrap() >= fastest * 0.98)
            .collect();
        remaining.retain(|i| !tied.contains(i));
        order.extend(tied);
    }
    let boards: Vec<_> = plan.iter().map(|a| a.board_id.clone()).collect();
    for (assignment, index) in plan.iter_mut().zip(order) {
        assignment.board_id = boards[index].clone();
    }
    plan
}

pub fn resolve(config: &Config) -> Result<Vec<Assignment>> {
    let plan = config.assignments().map_err(anyhow::Error::msg)?;
    if config.hardware.boards.len() != 3
        || config.hardware.chain_assignments.is_some()
        || config.hardware.automatic_state.is_none()
        || config.hardware.client_binary.is_none()
    {
        return Ok(plan);
    }
    let hash = automatic::fingerprint(config)?;
    let host = automatic::host();
    let scores: Result<Vec<_>> = config
        .hardware
        .boards
        .iter()
        .map(|board| {
            if board.profile != ProfileMode::Automatic {
                return Ok(None);
            }
            Ok(automatic::learned(config, board, 1, &hash, &host)?
                .filter(|e| {
                    e.active_seconds >= board.automatic.healthy_seconds
                        && e.point.voltage_v >= board.automatic.start_voltage_v
                        && e.updated_unix <= crate::unix_now()
                        && crate::unix_now().saturating_sub(e.updated_unix) <= 30 * 86400
                })
                .and_then(|e| e.performance)
                .filter(Performance::usable)
                .map(|p| p.raw_ips))
        })
        .collect();
    match scores {
        Ok(scores) => Ok(rank(plan, &scores)),
        Err(error) => {
            tracing::warn!(%error, "Assignment evidence unavailable; preserving configured order");
            Ok(plan)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        let mut config = Config::default();
        config.hardware.boards = (0..3).map(|i| toml::from_str(&format!(
            "id='board-{i}'\nname='Board {i}'\nserial='serial-{i}'\nclient_port={}\ndevice_location={}", 18110+i, 10+i
        )).unwrap()).collect();
        config
    }
    fn ids(plan: &[Assignment]) -> Vec<&str> {
        plan.iter().map(|a| a.board_id.as_str()).collect()
    }
    fn performance(ips: f64) -> Performance {
        Performance {
            raw_ips: ips,
            samples: 20,
            observed_seconds: 100,
            recoveries_per_hour: 40.,
        }
    }
    #[test]
    fn ranks_only_comparable_boards_and_keeps_close_or_unknown_order() {
        let plan = config().assignments().unwrap();
        let ranked = rank(
            plan.clone(),
            &[Some(900_000.), Some(1_100_000.), Some(1_000_000.)],
        );
        assert_eq!(ids(&ranked), ["board-1", "board-2", "board-0"]);
        assert!(ranked.iter().all(|a| a.engine == 1));
        assert_eq!(
            ranked.iter().map(|a| a.chain).collect::<Vec<_>>(),
            timelord_core::Chain::ALL
        );
        for scores in [
            [Some(1_000_000.), Some(1_010_000.), Some(1_005_000.)],
            [Some(900_000.), None, Some(1_100_000.)],
            [Some(f64::NAN), Some(1.), Some(2.)],
        ] {
            assert_eq!(rank(plan.clone(), &scores), plan);
        }
        for count in 1..=2 {
            let mut c = config();
            c.hardware.boards.truncate(count);
            assert_eq!(resolve(&c).unwrap(), c.assignments().unwrap());
        }
    }
    #[test]
    fn raw_window_requires_fresh_distinct_engine_one_samples_and_resets_with_phase() {
        let mut window = RawWindow::default();
        let sample = |time, ips, engine, state| RawSpeed {
            timestamp_unix: time,
            ips,
            engine,
            state,
        };
        for now in 100..200 {
            window.observe(
                100,
                now,
                0,
                &[sample(now, 2_000_000., 0, RawState::Running)],
            );
            window.observe(
                100,
                now,
                0,
                &[sample(100, 2_000_000., 1, RawState::Running)],
            );
        }
        assert!(window.performance().is_none());
        for now in (110..=210).step_by(5) {
            window.observe(
                100,
                now,
                (now - 110) / 50,
                &[sample(now, 1_000_000., 1, RawState::Running)],
            );
            window.observe(
                100,
                now,
                0,
                &[sample(now, 99_000_000., 1, RawState::Running)],
            );
        }
        let value = window.performance().unwrap();
        assert_eq!(value.samples, 21);
        assert_eq!(value.raw_ips, 1_000_000.);
        assert_eq!(value.observed_seconds, 100);
        assert_eq!(value.recoveries_per_hour, 72.);
        // A short outage starts a new evidence window rather than joining two runs.
        window.observe(100, 240, 2, &[sample(240, 900_000., 1, RawState::Running)]);
        assert!(window.performance().is_none());
        window.observe(100, 600, 2, &[]);
        assert!(window.performance().is_none());
        window.observe(600, 610, 2, &[sample(610, 900_000., 1, RawState::Running)]);
        assert!(window.performance().is_none());
    }
    #[test]
    fn resolution_follows_serial_and_provenance_without_mutating_the_live_plan() {
        let directory =
            std::env::temp_dir().join(format!("assignment-evidence-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut c = config();
        let client = directory.join("client");
        std::fs::write(&client, "fixture binary").unwrap();
        c.hardware.client_binary = Some(client);
        let cache = directory.join("profiles.json");
        c.hardware.automatic_state = Some(cache.clone());
        let hash = automatic::fingerprint(&c).unwrap();
        let host = automatic::host();
        let mut entries: Vec<_> = (0..3)
            .map(|i| automatic::Learned {
                serial: format!("serial-{i}"),
                engines: 1,
                engine_mask: Some(2),
                host: host.clone(),
                client_sha256: hash.clone(),
                point: automatic::Point {
                    frequency_mhz: 1500.,
                    voltage_v: 1.,
                },
                active_seconds: 120,
                updated_unix: crate::unix_now(),
                ceiling_mhz: 1600.,
                cooldown_seconds: 0,
                performance: Some(performance(900_000. + i as f64 * 100_000.)),
                retest: None,
            })
            .collect();
        for entry in &entries {
            automatic::save(&cache, entry.clone()).unwrap();
        }
        let live = resolve(&c).unwrap();
        assert_eq!(ids(&live), ["board-2", "board-1", "board-0"]);
        c.hardware.boards[2].id = "same-serial-new-id".into();
        c.hardware.boards[2].device_location = Some(99);
        assert_eq!(
            ids(&resolve(&c).unwrap()),
            ["same-serial-new-id", "board-1", "board-0"]
        );
        // New cache evidence applies only to a future resolution, not this run's plan.
        entries[0].performance = Some(performance(1_300_000.));
        automatic::save(&cache, entries[0].clone()).unwrap();
        assert_eq!(
            ids(&resolve(&c).unwrap()),
            ["board-0", "same-serial-new-id", "board-1"]
        );
        assert_eq!(ids(&live), ["board-2", "board-1", "board-0"]);
        for invalidation in 0..6 {
            let mut invalid = entries[0].clone();
            match invalidation {
                0 => invalid.active_seconds = 0,
                1 => invalid.performance = None,
                2 => invalid.updated_unix = 1,
                3 => invalid.performance = Some(performance(-1.)),
                4 => invalid.host = "other-mac".into(),
                _ => invalid.engine_mask = Some(1),
            }
            // Remove valid entries for this serial so other-provenance entries can't hide it.
            std::fs::remove_file(&cache).unwrap();
            for e in [&invalid, &entries[1], &entries[2]] {
                automatic::save(&cache, e.clone()).unwrap();
            }
            assert_eq!(resolve(&c).unwrap(), c.assignments().unwrap());
        }
        std::fs::write(&cache, "corrupt").unwrap();
        assert_eq!(resolve(&c).unwrap(), c.assignments().unwrap());
        c.hardware.chain_assignments = Some(timelord_core::ChainAssignments {
            challenge: "serial-1".into(),
            reward: "serial-0".into(),
            infused_challenge: "serial-2".into(),
        });
        assert_eq!(
            ids(&resolve(&c).unwrap()),
            ["board-1", "board-0", "same-serial-new-id"]
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}

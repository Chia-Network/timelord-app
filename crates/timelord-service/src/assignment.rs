//! Raw-speed evidence windows used by Automatic tuning.
use crate::runtime::{RawSpeed, RawState};
use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use super::*;
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
}

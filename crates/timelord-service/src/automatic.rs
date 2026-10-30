//! Online policy. All USB writes are acknowledged by the native client's owner thread.
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use timelord_core::AutomaticConfig;

mod profiles;
pub use profiles::{Learned, fingerprint, host, initial, learned, save, socket_path};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub frequency_mhz: f64,
    pub voltage_v: f64,
}
#[derive(Clone, Default, Serialize)]
pub struct Status {
    pub state: String,
    pub message: String,
    pub active_seconds: u64,
    pub cooldown_seconds: u64,
    pub retest_seconds_remaining: Option<u64>,
}
pub enum Action {
    Set(Point, String),
    Save,
}
pub struct Input {
    pub exploration_allowed: bool,
    pub healthy: bool,
    pub recoveries: u64,
    pub score: Option<f64>,
    pub temperature: Option<f64>,
    pub temperature_limit: f64,
    pub paused: bool,
}
// A severe burst is independent of proof throughput: a storm can starve proofs.
// Keep only the last 20 events, and allow five seconds after a clock write for
// settling and pre-write log backlog. Normal recoveries still use the IPS policy.
const STORM_RECOVERIES: usize = 20;
const STORM_WINDOW_SECONDS: u64 = 10;
const SETTLING_SECONDS: u64 = 5;
#[derive(Default)]
struct RecoveryBurst {
    events: VecDeque<u64>,
    settled_at: u64,
}
impl RecoveryBurst {
    fn reset(&mut self, now: u64) {
        self.events.clear();
        self.settled_at = now.saturating_add(SETTLING_SECONDS);
    }
    fn observe(&mut self, now: u64, count: u64) -> bool {
        if now < self.settled_at {
            self.events.clear();
            return false;
        }
        while self
            .events
            .front()
            .is_some_and(|&t| now.saturating_sub(t) >= STORM_WINDOW_SECONDS)
        {
            self.events.pop_front();
        }
        for _ in 0..count.min(STORM_RECOVERIES as u64) {
            if self.events.len() == STORM_RECOVERIES {
                self.events.pop_front();
            }
            self.events.push_back(now);
        }
        self.events.len() == STORM_RECOVERIES
    }
}
/// Countdown counts only healthy verified work, never offline wall time.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetestState {
    pub attempts: u32,
    pub remaining_seconds: u64,
}
#[derive(Clone, Copy)]
enum TrialKind {
    Exploration,
    Retest,
}
struct Trial {
    rollback: Point,
    kind: TrialKind,
}

pub struct Policy {
    pub point: Point,
    pub status: Status,
    pub epoch: u64,
    pub settings: AutomaticConfig,
    baseline: Option<f64>,
    trial: Option<Trial>,
    retest: RetestState,
    last_tick: u64,
    recoveries: u64,
    recovery_burst: RecoveryBurst,
    window_started: u64,
    window_recoveries: u64,
    degraded_windows: u32,
    ceiling: f64,
    was_paused: bool,
    frequencies: Vec<f64>,
}
impl Policy {
    pub fn new(settings: AutomaticConfig, point: Point, now: u64, frequencies: Vec<f64>) -> Self {
        Self {
            ceiling: settings.max_frequency_mhz,
            retest: RetestState {
                attempts: 0,
                remaining_seconds: settings.retest_after_seconds,
            },
            settings,
            point,
            baseline: None,
            trial: None,
            epoch: now,
            last_tick: now,
            recoveries: 0,
            recovery_burst: RecoveryBurst {
                settled_at: now.saturating_add(SETTLING_SECONDS),
                ..Default::default()
            },
            window_started: now,
            window_recoveries: 0,
            degraded_windows: 0,
            was_paused: false,
            frequencies,
            status: Status {
                state: "learning".into(),
                message: "Observing delivered proof performance".into(),
                ..Default::default()
            },
        }
    }
    pub fn restore(&mut self, entry: &Learned) {
        if entry.ceiling_mhz.is_finite() {
            self.ceiling = entry
                .ceiling_mhz
                .min(self.settings.max_frequency_mhz)
                .max(self.point.frequency_mhz);
        }
        self.status.cooldown_seconds = entry.cooldown_seconds.min(self.settings.cooldown_seconds);
        if let Some(retest) = &entry.retest {
            self.retest = RetestState {
                attempts: retest.attempts.min(16),
                remaining_seconds: retest.remaining_seconds.min(86400),
            };
        }
        self.status.retest_seconds_remaining = self.retest_remaining();
    }
    pub fn is_retest(&self) -> bool {
        self.trial
            .as_ref()
            .is_some_and(|trial| matches!(trial.kind, TrialKind::Retest))
    }
    pub fn retest_state(&self) -> RetestState {
        self.retest.clone()
    }
    pub fn retest_remaining(&self) -> Option<u64> {
        (self.settings.retest_after_seconds > 0
            && self.ceiling < self.settings.max_frequency_mhz
            && self
                .frequencies
                .iter()
                .any(|&f| f > self.ceiling && f <= self.settings.max_frequency_mhz))
        .then_some(self.retest.remaining_seconds)
    }
    fn retest_delay(&self) -> u64 {
        self.settings
            .retest_after_seconds
            .saturating_mul(1u64 << self.retest.attempts.min(16))
            .min(86400)
    }
    pub fn in_trial(&self) -> bool {
        self.trial.is_some()
    }
    pub fn learned_limits(&self) -> (f64, u64) {
        (self.ceiling, self.status.cooldown_seconds)
    }
    pub fn synchronize_recoveries(&mut self, count: u64) {
        self.recoveries = count;
    }
    pub fn applied(&mut self, point: Point, now: u64, reason: String) {
        self.point = point;
        self.recovery_burst.reset(now);
        self.epoch = now;
        self.last_tick = now;
        self.window_started = now;
        self.window_recoveries = 0;
        self.degraded_windows = 0;
        self.status.active_seconds = 0;
        self.status.state = if self.in_trial() {
            "testing"
        } else {
            "learning"
        }
        .into();
        self.status.message = reason;
    }
    fn backoff(&mut self, reason: &str, force_frequency: bool) -> Option<Action> {
        let target = if !force_frequency && self.point.voltage_v < 1.0 {
            Point {
                voltage_v: 1.0,
                ..self.point
            }
        } else if let Some(trial) = &self.trial
            && trial.rollback.frequency_mhz < self.point.frequency_mhz
        {
            trial.rollback
        } else {
            let frequency_mhz = self
                .frequencies
                .iter()
                .copied()
                .rev()
                .find(|&f| {
                    f <= self.point.frequency_mhz - 20. && f >= self.settings.min_frequency_mhz
                })
                .or_else(|| {
                    self.frequencies.iter().copied().rev().find(|&f| {
                        f < self.point.frequency_mhz && f >= self.settings.min_frequency_mhz
                    })
                })
                .unwrap_or(self.point.frequency_mhz);
            Point {
                frequency_mhz,
                ..self.point
            }
        };
        self.trial = None;
        self.degraded_windows = 0;
        self.window_recoveries = 0;
        self.status.cooldown_seconds = self.settings.cooldown_seconds;
        if target == self.point {
            self.status.state = "monitoring".into();
            self.status.message = format!("{reason}; already at the minimum frequency");
            return None;
        }
        if target.frequency_mhz < self.point.frequency_mhz {
            self.ceiling = self.point.frequency_mhz - 10.;
            self.retest.remaining_seconds = self.retest_delay();
        }
        self.status.retest_seconds_remaining = self.retest_remaining();
        self.baseline = None;
        Some(Action::Set(target, reason.into()))
    }
    /// Admission yields an experiment without changing its current hardware point.
    /// A fresh full observation is required before retaining learned evidence.
    pub fn interrupt_trial(&mut self, now: u64) {
        if self.trial.take().is_none() {
            return;
        }
        self.baseline = None;
        self.epoch = now;
        self.last_tick = now;
        self.window_started = now;
        self.window_recoveries = 0;
        self.degraded_windows = 0;
        self.status.active_seconds = 0;
    }
    pub fn observe(&mut self, now: u64, input: Input) -> Option<Action> {
        self.status.retest_seconds_remaining = self.retest_remaining();
        let clock_reversed = now < self.last_tick;
        let dt = now.saturating_sub(self.last_tick).min(5);
        self.last_tick = now;
        let reset_counter = input.recoveries < self.recoveries;
        let mut new_recoveries = input.recoveries.saturating_sub(self.recoveries);
        if reset_counter {
            self.recovery_burst.reset(now);
        }
        self.window_recoveries = self.window_recoveries.saturating_add(new_recoveries);
        self.recoveries = input.recoveries;
        if now < self.epoch || clock_reversed {
            self.recovery_burst.reset(now);
            self.epoch = now;
            self.window_started = now;
            self.window_recoveries = 0;
            self.degraded_windows = 0;
            self.status.active_seconds = 0;
            self.baseline = None;
            return None;
        }
        if input.paused {
            self.recovery_burst.reset(now);
            self.status.state = "paused".into();
            self.status.message = "Current settings held; proof production continues".into();
            self.was_paused = true;
            if !self.is_retest() {
                self.trial = None;
            }
            return None;
        }
        if self.was_paused {
            self.recovery_burst.reset(now);
            new_recoveries = 0;
            self.was_paused = false;
            self.epoch = now;
            self.window_started = now;
            self.window_recoveries = 0;
            self.degraded_windows = 0;
            self.status.active_seconds = 0;
            if !self.is_retest() {
                self.baseline = None;
            }
        }
        if self.recovery_burst.observe(now, new_recoveries) {
            self.recovery_burst.reset(now);
            return self.backoff("Recovery storm: 20 restarts within 10 seconds", true);
        }
        if !input.healthy {
            self.status.state = "waiting".into();
            self.status.message = "Waiting for fresh proof work and temperature telemetry".into();
            return None;
        }
        // Soft thermal backoff supplements the independent hard temperature stop.
        if input
            .temperature
            .is_some_and(|t| t >= input.temperature_limit - 1.)
        {
            return self.backoff("Reduced frequency near the temperature limit", true);
        }
        self.status.active_seconds += dt;
        self.status.cooldown_seconds = self.status.cooldown_seconds.saturating_sub(dt);
        self.status.state = if self.in_trial() {
            "testing"
        } else {
            "learning"
        }
        .into();
        let score = input.score.filter(|s| s.is_finite() && *s > 0.)?;
        if let Some(baseline) = self.baseline
            && now.saturating_sub(self.window_started) >= self.settings.recovery_window_seconds
        {
            let degraded = self.window_recoveries > 0
                && score < baseline * (1. - self.settings.recovery_loss_percent / 100.);
            self.degraded_windows = if degraded {
                self.degraded_windows + 1
            } else {
                0
            };
            self.window_started = now;
            self.window_recoveries = 0;
            if self.degraded_windows >= self.settings.recovery_bad_windows {
                return self.backoff("Sustained delivered IPS loss with recoveries in consecutive observation windows",false);
            }
        }
        let required_seconds = if self.is_retest() {
            self.settings.healthy_seconds.max(
                self.settings.recovery_window_seconds
                    * u64::from(self.settings.recovery_bad_windows),
            )
        } else {
            self.settings.healthy_seconds
        };
        if self.status.active_seconds < required_seconds {
            return None;
        }
        if self.degraded_windows > 0 {
            self.status.message = "Waiting for a complete recovery-cost observation window".into();
            return None;
        }
        if self.in_trial() {
            // No rollback merely because an otherwise stable trial missed a tiny gain.
            // A materially slower recovering trial stays under observation until the
            // consecutive-window test above can establish a problem.
            if self
                .baseline
                .is_some_and(|b| score < b * (1. - self.settings.recovery_loss_percent / 100.))
                && self.window_recoveries > 0
            {
                self.status.message =
                    "Observing recovery cost before accepting this setting".into();
                return None;
            }
            let was_retest = self.is_retest();
            self.trial = None;
            if was_retest {
                self.ceiling = self.settings.max_frequency_mhz;
                self.retest = RetestState {
                    attempts: 0,
                    remaining_seconds: self.settings.retest_after_seconds,
                };
                self.status.retest_seconds_remaining = None;
            }
            self.baseline = Some(score);
            self.epoch = now;
            self.window_started = now;
            self.window_recoveries = 0;
            self.degraded_windows = 0;
            self.status.active_seconds = 0;
            self.status.message =
                "Retained higher frequency; continuing to monitor delivered IPS".into();
            return Some(Action::Save);
        }
        if self.baseline.is_none() {
            self.baseline = Some(score);
            self.window_started = now;
            self.window_recoveries = 0;
            self.degraded_windows = 0;
            self.status.message =
                "Saved observed performance baseline; recoveries are permitted".into();
            return Some(Action::Save);
        }
        if !input.exploration_allowed {
            self.status.state = "monitoring".into();
            self.status.message =
                "Holding settings while another board recovers or is being evaluated".into();
            return None;
        }
        if self.retest_remaining().is_some()
            && input
                .temperature
                .is_some_and(|t| t <= input.temperature_limit - 5.)
        {
            self.retest.remaining_seconds = self.retest.remaining_seconds.saturating_sub(dt);
            self.status.retest_seconds_remaining = self.retest_remaining();
        }
        if self.status.cooldown_seconds > 0 {
            self.status.state = "cooldown".into();
            return None;
        }
        if input
            .temperature
            .is_none_or(|t| t >= input.temperature_limit - 3.)
        {
            self.status.state = "monitoring".into();
            return None;
        }
        if let Some(frequency_mhz) = self
            .frequencies
            .iter()
            .copied()
            .find(|&f| f >= self.point.frequency_mhz + 10. - 0.000001 && f <= self.ceiling)
        {
            self.trial = Some(Trial {
                rollback: self.point,
                kind: TrialKind::Exploration,
            });
            return Some(Action::Set(
                Point {
                    frequency_mhz,
                    ..self.point
                },
                format!("Testing supported frequency {frequency_mhz:.0} MHz"),
            ));
        }
        if self.retest_remaining() == Some(0)
            && input
                .temperature
                .is_some_and(|t| t <= input.temperature_limit - 5.)
            && let Some(frequency_mhz) = self.frequencies.iter().copied().find(|&f| {
                f >= self.point.frequency_mhz + 10. - 0.000001
                    && f <= self.settings.max_frequency_mhz
            })
        {
            self.trial = Some(Trial {
                rollback: self.point,
                kind: TrialKind::Retest,
            });
            self.retest.attempts = self.retest.attempts.saturating_add(1).min(16);
            // Reserve the next wait before touching hardware. A crash or failed
            // command cannot turn a restart into an immediate repeated test.
            self.retest.remaining_seconds = self.retest_delay();
            self.status.retest_seconds_remaining = self.retest_remaining();
            return Some(Action::Set(
                Point {
                    frequency_mhz,
                    ..self.point
                },
                format!(
                    "Retesting supported frequency {frequency_mhz:.0} MHz after healthy operation"
                ),
            ));
        }
        self.status.state = "monitoring".into();
        self.status.message = match self.retest_remaining() {
            Some(seconds) => format!(
                "Monitoring; higher frequency retry after {} more healthy minutes",
                seconds.div_ceil(60)
            ),
            None => {
                "Holding frequency and voltage; monitoring delivered IPS and recovery cost".into()
            }
        };
        None
    }
}

pub fn phase_score(
    observations: &[crate::runtime::Observation],
    chains: &[timelord_core::Chain],
    epoch: u64,
    minimum: usize,
) -> Option<f64> {
    let mut score = f64::INFINITY;
    for chain in chains {
        let values: Vec<_> = observations
            .iter()
            .rev()
            .take_while(|o| o.timestamp_unix >= epoch)
            .filter(|o| {
                o.chain == *chain
                    && o.work_started_unix >= epoch.saturating_add(10)
                    && o.ips.is_finite()
                    && o.ips > 0.
            })
            .map(|o| o.ips)
            .collect();
        if values.len() < minimum {
            return None;
        }
        score = score.min(values.iter().sum::<f64>() / values.len() as f64);
    }
    score.is_finite().then_some(score)
}

#[cfg(test)]
mod tests;

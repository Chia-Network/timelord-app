//! Per-serial reliability. It decides which chain a working board carries,
//! never whether a working board is used. Worker restarts and profile switches
//! never reset the record.
use serde::{Deserialize, Serialize};

/// Probation length after the n-th consecutive failure.
const PROBATION_SECONDS: [u64; 4] = [60, 300, 1800, 7200];
/// Continuous healthy serving after probation that clears the failure streak.
pub const STABLE_SECONDS: u64 = 1800;
/// Stability needs current evidence, not merely an open process.
pub const EVIDENCE_SECONDS: u64 = 300;
const RECENT_LIMIT: usize = 32;
const RECENT_WINDOW: u64 = 86400;
const REASON_LIMIT: usize = 240;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Preferred for the challenge and reward chains.
    Trusted,
    /// Failed recently: still carries work, preferring the infused-challenge
    /// chain while a more reliable board carries the critical chains.
    Probation,
    /// Operator disabled.
    Disabled,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Health {
    pub consecutive_failures: u32,
    pub recent_failures: Vec<u64>,
    pub last_failure_unix: u64,
    pub last_failure: Option<String>,
    pub probation_until_unix: u64,
    /// `Some(u64::MAX)` disables until an operator enables the board.
    pub disabled_until_unix: Option<u64>,
}

impl Health {
    pub fn validate(&self) -> bool {
        self.recent_failures.len() <= RECENT_LIMIT
            && self
                .last_failure
                .as_ref()
                .is_none_or(|r| r.len() <= REASON_LIMIT * 4)
    }
    pub fn probation_seconds(consecutive: u32) -> u64 {
        match consecutive {
            0 => 0,
            1 => PROBATION_SECONDS[0],
            2 => PROBATION_SECONDS[1],
            3..=5 => PROBATION_SECONDS[2],
            _ => PROBATION_SECONDS[3],
        }
    }
    pub fn record_failure(&mut self, now: u64, reason: &str) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.recent_failures
            .retain(|&t| now.saturating_sub(t) < RECENT_WINDOW && t <= now);
        if self.recent_failures.len() == RECENT_LIMIT {
            self.recent_failures.remove(0);
        }
        self.recent_failures.push(now);
        self.last_failure_unix = now;
        self.last_failure = Some(reason.chars().take(REASON_LIMIT).collect());
        self.probation_until_unix = self
            .probation_until_unix
            .max(now.saturating_add(Self::probation_seconds(self.consecutive_failures)));
    }
    pub fn disabled(&self, now: u64) -> bool {
        self.disabled_until_unix.is_some_and(|until| until > now)
    }
    pub fn tier(&self, now: u64) -> Tier {
        if self.disabled(now) {
            Tier::Disabled
        } else if now < self.probation_until_unix {
            Tier::Probation
        } else {
            Tier::Trusted
        }
    }
    pub fn failures_within(&self, now: u64, seconds: u64) -> usize {
        self.recent_failures
            .iter()
            .filter(|&&t| t <= now && now - t < seconds)
            .count()
    }
    /// A board that has served continuously since probation ended, with fresh
    /// verified evidence, has cleared its failure streak.
    pub fn stable(&self, now: u64, serving_since: u64, evidence_unix: u64) -> bool {
        self.consecutive_failures > 0
            && now >= self.probation_until_unix
            && serving_since > 0
            && now.saturating_sub(serving_since.max(self.probation_until_unix)) >= STABLE_SECONDS
            && evidence_unix <= now
            && now - evidence_unix <= EVIDENCE_SECONDS
    }
    pub fn clear_streak(&mut self) {
        self.consecutive_failures = 0;
    }
    /// Operator retry: the operator judges the board fine. End probation and
    /// the failure streak now; recent failures stay in the history.
    pub fn retry_now(&mut self, now: u64) {
        self.probation_until_unix = self.probation_until_unix.min(now);
        self.consecutive_failures = 0;
    }
    pub fn disable(&mut self, until: Option<u64>) {
        self.disabled_until_unix = Some(until.unwrap_or(u64::MAX));
    }
    pub fn enable(&mut self) {
        self.disabled_until_unix = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_failures_grow_probation() {
        let mut health = Health::default();
        assert_eq!(health.tier(0), Tier::Trusted);
        health.record_failure(100, "lane timed out");
        assert_eq!(health.tier(100), Tier::Probation);
        assert_eq!(health.tier(160), Tier::Trusted);
        health.record_failure(200, "lane timed out");
        assert_eq!(health.probation_until_unix, 500);
        health.record_failure(300, "lane timed out");
        assert_eq!(health.tier(301), Tier::Probation);
        assert_eq!(health.probation_until_unix, 2100);
        for t in 0..10 {
            health.record_failure(400 + t, "again");
        }
        assert_eq!(health.probation_until_unix, 409 + 7200);
        assert_eq!(health.failures_within(410, 3600), 13);
    }
    #[test]
    fn stability_requires_post_probation_serving_and_fresh_evidence() {
        let mut health = Health::default();
        health.record_failure(1000, "x");
        let end = health.probation_until_unix;
        assert!(!health.stable(end + STABLE_SECONDS - 1, 1000, end + STABLE_SECONDS - 2));
        assert!(health.stable(end + STABLE_SECONDS, 1000, end + STABLE_SECONDS - 1));
        // Serving began after probation: measure from serving start.
        assert!(!health.stable(end + STABLE_SECONDS, end + 10, end + STABLE_SECONDS));
        // Old evidence does not establish current stability.
        assert!(!health.stable(
            end + STABLE_SECONDS,
            1000,
            end + STABLE_SECONDS - EVIDENCE_SECONDS - 1
        ));
        health.clear_streak();
        assert!(!health.stable(end + 10 * STABLE_SECONDS, 1000, end + 10 * STABLE_SECONDS));
    }
    #[test]
    fn operator_controls_override_tiers_without_erasing_history() {
        let mut health = Health::default();
        for t in 0..3 {
            health.record_failure(100 + t, "x");
        }
        assert_eq!(health.tier(110), Tier::Probation);
        health.retry_now(110);
        assert_eq!(health.tier(110), Tier::Trusted);
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.failures_within(110, 3600), 3);
        // After a retry, the next failure starts a new streak.
        health.record_failure(111, "x");
        assert_eq!(health.consecutive_failures, 1);
        assert_eq!(health.tier(112), Tier::Probation);
        health.retry_now(112);
        health.disable(Some(200));
        assert_eq!(health.tier(150), Tier::Disabled);
        assert_eq!(health.tier(200), Tier::Trusted);
        health.disable(None);
        assert_eq!(health.tier(u64::MAX - 1), Tier::Disabled);
        health.enable();
        assert_eq!(health.tier(150), Tier::Trusted);
    }
    #[test]
    fn failure_records_are_bounded() {
        let mut health = Health::default();
        for t in 0..100 {
            health.record_failure(1000 + t, &"x".repeat(1000));
        }
        assert_eq!(health.recent_failures.len(), RECENT_LIMIT);
        assert_eq!(health.last_failure.as_ref().unwrap().len(), REASON_LIMIT);
        assert!(health.validate());
        // Records older than a day expire on the next failure.
        health.record_failure(1000 + 2 * RECENT_WINDOW, "late");
        assert_eq!(health.recent_failures.len(), 1);
    }
}

//! Shared agreement window for physical topology and usable capacity changes.
use std::time::{Duration, Instant};

pub(crate) struct StableChange<T> {
    pending: Option<(T, Instant, u8)>,
}
impl<T> Default for StableChange<T> {
    fn default() -> Self {
        Self { pending: None }
    }
}
impl<T: PartialEq> StableChange<T> {
    pub fn reset(&mut self) {
        self.pending = None;
    }
    pub fn observe_at(&mut self, current: &T, next: T, now: Instant) -> bool {
        if &next == current {
            self.reset();
            return false;
        }
        match &mut self.pending {
            Some((pending, _, count)) if *pending == next => *count = count.saturating_add(1),
            _ => self.pending = Some((next, now, 1)),
        }
        let (_, since, count) = self.pending.as_ref().expect("pending candidate");
        if *count < 3 || now.saturating_duration_since(*since) < Duration::from_secs(10) {
            return false;
        }
        self.reset();
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn agreement_needs_three_polls_and_time_and_resets_after_failure_or_no_change() {
        let now = Instant::now();
        let mut change = StableChange::default();
        assert!(!change.observe_at(&0, 1, now));
        assert!(!change.observe_at(&0, 1, now + Duration::from_secs(5)));
        assert!(change.observe_at(&0, 1, now + Duration::from_secs(10)));
        assert!(!change.observe_at(&0, 1, now + Duration::from_secs(11)));
        change.reset();
        assert!(!change.observe_at(&0, 1, now + Duration::from_secs(20)));
        assert!(!change.observe_at(&0, 1, now + Duration::from_secs(25)));
        assert!(!change.observe_at(&0, 0, now + Duration::from_secs(30)));
        assert!(!change.observe_at(&0, 1, now + Duration::from_secs(40)));
        assert!(!change.observe_at(&0, 2, now + Duration::from_secs(45)));
        assert!(!change.observe_at(&0, 2, now + Duration::from_secs(50)));
        assert!(change.observe_at(&0, 2, now + Duration::from_secs(55)));
    }
}

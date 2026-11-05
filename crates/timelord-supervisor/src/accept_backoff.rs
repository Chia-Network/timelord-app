//! Shared retry pacing for long-lived accept loops.

use std::{collections::VecDeque, future::Future, io, time::Duration};
use tokio::time::{Instant, sleep_until};

const INITIAL_DELAY: Duration = Duration::from_millis(50);
const MAX_DELAY: Duration = Duration::from_secs(1);

/// Consecutive accept failures double the retry delay from 50ms up to 1s.
/// A successful accept resets the sequence and any pending retry deadline.
#[derive(Debug, Default)]
pub struct AcceptBackoff {
    failures: u32,
    retry_at: Option<Instant>,
    injected: VecDeque<io::Error>,
}

impl AcceptBackoff {
    pub const fn new() -> Self {
        Self {
            failures: 0,
            retry_at: None,
            injected: VecDeque::new(),
        }
    }

    /// Test-only constructor that fails the next accepts before the listener is
    /// polled. Production callers must use [`Self::new`].
    pub fn with_injected(errors: impl IntoIterator<Item = io::Error>) -> Self {
        Self {
            injected: errors.into_iter().collect(),
            ..Self::new()
        }
    }

    /// Wait for any pending retry deadline, then accept.
    ///
    /// A failure is recorded, reported once through `on_error`, and the next
    /// retry deadline is stored before returning `None`. A success resets the
    /// sequence and returns the accepted value. This is cancel-safe inside a
    /// `tokio::select!`: dropping the future leaves the retry deadline intact,
    /// so a shutdown branch can win while the listener is backing off.
    pub async fn accept<T>(
        &mut self,
        accept: impl Future<Output = io::Result<T>>,
        mut on_error: impl FnMut(&io::Error, u32, Duration),
    ) -> Option<T> {
        if let Some(retry_at) = self.retry_at {
            sleep_until(retry_at).await;
        }
        if let Some(error) = self.injected.pop_front() {
            self.fail(&error, &mut on_error);
            return None;
        }
        match accept.await {
            Ok(value) => {
                self.succeeded();
                Some(value)
            }
            Err(error) => {
                self.fail(&error, &mut on_error);
                None
            }
        }
    }

    fn fail(&mut self, error: &io::Error, on_error: &mut impl FnMut(&io::Error, u32, Duration)) {
        let delay = self.failed();
        self.retry_at = Some(Instant::now() + delay);
        on_error(error, self.failures, delay);
    }

    /// Record a failed accept and return the delay before the next attempt.
    pub fn failed(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        self.delay()
    }

    /// Record a successful accept, resetting the retry sequence.
    pub fn succeeded(&mut self) {
        self.failures = 0;
        self.retry_at = None;
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Delay for the current number of consecutive failures.
    pub fn delay(&self) -> Duration {
        let exponent = self.failures.saturating_sub(1).min(5);
        INITIAL_DELAY
            .saturating_mul(1u32 << exponent)
            .min(MAX_DELAY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_doubles_to_one_second_and_resets() {
        let mut backoff = AcceptBackoff::new();
        let expected = [50, 100, 200, 400, 800, 1_000, 1_000];
        for milliseconds in expected {
            assert_eq!(backoff.failed(), Duration::from_millis(milliseconds));
        }
        backoff.succeeded();
        assert_eq!(backoff.failures(), 0);
        assert_eq!(backoff.failed(), INITIAL_DELAY);
    }
}

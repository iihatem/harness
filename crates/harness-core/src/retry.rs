use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::provider::ProviderError;

/// A server-requested `Retry-After` beyond this is not worth waiting for automatically; the turn
/// fails instead of blocking the session for that long.
pub const MAX_AUTOMATIC_RETRY_AFTER: Duration = Duration::from_secs(60);

/// How transient provider errors (network, 429, 5xx) are retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts including the first.
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Whether a call whose attempt number `attempt` (1-based) failed with `error` is tried
    /// again: an error worth retrying, while attempts remain. A reply that did not start within
    /// its wait (300 s from a hosted provider) is tried once more at most, since each try waits
    /// that long.
    pub fn retries(&self, error: &ProviderError, attempt: u32) -> bool {
        let attempts = match error {
            ProviderError::NoStart { .. } => self.max_attempts.min(2),
            _ => self.max_attempts,
        };
        error.is_retryable() && attempt < attempts
    }

    /// Delay before retry number `attempt` (1-based). A server-provided `Retry-After` wins; otherwise
    /// exponential backoff capped at `max_delay`, plus up to `base_delay` of jitter.
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(requested) = retry_after {
            return requested;
        }
        let exponent = attempt.saturating_sub(1).min(16);
        let backoff = self
            .base_delay
            .saturating_mul(1u32 << exponent)
            .min(self.max_delay);
        backoff + jitter(self.base_delay)
    }
}

fn jitter(max: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    let span = (max.as_millis() as u64).max(1);
    Duration::from_millis(nanos % span)
}

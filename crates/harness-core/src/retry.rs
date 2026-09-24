use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

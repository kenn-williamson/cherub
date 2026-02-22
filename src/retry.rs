//! Retry logic with exponential backoff for transient API errors.
//!
//! Hand-rolled, no external dependencies. Jitter uses `SystemTime` nanoseconds
//! to avoid adding `rand` as a non-optional dependency.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Configuration for retry behavior.
pub struct RetryConfig {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl RetryConfig {
    pub fn new() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// Classification of an HTTP response for retry decisions.
pub enum RetryVerdict {
    /// Request succeeded (2xx).
    Success,
    /// Transient failure — safe to retry. Optional `Retry-After` hint.
    Transient(Option<Duration>),
    /// Permanent failure — do not retry (4xx client errors).
    Permanent,
}

/// Classify an HTTP status code into a retry verdict.
pub fn classify_status(status: u16) -> RetryVerdict {
    match status {
        200..=299 => RetryVerdict::Success,
        429 => RetryVerdict::Transient(None),
        500..=599 => RetryVerdict::Transient(None),
        _ => RetryVerdict::Permanent,
    }
}

/// Compute the delay before the next retry attempt.
///
/// Uses exponential backoff with jitter: `base * 2^attempt + jitter_ms`.
/// Jitter is derived from `SystemTime` nanoseconds (0–999 ms) to avoid
/// thundering-herd effects without requiring the `rand` crate.
pub fn compute_delay(config: &RetryConfig, attempt: u32) -> Duration {
    let exp = config.base_delay.saturating_mul(1u32.wrapping_shl(attempt));
    let capped = exp.min(config.max_delay);

    let jitter_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .subsec_nanos()
        % 1000;

    let with_jitter = capped + Duration::from_millis(u64::from(jitter_ms));
    with_jitter.min(config.max_delay + Duration::from_millis(999))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_success_codes() {
        assert!(matches!(classify_status(200), RetryVerdict::Success));
        assert!(matches!(classify_status(201), RetryVerdict::Success));
        assert!(matches!(classify_status(299), RetryVerdict::Success));
    }

    #[test]
    fn classify_rate_limit() {
        assert!(matches!(classify_status(429), RetryVerdict::Transient(_)));
    }

    #[test]
    fn classify_server_errors() {
        assert!(matches!(classify_status(500), RetryVerdict::Transient(_)));
        assert!(matches!(classify_status(502), RetryVerdict::Transient(_)));
        assert!(matches!(classify_status(503), RetryVerdict::Transient(_)));
    }

    #[test]
    fn classify_client_errors() {
        assert!(matches!(classify_status(400), RetryVerdict::Permanent));
        assert!(matches!(classify_status(401), RetryVerdict::Permanent));
        assert!(matches!(classify_status(403), RetryVerdict::Permanent));
        assert!(matches!(classify_status(404), RetryVerdict::Permanent));
    }

    #[test]
    fn delay_grows_exponentially() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
        };
        let d0 = compute_delay(&config, 0);
        let d1 = compute_delay(&config, 1);
        let d2 = compute_delay(&config, 2);
        // Base delays: 1s, 2s, 4s (before jitter)
        // With up to 999ms jitter, d0 < 2s, d1 < 3s, d2 < 5s
        assert!(d0 < Duration::from_secs(2));
        assert!(d1 >= Duration::from_secs(1)); // 2s base - jitter varies
        assert!(d2 >= Duration::from_secs(2)); // 4s base - jitter varies
    }

    #[test]
    fn delay_caps_at_max() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
        };
        let d10 = compute_delay(&config, 10);
        // max_delay(5s) + max_jitter(999ms)
        assert!(d10 <= Duration::from_millis(5999));
    }

    #[test]
    fn jitter_is_nonzero_most_of_the_time() {
        // Jitter is subsec nanos % 1000 — extremely unlikely to be exactly 0
        // on all 10 calls unless the clock has zero-resolution subsec nanos.
        let config = RetryConfig::new();
        let delays: Vec<_> = (0..10).map(|_| compute_delay(&config, 0)).collect();
        let all_exactly_base = delays.iter().all(|d| *d == Duration::from_secs(1));
        // Not a hard assert — just a statistical check. On a real system with
        // nanosecond resolution this will always pass.
        assert!(!all_exactly_base || cfg!(miri));
    }
}

use std::collections::HashSet;
use std::time::Duration;

use crate::constants;
use crate::error::{Aria2Error, RecoverableError};

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum total attempts. `0` means unlimited, matching aria2's
    /// `--max-tries` contract. The field name is retained for source
    /// compatibility with existing internal callers.
    pub max_retries: u32,
    pub base_wait_ms: u64,
    pub max_wait_ms: u64,
    pub backoff_factor: f64,
    pub retryable_http_codes: HashSet<u16>,
    pub max_retries_per_server: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        let codes: HashSet<u16> = constants::RETRYABLE_HTTP_CODES.iter().copied().collect();
        Self {
            max_retries: constants::DEFAULT_MAX_RETRIES,
            base_wait_ms: 1000,
            max_wait_ms: 30000,
            backoff_factor: 2.0,
            retryable_http_codes: codes,
            max_retries_per_server: u32::MAX,
        }
    }
}

impl RetryPolicy {
    #[allow(clippy::field_reassign_with_default)]
    pub fn new(max_retries: u32, base_wait_ms: u64) -> Self {
        let mut policy = Self::default();
        policy.max_retries = max_retries;
        policy.base_wait_ms = base_wait_ms;
        policy
    }

    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    pub fn with_max_per_server(mut self, n: u32) -> Self {
        self.max_retries_per_server = n;
        self
    }

    pub fn with_max_wait_ms(mut self, ms: u64) -> Self {
        self.max_wait_ms = ms;
        self
    }

    /// Return the configured maximum total attempts.
    pub fn max_tries(&self) -> u32 {
        self.max_retries
    }

    /// Return whether another attempt is allowed after `attempts` attempts
    /// have already been made. `0` is the public unlimited value.
    pub fn can_retry_after(&self, attempts: u32) -> bool {
        self.max_retries == 0 || attempts < self.max_retries
    }

    pub fn compute_wait(&self, attempt: u32) -> Option<Duration> {
        if attempt == 0 {
            return None;
        }
        Some(self.backoff_duration(attempt.saturating_sub(1)))
    }

    /// Compute wait duration using exponential backoff (Duration-based API).
    ///
    /// This is the direct-duration form of [`compute_wait`](Self::compute_wait)
    /// for callers that need the initial wait as `attempt = 0`.
    pub fn wait_duration(&self, attempt: u32) -> Duration {
        self.backoff_duration(attempt)
    }

    fn backoff_duration(&self, exponent: u32) -> Duration {
        let raw = (self.base_wait_ms as f64) * self.backoff_factor.powi(exponent.min(20) as i32);
        let millis = raw.max(0.0).min(self.max_wait_ms as f64) as u64;
        Duration::from_millis(millis)
    }

    /// Check whether a retry should be attempted after a zero-based attempt.
    ///
    /// `attempt` is the index of the failed attempt, so attempt `0` is the
    /// first request. The policy stores total attempts; `0` means unlimited.
    pub fn should_retry(&self, attempt: u32, error: &Aria2Error) -> bool {
        self.can_retry_after(attempt.saturating_add(1)) && self.is_retryable_error(error)
    }

    fn is_retryable_error(&self, error: &Aria2Error) -> bool {
        match error {
            Aria2Error::Recoverable(RecoverableError::Timeout)
            | Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. }) => true,
            Aria2Error::Recoverable(RecoverableError::ServerError { code }) => {
                self.should_retry_http(*code)
            }
            _ => false,
        }
    }

    pub fn should_retry_http(&self, status_code: u16) -> bool {
        self.retryable_http_codes.contains(&status_code)
    }

    pub fn should_retry_error(&self, error_str: &str) -> bool {
        let lower = error_str.to_lowercase();
        lower.contains("timeout")
            || lower.contains("connection reset")
            || lower.contains("connection refused")
            || lower.contains("broken pipe")
            || lower.contains("timed out")
            || lower.contains("eof")
            || lower.contains("network")
            || lower.contains("dns")
            || lower.contains("socket")
            || lower.contains("unreachable")
            || lower.contains("reset by peer")
            || lower.contains("temporary")
            || lower.contains("try again")
    }

    /// Return whether `attempts` completed attempts have exhausted the limit.
    /// Unlimited policies are never exhausted.
    pub fn is_exhausted(&self, attempts: u32) -> bool {
        self.max_retries != 0 && attempts >= self.max_retries
    }

    pub fn total_estimated_wait_sec(&self) -> f64 {
        if self.max_retries == 0 {
            return f64::INFINITY;
        }

        let mut total = 0.0f64;
        for a in 1..self.max_retries {
            total += (self.base_wait_ms as f64) * self.backoff_factor.powi(a as i32 - 1);
        }
        total / 1000.0
    }

    pub fn stats(&self) -> RetryPolicyStats {
        RetryPolicyStats {
            max_retries: self.max_retries,
            retryable_codes_count: self.retryable_http_codes.len(),
            estimated_max_total_wait_sec: self.total_estimated_wait_sec(),
        }
    }
}

pub struct RetryPolicyStats {
    pub max_retries: u32,
    pub retryable_codes_count: usize,
    pub estimated_max_total_wait_sec: f64,
}

#[derive(Debug, Clone)]
pub struct AttemptRecord {
    pub attempt_number: u32,
    pub started_at: std::time::Instant,
    pub error: Option<String>,
    pub duration: Duration,
}

impl AttemptRecord {
    pub fn new(attempt_number: u32) -> Self {
        Self {
            attempt_number,
            started_at: std::time::Instant::now(),
            error: None,
            duration: Duration::ZERO,
        }
    }

    pub fn finish(mut self, error: Option<String>) -> Self {
        self.duration = self.started_at.elapsed();
        self.error = error;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_policy_values() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_retries, 5);
        assert_eq!(p.base_wait_ms, 1000);
        assert_eq!(p.max_wait_ms, 30000);
        assert!((p.backoff_factor - 2.0).abs() < 0.001);
        assert!(p.retryable_http_codes.contains(&408));
        assert!(p.retryable_http_codes.contains(&503));
        assert_eq!(p.retryable_http_codes.len(), 6);
    }

    #[test]
    fn test_compute_wait_exponential() {
        let p = RetryPolicy::default();
        assert_eq!(p.compute_wait(0), None);
        assert_eq!(p.compute_wait(1), Some(Duration::from_millis(1000)));
        assert_eq!(p.compute_wait(2), Some(Duration::from_millis(2000)));
        assert_eq!(p.compute_wait(3), Some(Duration::from_millis(4000)));
        assert_eq!(p.compute_wait(4), Some(Duration::from_millis(8000)));
    }

    #[test]
    fn test_compute_wait_capped_at_max() {
        let p = RetryPolicy::default();
        let w5 = p.compute_wait(5).unwrap().as_millis();
        assert!(
            w5 <= p.max_wait_ms as u128,
            "wait={} should be capped at {}",
            w5,
            p.max_wait_ms
        );
    }

    #[test]
    fn test_compute_wait_zero_attempts() {
        let p = RetryPolicy::default();
        assert_eq!(p.compute_wait(0), None, "attempt 0 should not wait");
    }

    #[test]
    fn test_wait_duration_preserves_millisecond_precision_and_backoff_factor() {
        let mut p = RetryPolicy::new(3, 5);
        p.backoff_factor = 3.0;

        assert_eq!(p.wait_duration(0), Duration::from_millis(5));
        assert_eq!(p.wait_duration(1), Duration::from_millis(15));
        assert_eq!(p.wait_duration(2), Duration::from_millis(45));
        assert_eq!(p.compute_wait(1), Some(Duration::from_millis(5)));
        assert_eq!(p.compute_wait(2), Some(Duration::from_millis(15)));
    }

    #[test]
    fn test_should_retry_http_408_true() {
        let p = RetryPolicy::default();
        assert!(
            p.should_retry_http(408),
            "Request Timeout should be retryable"
        );
        assert!(
            p.should_retry_http(429),
            "Too Many Requests should be retryable"
        );
        assert!(
            p.should_retry_http(500),
            "Internal Server Error should be retryable"
        );
        assert!(
            p.should_retry_http(503),
            "Service Unavailable should be retryable"
        );
    }

    #[test]
    fn test_should_retry_http_404_false() {
        let p = RetryPolicy::default();
        assert!(
            !p.should_retry_http(404),
            "Not Found should NOT be retryable"
        );
        assert!(
            !p.should_retry_http(403),
            "Forbidden should NOT be retryable"
        );
        assert!(
            !p.should_retry_http(400),
            "Bad Request should NOT be retryable"
        );
    }

    #[test]
    fn test_should_retry_network_error_true() {
        let p = RetryPolicy::default();
        assert!(p.should_retry_error("connection reset by peer"));
        assert!(p.should_retry_error("operation timed out"));
        assert!(p.should_retry_error("DNS resolution failed"));
        assert!(p.should_retry_error("broken pipe"));
        assert!(p.should_retry_error("network is unreachable"));
    }

    #[test]
    fn test_should_retry_non_network_false() {
        let p = RetryPolicy::default();
        assert!(!p.should_retry_error("file not found"));
        assert!(!p.should_retry_error("permission denied"));
        assert!(!p.should_retry_error("invalid URL"));
        assert!(!p.should_retry_error("HTTP 404 Not Found"));
    }

    #[test]
    fn test_is_exhausted_false_under_limit() {
        let p = RetryPolicy::with_max_retries(RetryPolicy::default(), 3);
        assert!(!p.is_exhausted(0));
        assert!(!p.is_exhausted(1));
        assert!(!p.is_exhausted(2));
        assert!(p.is_exhausted(3));
    }

    #[test]
    fn test_is_exhausted_true_at_limit() {
        let p = RetryPolicy::with_max_retries(RetryPolicy::default(), 3);
        assert!(
            p.is_exhausted(3),
            "three completed attempts should exhaust max=3"
        );
    }

    #[test]
    fn test_zero_max_tries_is_unlimited() {
        let p = RetryPolicy::new(0, 0);
        assert!(!p.is_exhausted(u32::MAX));
        assert!(p.can_retry_after(u32::MAX));
        assert!(p.should_retry(
            u32::MAX,
            &Aria2Error::Recoverable(crate::error::RecoverableError::Timeout)
        ));
        assert!(p.total_estimated_wait_sec().is_infinite());
    }

    #[test]
    fn test_should_retry_only_allows_transient_errors_and_configured_statuses() {
        let p = RetryPolicy::default();

        assert!(p.should_retry(0, &Aria2Error::Recoverable(RecoverableError::Timeout)));
        assert!(p.should_retry(
            0,
            &Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "connection reset".to_string(),
            })
        ));

        for code in [408, 429, 500, 502, 503, 504] {
            assert!(
                p.should_retry(
                    0,
                    &Aria2Error::Recoverable(RecoverableError::ServerError { code })
                ),
                "HTTP status {code} should be retryable"
            );
        }

        for error in [
            Aria2Error::Recoverable(RecoverableError::ServerError { code: 404 }),
            Aria2Error::Recoverable(RecoverableError::ServerError { code: 501 }),
            Aria2Error::Recoverable(RecoverableError::RangeNotSatisfiable {
                range: "bytes=10-20".to_string(),
            }),
            Aria2Error::Recoverable(RecoverableError::CannotResume),
            Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
                message: "redirect missing Location".to_string(),
            }),
            Aria2Error::Recoverable(RecoverableError::HttpAuthFailed {
                message: "HTTP 401".to_string(),
            }),
            Aria2Error::Recoverable(RecoverableError::HttpTooManyRedirects { count: 20 }),
            Aria2Error::Recoverable(RecoverableError::ResourceNotFound),
            Aria2Error::Recoverable(RecoverableError::MaxFileNotFound),
        ] {
            assert!(
                !p.should_retry(0, &error),
                "error should not be retried: {error}"
            );
        }
    }

    #[test]
    fn test_should_retry_honors_custom_http_status_allowlist() {
        let mut p = RetryPolicy::new(3, 0);
        p.retryable_http_codes.clear();
        p.retryable_http_codes.insert(418);

        assert!(p.should_retry(
            0,
            &Aria2Error::Recoverable(RecoverableError::ServerError { code: 418 })
        ));
        assert!(!p.should_retry(
            0,
            &Aria2Error::Recoverable(RecoverableError::ServerError { code: 503 })
        ));
    }

    #[test]
    fn test_max_tries_is_total_attempts() {
        let p = RetryPolicy::new(1, 0);
        let error = Aria2Error::Recoverable(crate::error::RecoverableError::Timeout);
        assert!(!p.should_retry(0, &error));

        let p = RetryPolicy::new(3, 0);
        assert!(p.should_retry(0, &error));
        assert!(p.should_retry(1, &error));
        assert!(!p.should_retry(2, &error));
    }

    #[test]
    fn test_stats_reasonable_values() {
        let p = RetryPolicy::default();
        let s = p.stats();
        assert_eq!(s.max_retries, 5);
        assert_eq!(s.retryable_codes_count, 6);
        assert!(s.estimated_max_total_wait_sec > 0.0);
        assert!(s.estimated_max_total_wait_sec < 30.0 + 10.0);
    }

    #[test]
    fn test_custom_policy_override() {
        let p = RetryPolicy::new(5, 2000).with_max_retries(10);
        assert_eq!(p.max_retries, 10);
        assert_eq!(p.base_wait_ms, 2000);

        let w = p.compute_wait(1).unwrap();
        assert_eq!(w, Duration::from_millis(2000));

        assert!(!p.is_exhausted(9));
        assert!(p.is_exhausted(11));
    }

    #[test]
    fn test_attempt_record_lifecycle() {
        let rec = AttemptRecord::new(2);
        assert_eq!(rec.attempt_number, 2);
        assert!(rec.error.is_none());
        assert_eq!(rec.duration, Duration::ZERO);

        let finished = rec.finish(Some("timeout".to_string()));
        assert_eq!(finished.error.as_deref().unwrap(), "timeout");
        // Just verify duration field exists (u128 is always >= 0)
        let _ = finished.duration;
    }
}

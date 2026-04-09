use std::collections::HashSet;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_wait_ms: u64,
    pub max_wait_ms: u64,
    pub backoff_factor: f64,
    pub retryable_http_codes: HashSet<u16>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        let mut codes = HashSet::new();
        codes.insert(408);
        codes.insert(429);
        codes.insert(500);
        codes.insert(502);
        codes.insert(503);
        codes.insert(504);
        Self {
            max_retries: 3,
            base_wait_ms: 1000,
            max_wait_ms: 30000,
            backoff_factor: 2.0,
            retryable_http_codes: codes,
        }
    }
}

impl RetryPolicy {
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

    pub fn compute_wait(&self, attempt: u32) -> Option<Duration> {
        if attempt == 0 {
            return None;
        }
        let raw = (self.base_wait_ms as f64) * self.backoff_factor.powi(attempt as i32 - 1);
        let ms = raw.min(self.max_wait_ms as f64) as u64;
        Some(Duration::from_millis(ms))
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

    pub fn is_exhausted(&self, attempts: u32) -> bool {
        attempts > self.max_retries
    }

    pub fn total_estimated_wait_sec(&self) -> f64 {
        let mut total = 0.0f64;
        for a in 1..=self.max_retries {
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
        assert_eq!(p.max_retries, 3);
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
        assert!(!p.is_exhausted(3));
    }

    #[test]
    fn test_is_exhausted_true_at_limit() {
        let p = RetryPolicy::with_max_retries(RetryPolicy::default(), 3);
        assert!(
            p.is_exhausted(4),
            "attempt 4 should be exhausted when max=3"
        );
    }

    #[test]
    fn test_stats_reasonable_values() {
        let p = RetryPolicy::default();
        let s = p.stats();
        assert_eq!(s.max_retries, 3);
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
        assert!(finished.duration.as_millis() >= 0);
    }
}

use crate::error::{Aria2Error, RecoverableError};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    max_tries: u32,
    retry_wait_base: Duration,
    retry_wait_max: Duration,
    max_retries_per_server: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_tries: 5,
            retry_wait_base: Duration::from_secs(5),
            retry_wait_max: Duration::from_secs(300),
            max_retries_per_server: u32::MAX,
        }
    }
}

impl RetryPolicy {
    pub fn new(max_tries: u32, wait_base: Duration) -> Self {
        Self {
            max_tries,
            retry_wait_base: wait_base,
            ..Default::default()
        }
    }

    pub fn with_max_wait(mut self, max: Duration) -> Self {
        self.retry_wait_max = max;
        self
    }

    pub fn with_max_per_server(mut self, n: u32) -> Self {
        self.max_retries_per_server = n;
        self
    }

    pub fn max_tries(&self) -> u32 {
        self.max_tries
    }

    pub fn should_retry(&self, attempt: u32, error: &Aria2Error) -> bool {
        if attempt + 1 >= self.max_tries {
            return false;
        }
        matches!(error, Aria2Error::Recoverable(_))
    }

    pub fn wait_duration(&self, attempt: u32) -> Duration {
        let secs = self
            .retry_wait_base
            .as_secs()
            .saturating_mul(1 << attempt.min(20));
        let dur = Duration::from_secs(secs);
        if dur > self.retry_wait_max {
            self.retry_wait_max
        } else if dur < self.retry_wait_base {
            self.retry_wait_base
        } else {
            dur
        }
    }
}

#[derive(Debug, Default)]
pub struct RetryStats {
    total: AtomicU32,
    timeouts: AtomicU32,
    server_errors: AtomicU32,
    network_failures: AtomicU32,
    max_retries_reached: AtomicU32,
}

impl RetryStats {
    pub fn record_retry(&self, error: &Aria2Error) {
        self.total.fetch_add(1, Ordering::Relaxed);
        match error {
            Aria2Error::Recoverable(RecoverableError::Timeout) => {
                self.timeouts.fetch_add(1, Ordering::Relaxed);
            }
            Aria2Error::Recoverable(RecoverableError::ServerError { .. }) => {
                self.server_errors.fetch_add(1, Ordering::Relaxed);
            }
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. }) => {
                self.network_failures.fetch_add(1, Ordering::Relaxed);
            }
            Aria2Error::Recoverable(RecoverableError::MaxTriesReached { .. }) => {
                self.max_retries_reached.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    pub fn total(&self) -> u32 {
        self.total.load(Ordering::Relaxed)
    }

    pub fn timeouts(&self) -> u32 {
        self.timeouts.load(Ordering::Relaxed)
    }

    pub fn server_errors(&self) -> u32 {
        self.server_errors.load(Ordering::Relaxed)
    }

    pub fn network_failures(&self) -> u32 {
        self.network_failures.load(Ordering::Relaxed)
    }

    pub fn reset(&self) {
        self.total.store(0, Ordering::Relaxed);
        self.timeouts.store(0, Ordering::Relaxed);
        self.server_errors.store(0, Ordering::Relaxed);
        self.network_failures.store(0, Ordering::Relaxed);
        self.max_retries_reached.store(0, Ordering::Relaxed);
    }
}

pub struct RetryExecutor<'a> {
    policy: &'a RetryPolicy,
    stats: &'a RetryStats,
}

impl<'a> RetryExecutor<'a> {
    pub fn new(policy: &'a RetryPolicy, stats: &'a RetryStats) -> Self {
        Self { policy, stats }
    }

    pub async fn execute<F, Fut, T>(&self, mut operation: F) -> crate::error::Result<T>
    where
        F: FnMut(u32) -> Fut,
        Fut: std::future::Future<Output = crate::error::Result<T>>,
    {
        let mut attempt = 0u32;
        loop {
            let result = operation(attempt).await;
            match result {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if !self.policy.should_retry(attempt, &error) {
                        warn!(
                            "重试失败 (尝试 {}/{}, 不再重试): {}",
                            attempt + 1,
                            self.policy.max_tries(),
                            error
                        );
                        self.stats.record_retry(&error);
                        return Err(error);
                    }
                    attempt += 1;
                    let wait = self.policy.wait_duration(attempt);
                    warn!(
                        "第 {} 次重试, 等待 {:?} 后执行 (原因: {})",
                        attempt, wait, error
                    );
                    self.stats.record_retry(&error);
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
}

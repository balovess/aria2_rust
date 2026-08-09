use crate::error::{Aria2Error, RecoverableError};
use std::sync::atomic::{AtomicU32, Ordering};
use tracing::warn;

// Re-export unified RetryPolicy from engine::retry_policy
pub use crate::engine::retry_policy::RetryPolicy;

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
                            "Retry failed (attempt {}/{}, no more retries): {}",
                            attempt + 1,
                            self.policy.max_tries(),
                            error
                        );
                        self.stats.record_retry(&error);
                        return Err(error);
                    }
                    let wait = self.policy.wait_duration(attempt);
                    attempt = attempt.saturating_add(1);
                    warn!(
                        "Retry #{}, waiting {:?} before next attempt (reason: {})",
                        attempt, wait, error
                    );
                    self.stats.record_retry(&error);
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
}

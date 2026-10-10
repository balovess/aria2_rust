//! Shared lifecycle for single-item protocol work.

use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::engine::work_scheduler::{RetryOutcome, WorkId, WorkItem, WorkScheduler};
use crate::error::{Aria2Error, FatalError, Result};

#[allow(clippy::double_must_use)]
#[async_trait]
pub(crate) trait SingleWorkAdapter: Send {
    type Output: Send;

    fn max_attempts(&self) -> u32;

    async fn execute_attempt(&mut self, attempt: u32) -> Result<Self::Output>;

    /// Return a delay when this error is retryable under source-specific rules.
    fn retry_wait(&self, attempt: u32, error: &Aria2Error) -> Option<Duration>;

    /// Wait while continuing to honor task pause, removal, and halt controls.
    async fn wait_for_retry(&mut self, wait: Duration) -> Result<()>;

    /// Reset attempt-local progress after the retry delay has elapsed.
    async fn prepare_retry(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Run one logical work item through the shared queue, attempt budget, and
/// retry-deadline lifecycle. Protocol adapters retain error classification
/// and their source-specific attempt implementation.
pub(crate) async fn run_single_work_item<A: SingleWorkAdapter>(
    adapter: &mut A,
) -> Result<A::Output> {
    let mut scheduler = WorkScheduler::new();
    scheduler
        .enqueue(WorkItem::new(WorkId::new(0), (), adapter.max_attempts()))
        .map_err(|error| scheduler_error("queue the single work item", error))?;

    loop {
        let lease = scheduler
            .admit(1, Instant::now())
            .pop()
            .ok_or_else(|| scheduler_error("admit the single work item", "no ready lease"))?;
        let attempt = lease.attempt();
        match adapter.execute_attempt(attempt).await {
            Ok(output) => {
                scheduler
                    .complete(lease)
                    .map_err(|error| scheduler_error("complete the work item", error))?;
                return Ok(output);
            }
            Err(error) => {
                let wait = adapter.retry_wait(attempt, &error);
                let retry_at = wait.map(|wait| {
                    let now = Instant::now();
                    now.checked_add(wait).unwrap_or(now)
                });
                match scheduler
                    .fail(lease, retry_at)
                    .map_err(|error| scheduler_error("record the failed attempt", error))?
                {
                    RetryOutcome::Scheduled => {
                        let deadline = scheduler.next_retry_deadline().ok_or_else(|| {
                            scheduler_error("read the retry deadline", "no delayed work")
                        })?;
                        let wait = deadline.saturating_duration_since(Instant::now());
                        if let Err(wait_error) = adapter.wait_for_retry(wait).await {
                            scheduler.cancel();
                            return Err(wait_error);
                        }
                        if let Err(retry_error) = adapter.prepare_retry().await {
                            scheduler.cancel();
                            return Err(retry_error);
                        }
                    }
                    RetryOutcome::Exhausted(()) => return Err(error),
                }
            }
        }
    }
}

fn scheduler_error(context: &str, error: impl std::fmt::Display) -> Aria2Error {
    Aria2Error::Fatal(FatalError::Config(format!(
        "Core work runner could not {context}: {error}"
    )))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use async_trait::async_trait;

    use super::{SingleWorkAdapter, run_single_work_item};
    use crate::error::{Aria2Error, RecoverableError, Result};

    struct FakeSource {
        max_attempts: u32,
        failed_attempts: u32,
        attempts: u32,
        waits: u32,
        cancel_during_wait: bool,
    }

    #[async_trait]
    impl SingleWorkAdapter for FakeSource {
        type Output = Vec<u8>;

        fn max_attempts(&self) -> u32 {
            self.max_attempts
        }

        async fn execute_attempt(&mut self, _attempt: u32) -> Result<Self::Output> {
            self.attempts += 1;
            if self.attempts <= self.failed_attempts {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "fake source failure".into(),
                    },
                ));
            }
            Ok(b"verified fake payload".to_vec())
        }

        fn retry_wait(&self, _attempt: u32, error: &Aria2Error) -> Option<Duration> {
            matches!(
                error,
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
            )
            .then_some(Duration::ZERO)
        }

        async fn wait_for_retry(&mut self, _wait: Duration) -> Result<()> {
            self.waits += 1;
            if self.cancel_during_wait {
                return Err(Aria2Error::DownloadFailed("fake task cancelled".into()));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn fake_source_uses_the_shared_retry_lifecycle() {
        let mut source = FakeSource {
            max_attempts: 3,
            failed_attempts: 1,
            attempts: 0,
            waits: 0,
            cancel_during_wait: false,
        };

        let output = run_single_work_item(&mut source).await.unwrap();

        assert_eq!(output, b"verified fake payload");
        assert_eq!(source.attempts, 2);
        assert_eq!(source.waits, 1);
    }

    #[tokio::test]
    async fn fake_source_stops_when_task_controls_cancel_a_retry_wait() {
        let mut source = FakeSource {
            max_attempts: 3,
            failed_attempts: 3,
            attempts: 0,
            waits: 0,
            cancel_during_wait: true,
        };

        let error = run_single_work_item(&mut source).await.unwrap_err();

        assert!(matches!(error, Aria2Error::DownloadFailed(_)));
        assert_eq!(source.attempts, 1);
        assert_eq!(source.waits, 1);
    }

    #[tokio::test]
    async fn fake_source_observes_the_shared_total_attempt_limit() {
        let mut source = FakeSource {
            max_attempts: 2,
            failed_attempts: 3,
            attempts: 0,
            waits: 0,
            cancel_during_wait: false,
        };

        let error = run_single_work_item(&mut source).await.unwrap_err();

        assert!(matches!(error, Aria2Error::Recoverable(_)));
        assert_eq!(source.attempts, 2);
        assert_eq!(source.waits, 1);
    }
}

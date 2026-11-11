use std::time::{Duration, Instant};

use tracing::debug;

use crate::engine::http::segment_downloader::{HttpSegmentDownloader, RangeProbeResult};
use crate::engine::retry_policy::RetryPolicy;
use crate::engine::work_runner::SingleWorkAdapter;
use crate::error::{Aria2Error, Result};

use super::DownloadCommand;

/// Maximum time spent waiting for a rate-limited capability probe.
const CAPACITY_PROBE_LIMIT: Duration = Duration::from_secs(5 * 60);

pub(super) struct HttpRangeProbeWork<'a> {
    pub(super) command: &'a mut DownloadCommand,
    pub(super) prober: &'a HttpSegmentDownloader,
    pub(super) uri: &'a str,
    pub(super) cookie_header: Option<&'a str>,
    pub(super) retry_policy: RetryPolicy,
    pub(super) adaptive_options: crate::request::request_group::HttpAdaptiveOptions,
}

#[async_trait::async_trait]
impl SingleWorkAdapter for HttpRangeProbeWork<'_> {
    type Output = RangeProbeResult;

    fn max_attempts(&self) -> u32 {
        self.retry_policy.max_tries()
    }

    async fn execute_attempt(&mut self, _attempt: u32) -> Result<Self::Output> {
        // Capacity backoff does not spend the probe's retry budget; otherwise
        // a zero retry wait would exhaust all attempts immediately.
        let started = Instant::now();
        let mut capacity_waits = 0u32;
        loop {
            match self
                .prober
                .probe_range_metadata(self.uri, self.cookie_header)
                .await
            {
                Err(error)
                    if crate::engine::http::concurrent_download::is_capacity_limited_error(
                        &error,
                    ) && started.elapsed() < CAPACITY_PROBE_LIMIT =>
                {
                    capacity_waits = capacity_waits.saturating_add(1);
                    let wait = crate::engine::http::adaptive_concurrency::capacity_backoff_with(
                        capacity_waits,
                        &self.adaptive_options,
                    );
                    debug!(
                        waits = capacity_waits,
                        wait_ms = wait.as_millis().min(u128::from(u64::MAX)) as u64,
                        error = %error,
                        "HTTP Range capability probe rate-limited; backing off"
                    );
                    self.command.wait_for_retry(wait).await?;
                }
                result => return result,
            }
        }
    }

    fn retry_wait(&self, attempt: u32, error: &Aria2Error) -> Option<Duration> {
        if !self
            .retry_policy
            .should_retry(attempt.saturating_sub(1), error)
        {
            return None;
        }
        let wait = self.retry_policy.compute_wait(attempt).unwrap_or_default();
        debug!(
            attempt,
            wait_ms = wait.as_millis() as u64,
            error = %error,
            "Retrying transient HTTP Range capability probe"
        );
        Some(wait)
    }

    async fn wait_for_retry(&mut self, wait: Duration) -> Result<()> {
        if wait.is_zero() {
            return Ok(());
        }
        self.command.wait_for_retry(wait).await
    }
}

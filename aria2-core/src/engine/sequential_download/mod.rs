// Sequential download engine: single-stream HTTP download with resume,
// redirect handling, auth challenge, and gap-based partial downloads.

mod auth_retry;
mod download_flow;
mod gap_download;

use std::sync::Arc;

use crate::engine::download_cookie::CookieHelper;
use crate::engine::download_progress::ProgressUpdater;
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, Result};
use crate::http::HttpRequestPolicy;
use crate::network::ConnectionContext;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{AtomicProgress, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub struct GapDownloadResult {
    pub completed_gaps: Vec<(u64, u64)>,
    pub error: Option<Aria2Error>,
}

pub struct SequentialDownloader {
    pub(crate) client: Arc<reqwest::Client>,
    pub(crate) output_path: std::path::PathBuf,
    pub(crate) request_policy: HttpRequestPolicy,
    pub(crate) cookie_helper: CookieHelper,
    pub(crate) progress_updater: ProgressUpdater,
    pub(crate) group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters — avoids `RwLock` on the hot path.
    pub(crate) progress: Arc<AtomicProgress>,
    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// When `Some`, tokens are acquired after the per-download limiter
    /// in `download_flow.rs` and `gap_download.rs`.
    pub(crate) global_limiter: Option<RateLimiter>,
}

impl SequentialDownloader {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Arc<reqwest::Client>,
        output_path: std::path::PathBuf,
        request_policy: HttpRequestPolicy,
        cookie_helper: CookieHelper,
        progress_updater: ProgressUpdater,
        group: Arc<std::sync::RwLock<RequestGroup>>,
        progress: Arc<AtomicProgress>,
        global_limiter: Option<RateLimiter>,
    ) -> Self {
        Self {
            client,
            output_path,
            request_policy,
            cookie_helper,
            progress_updater,
            group,
            progress,
            global_limiter,
        }
    }

    /// Non-blocking cancellation check.
    ///
    /// Returns `Err` when the underlying RequestGroup has been marked
    /// `Removed` (by `aria2.remove` / `aria2.forceRemove`) or `Paused`
    /// (by `aria2.pause` / `aria2.forcePause`). Uses `try_read` on the
    /// outer group lock so it is safe to call from the hot download loop;
    /// a contended lock is treated as "not cancelled" and the caller will
    /// re-check on the next iteration.
    pub(crate) fn publish_connection_context(&self, uri: &str, peer: Option<std::net::SocketAddr>) {
        let Some(peer) = peer else { return };
        let Ok(url) = reqwest::Url::parse(uri) else {
            return;
        };
        let Some(host) = url.host_str() else { return };
        self.group
            .recover()
            .set_connection_context(ConnectionContext::new(
                host,
                url.port_or_known_default().unwrap_or(80),
                peer,
            ));
    }

    pub(crate) fn check_cancelled(&self) -> Result<()> {
        match self.group.try_read() {
            Ok(g) if g.is_removed() => Err(Aria2Error::DownloadFailed(
                "Download cancelled by user".into(),
            )),
            Ok(g) if g.is_paused_flag() => {
                Err(Aria2Error::DownloadFailed("Download paused".into()))
            }
            Ok(g) if g.is_force_halt_requested() || g.is_halt_requested() => {
                Err(Aria2Error::DownloadFailed("Download halted".into()))
            }
            _ => Ok(()),
        }
    }

    pub(crate) async fn wait_for_cancellation(&self) -> Result<()> {
        loop {
            let notifier = self.group.recover().lifecycle_notifier();
            let notified = notifier.notified();
            self.check_cancelled()?;
            notified.await;
        }
    }

    /// Wait between retry attempts without delaying RequestGroup controls.
    /// A plain sleep would make pause, remove, and halt wait for the full
    /// configured retry interval.
    pub(crate) async fn wait_for_retry(&self, wait: std::time::Duration) -> Result<()> {
        let notifier = self.group.recover().lifecycle_notifier();
        let notified = notifier.notified();
        self.check_cancelled()?;
        tokio::select! {
            _ = tokio::time::sleep(wait) => self.check_cancelled(),
            _ = notified => self.check_cancelled(),
        }
    }

    pub(crate) fn classify_file_not_found(&self) -> Aria2Error {
        self.group.recover().file_not_found_error()
    }

    fn should_retry(&self, attempt: u32, error: &Aria2Error, policy: &RetryPolicy) -> bool {
        policy.should_retry(attempt, error)
            || (matches!(
                error,
                Aria2Error::Recoverable(crate::error::RecoverableError::ResourceNotFound)
            ) && policy.can_retry_after(attempt.saturating_add(1))
                && self.group.recover().can_retry_file_not_found())
    }

    // ── Range / gap utility functions ─────────────────────────────────

    pub fn merge_ranges(ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
        if ranges.is_empty() {
            return Vec::new();
        }

        let mut sorted = ranges.to_vec();
        sorted.sort_by_key(|r| r.0);

        let mut merged = Vec::new();
        let mut current = sorted[0];

        for &(offset, length) in sorted.iter().skip(1) {
            let current_end = current.0 + current.1;
            let next_end = offset + length;

            if offset <= current_end {
                current = (current.0, std::cmp::max(current_end, next_end) - current.0);
            } else {
                merged.push(current);
                current = (offset, length);
            }
        }
        merged.push(current);
        merged
    }

    pub fn find_all_gaps(completed_ranges: &[(u64, u64)], total_length: u64) -> Vec<(u64, u64)> {
        let merged_ranges = Self::merge_ranges(completed_ranges);
        let mut gaps = Vec::new();
        if merged_ranges.is_empty() {
            if total_length > 0 {
                gaps.push((0, total_length));
            }
            return gaps;
        }

        let mut current = 0;
        for &(offset, length) in &merged_ranges {
            if offset > current {
                gaps.push((current, offset - current));
            }
            current = std::cmp::max(current, offset + length);
        }
        if current < total_length {
            gaps.push((current, total_length - current));
        }
        gaps
    }

    // ── Retry wrappers ────────────────────────────────────────────────

    pub async fn execute_with_gaps_with_retry(
        &mut self,
        uri: &str,
        total_length: u64,
        completed_ranges: &[(u64, u64)],
        retry_policy: &RetryPolicy,
    ) -> Result<()> {
        let mut accumulated_completed: Vec<(u64, u64)> = completed_ranges.to_vec();

        let mut attempt = 0u32;
        loop {
            if attempt > 0
                && let Some(wait) = retry_policy.compute_wait(attempt)
            {
                tracing::info!(
                    "Sequential download with gaps retry #{} (waiting {:?}), {} ranges already completed...",
                    attempt,
                    wait,
                    accumulated_completed.len()
                );
                self.wait_for_retry(wait).await?;
            }

            let result = self
                .execute_with_gaps(uri, total_length, &accumulated_completed)
                .await;

            if !result.completed_gaps.is_empty() {
                tracing::info!(
                    "Attempt #{} completed {} gaps",
                    attempt + 1,
                    result.completed_gaps.len()
                );
                accumulated_completed.extend(result.completed_gaps);
                accumulated_completed = Self::merge_ranges(&accumulated_completed);
            }

            if result.error.is_none() {
                return Ok(());
            }

            tracing::warn!(
                "Sequential download with gaps attempt #{} failed: {}",
                attempt.saturating_add(1),
                result.error.as_ref().unwrap()
            );
            let error = result.error.expect("error was checked above");

            if !self.should_retry(attempt, &error, retry_policy) {
                return Err(error);
            }
            debug_assert!(!retry_policy.is_exhausted(attempt.saturating_add(1)));
            attempt = attempt.saturating_add(1);
        }
    }

    pub async fn execute_with_retry(
        &mut self,
        uri: &str,
        resume_state: &crate::filesystem::resume_helper::ResumeState,
        total_length: u64,
        retry_policy: &RetryPolicy,
    ) -> Result<()> {
        let mut attempt = 0u32;
        loop {
            if attempt > 0
                && let Some(wait) = retry_policy.compute_wait(attempt)
            {
                tracing::info!(
                    "Sequential download retry #{} (waiting {:?})...",
                    attempt,
                    wait
                );
                self.wait_for_retry(wait).await?;
            }

            match self.execute(uri, resume_state, total_length).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!(
                        "Sequential download attempt #{} failed: {}",
                        attempt.saturating_add(1),
                        e
                    );
                    let should_retry = self.should_retry(attempt, &e, retry_policy);
                    if !should_retry {
                        return Err(e);
                    }
                    debug_assert!(!retry_policy.is_exhausted(attempt.saturating_add(1)));
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Linux-only splice download path
// ---------------------------------------------------------------------------

impl SequentialDownloader {
    #[cfg(target_os = "linux")]
    async fn try_splice_sequential(&mut self, uri: &str, total_length: u64) -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.output_path)?;

        let bytes = crate::http::splice_http::try_splice_download(uri, 0, total_length, &file, 0)
            .await
            .map_err(|e| Aria2Error::Io(format!("splice download failed: {e}")))?;

        let final_speed = {
            let g = self.group.recover();
            let elapsed = g.elapsed_time();
            match elapsed {
                Some(d) if d.as_secs_f64() > 0.0 => (bytes as f64 / d.as_secs_f64()) as u64,
                _ => 0,
            }
        };

        {
            self.progress.set_total_length(bytes);
            self.progress.set_completed_length(bytes);
            self.progress.set_download_speed(final_speed);
            self.progress.set_upload_speed(0);
            let mut g = self.group.recover_mut();
            g.complete()?;
        }

        tracing::info!(
            "Sequential download (splice) complete: {} ({} bytes)",
            self.output_path.display(),
            bytes
        );
        self.cookie_helper.save_cookies_if_configured();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::engine::download_cookie::CookieHelper;
    use crate::engine::download_progress::ProgressUpdater;
    use crate::http::HttpRequestPolicy;
    use crate::http::cookie_storage::CookieStorage;
    use crate::request::request_group::{AtomicProgress, DownloadOptions, GroupId, RequestGroup};
    use crate::util::perf_monitor::AtomicMetrics;

    use super::SequentialDownloader;

    fn test_downloader(group: Arc<std::sync::RwLock<RequestGroup>>) -> SequentialDownloader {
        let progress = Arc::new(AtomicProgress::new());
        SequentialDownloader::new(
            crate::http::client_pool::get_global_client(),
            std::env::temp_dir().join("aria2-sequential-retry-test.bin"),
            HttpRequestPolicy::default(),
            CookieHelper::new(Arc::new(CookieStorage::new()), None),
            ProgressUpdater::new(
                None,
                None,
                Arc::clone(&progress),
                Arc::new(AtomicMetrics::new()),
                None,
            ),
            group,
            progress,
            None,
        )
    }

    #[tokio::test]
    async fn retry_wait_is_interruptible_when_removed() {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(900),
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )));
        group.write().unwrap().mark_removed();
        let downloader = test_downloader(group);

        let result = tokio::time::timeout(
            Duration::from_millis(100),
            downloader.wait_for_retry(Duration::from_secs(5)),
        )
        .await
        .expect("removed retry wait should stop promptly");

        assert!(matches!(
            result,
            Err(crate::error::Aria2Error::DownloadFailed(message))
                if message == "Download cancelled by user"
        ));
    }

    #[tokio::test]
    async fn retry_wait_wakes_when_removed_after_wait_starts() {
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(901),
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )));
        let group_for_wait = Arc::clone(&group);
        let downloader = test_downloader(group);
        let wait_task =
            tokio::spawn(async move { downloader.wait_for_retry(Duration::from_secs(5)).await });

        tokio::time::sleep(Duration::from_millis(10)).await;
        group_for_wait.write().unwrap().mark_removed();

        let result = tokio::time::timeout(Duration::from_millis(100), wait_task)
            .await
            .expect("lifecycle notification should wake retry wait")
            .expect("retry wait task should not panic");
        assert!(matches!(
            result,
            Err(crate::error::Aria2Error::DownloadFailed(message))
                if message == "Download cancelled by user"
        ));
    }

    #[test]
    fn test_merge_ranges_empty() {
        let ranges = &[];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert!(result.is_empty());
    }

    #[test]
    fn test_merge_ranges_single() {
        let ranges = &[(0, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 100)]);
    }

    #[test]
    fn test_merge_ranges_non_overlapping_sorted() {
        let ranges = &[(0, 100), (200, 100), (400, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 100), (200, 100), (400, 100)]);
    }

    #[test]
    fn test_merge_ranges_non_overlapping_unsorted() {
        let ranges = &[(200, 100), (0, 100), (400, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 100), (200, 100), (400, 100)]);
    }

    #[test]
    fn test_merge_ranges_overlapping_inner() {
        let ranges = &[(0, 200), (50, 50)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_overlapping_inner_unsorted() {
        let ranges = &[(50, 50), (0, 200)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_overlapping_partial() {
        let ranges = &[(0, 100), (50, 150)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_overlapping_partial_unsorted() {
        let ranges = &[(50, 150), (0, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_adjacent() {
        let ranges = &[(0, 100), (100, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_adjacent_unsorted() {
        let ranges = &[(100, 100), (0, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_duplicate() {
        let ranges = &[(0, 100), (0, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 100)]);
    }

    #[test]
    fn test_merge_ranges_multiple_overlapping() {
        let ranges = &[(0, 100), (50, 150), (200, 100), (180, 150)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 330)]);
    }

    #[test]
    fn test_merge_ranges_zero_length() {
        let ranges = &[(0, 0), (100, 0), (200, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 0), (100, 0), (200, 100)]);
    }

    #[test]
    fn test_merge_ranges_complex() {
        let ranges = &[(10, 5), (0, 20), (15, 25), (50, 10), (45, 20), (100, 50)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 40), (45, 20), (100, 50)]);
    }

    #[test]
    fn test_find_all_gaps_empty_ranges() {
        let ranges = &[];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
        assert_eq!(gaps, vec![(0, 1000)]);
    }

    #[test]
    fn test_find_all_gaps_no_gaps() {
        let ranges = &[(0, 1000)];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
        assert!(gaps.is_empty());
    }

    #[test]
    fn test_find_all_gaps_single_gap() {
        let ranges = &[(0, 500), (600, 400)];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
        assert_eq!(gaps, vec![(500, 100)]);
    }

    #[test]
    fn test_find_all_gaps_multiple_gaps() {
        let ranges = &[(0, 100), (200, 100), (400, 200)];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
        assert_eq!(gaps, vec![(100, 100), (300, 100), (600, 400)]);
    }

    #[test]
    fn test_find_all_gaps_overlapping_ranges() {
        let ranges = &[(0, 200), (100, 150), (300, 50)];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 500);
        assert_eq!(gaps, vec![(250, 50), (350, 150)]);
    }
}

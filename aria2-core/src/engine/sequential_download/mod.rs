// Sequential download engine: single-stream HTTP download with resume,
// redirect handling, auth challenge, and gap-based partial downloads.

mod auth_retry;
mod download_flow;
mod gap_download;

use std::sync::Arc;

use crate::engine::download_cookie::CookieHelper;
use crate::engine::download_progress::ProgressUpdater;
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{AtomicProgress, RequestGroup};

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
    pub(crate) headers: Vec<(String, String)>,
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
    pub fn new(
        client: Arc<reqwest::Client>,
        output_path: std::path::PathBuf,
        headers: Vec<(String, String)>,
        cookie_helper: CookieHelper,
        progress_updater: ProgressUpdater,
        group: Arc<std::sync::RwLock<RequestGroup>>,
        progress: Arc<AtomicProgress>,
        global_limiter: Option<RateLimiter>,
    ) -> Self {
        Self {
            client,
            output_path,
            headers,
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
        let mut last_err = None;
        let mut accumulated_completed: Vec<(u64, u64)> = completed_ranges.to_vec();

        for attempt in 0..=retry_policy.max_retries {
            if attempt > 0
                && let Some(wait) = retry_policy.compute_wait(attempt - 1)
            {
                tracing::info!(
                    "Sequential download with gaps retry #{} (waiting {:?}), {} ranges already completed...",
                    attempt,
                    wait,
                    accumulated_completed.len()
                );
                tokio::time::sleep(wait).await;
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
                attempt + 1,
                result.error.as_ref().unwrap()
            );
            last_err = result.error;

            if retry_policy.is_exhausted(attempt)
                || !retry_policy.should_retry_error(&format!("{:?}", last_err.as_ref().unwrap()))
            {
                break;
            }
        }

        Err(last_err.unwrap_or_else(|| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "All retries failed".into(),
            })
        }))
    }

    pub async fn execute_with_retry(
        &mut self,
        uri: &str,
        resume_state: &crate::filesystem::resume_helper::ResumeState,
        total_length: u64,
        retry_policy: &RetryPolicy,
    ) -> Result<()> {
        let mut last_err = None;

        for attempt in 0..=retry_policy.max_retries {
            if attempt > 0
                && let Some(wait) = retry_policy.compute_wait(attempt - 1)
            {
                tracing::info!(
                    "Sequential download retry #{} (waiting {:?})...",
                    attempt,
                    wait
                );
                tokio::time::sleep(wait).await;
            }

            match self.execute(uri, resume_state, total_length).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!("Sequential download attempt #{} failed: {}", attempt + 1, e);
                    last_err = Some(e);
                    if retry_policy.is_exhausted(attempt)
                        || !retry_policy
                            .should_retry_error(&format!("{:?}", last_err.as_ref().unwrap()))
                    {
                        break;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "All retries failed".into(),
            })
        }))
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
    use super::SequentialDownloader;

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

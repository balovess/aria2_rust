//! Concurrent download module — split into sub-modules for maintainability.

mod pipeline;
mod segment;

use std::sync::Arc;
use std::time::Duration;

use crate::engine::download_cookie::CookieHelper;
use crate::engine::download_progress::ProgressUpdater;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::filesystem::resume_helper::ResumeState;
use crate::http::AuthResolveOptions;
use crate::http::HttpRequestPolicy;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{AtomicProgress, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

pub use pipeline::execute_with_coordinator;
pub use segment::execute;

/// Cap the requested concurrent ranges at aria2's minimum split-size policy.
///
/// A task may request more connections than its payload can support without
/// creating ranges of the configured minimum size. The original scheduler
/// therefore keeps one range for a payload smaller than that threshold and
/// only admits additional ranges as another minimum-sized range remains.
pub(crate) fn effective_segment_count(
    total_length: u64,
    requested_split: u16,
    min_split_size: u64,
) -> usize {
    let requested = u64::from(requested_split.max(1));
    let max_by_minimum = total_length
        .checked_div(min_split_size.max(1))
        .unwrap_or(0)
        .max(1);
    requested.min(max_by_minimum) as usize
}

/// Map a completed HTTP attempt to the status code recorded by mirror stats.
///
/// The download error remains the source of truth for callers. This helper
/// only supplies the best structured status available to the internal mirror
/// scheduler; errors without an HTTP status retain the existing 500 fallback.
pub(crate) fn server_stat_error_code(error: &Aria2Error) -> u16 {
    match error {
        Aria2Error::Recoverable(RecoverableError::ServerError { code }) => *code,
        Aria2Error::Recoverable(RecoverableError::RangeNotSatisfiable { .. }) => 416,
        Aria2Error::Recoverable(
            RecoverableError::ResourceNotFound | RecoverableError::MaxFileNotFound,
        ) => 404,
        Aria2Error::Recoverable(RecoverableError::Timeout) => 408,
        _ => crate::constants::HTTP_DEFAULT_ERROR_CODE,
    }
}

/// Outcome of a concurrent download attempt.
pub enum ConcurrentDownloadResult {
    /// All segments completed successfully.
    Complete,
    /// The server does not support Range requests well enough; fall back to
    /// sequential mode, preserving already-completed byte ranges.
    Fallback { completed_ranges: Vec<(u64, u64)> },
}

/// Orchestrates concurrent (multi-segment / multi-mirror) HTTP downloads.
///
/// Fields are pub(crate) so that the sibling modules segment and
/// pipeline can access them without going through accessor methods on the
/// hot path.
pub struct ConcurrentDownloader {
    pub(crate) client: Arc<reqwest::Client>,
    pub(crate) output_path: std::path::PathBuf,
    pub(crate) request_policy: HttpRequestPolicy,
    pub(crate) auth_options: AuthResolveOptions,
    pub(crate) netrc_path: Option<String>,
    pub(crate) cookie_helper: CookieHelper,
    pub(crate) progress_updater: ProgressUpdater,
    pub(crate) group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters — avoids RwLock on the hot path.
    pub(crate) progress: Arc<AtomicProgress>,
    pub(crate) mmap_threshold: u64,
    pub(crate) file_allocation: String,
    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// When `Some`, tokens are acquired after the per-download limiter
    /// in `segment.rs` and `pipeline.rs`.
    pub(crate) global_limiter: Option<RateLimiter>,
}

pub(super) async fn flush_requested_control_file(
    dl: &ConcurrentDownloader,
    writer: &mut CachedDiskWriter,
    control_file: &mut Option<ControlFile>,
    completed_bytes: u64,
) -> Result<()> {
    if control_file.is_none() || !dl.group.recover().is_save_control_file_requested() {
        return Ok(());
    }

    writer.flush().await.map_err(|error| {
        Aria2Error::FileIo(format!(
            "Failed to flush requested concurrent checkpoint: {error}"
        ))
    })?;
    if let Some(control_file) = control_file.as_mut() {
        control_file.update_completed_length(completed_bytes);
        control_file.save().await.map_err(|error| {
            Aria2Error::FileIo(format!(
                "Failed to save requested concurrent checkpoint: {error}"
            ))
        })?;
    }
    dl.group.recover().take_save_control_file_request();
    Ok(())
}

impl ConcurrentDownloader {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Arc<reqwest::Client>,
        output_path: std::path::PathBuf,
        request_policy: HttpRequestPolicy,
        auth_options: AuthResolveOptions,
        netrc_path: Option<String>,
        cookie_helper: CookieHelper,
        progress_updater: ProgressUpdater,
        group: Arc<std::sync::RwLock<RequestGroup>>,
        progress: Arc<AtomicProgress>,
        mmap_threshold: u64,
        file_allocation: String,
        global_limiter: Option<RateLimiter>,
    ) -> Self {
        Self {
            client,
            output_path,
            request_policy,
            auth_options,
            netrc_path,
            cookie_helper,
            progress_updater,
            group,
            progress,
            mmap_threshold,
            file_allocation,
            global_limiter,
        }
    }

    /// Non-blocking cancellation check.
    ///
    /// Returns Err when the underlying RequestGroup has been marked
    /// removed or paused. Uses try_read on the outer group lock so it is
    /// safe to call from the download loop; a contended lock is treated as
    /// `not cancelled'' and the caller will re-check on the next iteration.
    pub(crate) fn check_cancelled(&self) -> Result<()> {
        use crate::error::Aria2Error;
        match self.group.try_read() {
            Ok(g) if g.is_removed() => Err(Aria2Error::DownloadFailed(
                "Download cancelled by user".into(),
            )),
            Ok(g) if g.is_paused_flag() => {
                Err(Aria2Error::DownloadFailed("Download paused".into()))
            }
            Ok(g) if g.is_force_halt_requested() => {
                Err(Aria2Error::DownloadFailed("Download halted".into()))
            }
            Ok(g) if g.is_halt_requested() => {
                Err(Aria2Error::DownloadFailed("Download halted".into()))
            }
            _ => Ok(()),
        }
    }

    /// Wait for adaptive HTTP retry cooldown without delaying RequestGroup
    /// pause, remove, or halt controls.
    pub(crate) async fn wait_for_retry(&self, wait: Duration) -> Result<()> {
        let notifier = self.group.recover().lifecycle_notifier();
        let notified = notifier.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        self.check_cancelled()?;
        tokio::select! {
            _ = tokio::time::sleep(wait) => self.check_cancelled(),
            _ = notified.as_mut() => self.check_cancelled(),
        }
    }

    /// Entry point: decides whether to use single-mirror or multi-mirror
    /// concurrent download and delegates accordingly.
    pub async fn execute_with_retry(
        &mut self,
        uri: &str,
        total_length: u64,
        resume_state: &ResumeState,
        max_retries_per_segment: u32,
    ) -> Result<ConcurrentDownloadResult> {
        use crate::constants;

        tracing::info!(
            "Using concurrent download mode (split={}, max_retries/segment={})",
            self.group
                .recover()
                .options()
                .split
                .unwrap_or(constants::DEFAULT_SPLIT),
            max_retries_per_segment
        );

        let all_uris: Vec<String> = {
            let g = self.group.recover();
            g.uris().to_vec()
        };

        if all_uris.len() > 1 {
            tracing::info!(
                "Intelligent multi-mirror selection enabled: {} mirror sources",
                all_uris.len()
            );
            pipeline::execute_with_coordinator(
                self,
                &all_uris,
                total_length,
                resume_state,
                max_retries_per_segment,
            )
            .await
        } else {
            segment::execute(
                self,
                uri,
                total_length,
                resume_state,
                max_retries_per_segment,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{effective_segment_count, server_stat_error_code};
    use crate::error::{Aria2Error, RecoverableError};

    #[test]
    fn server_stat_error_code_preserves_structured_http_statuses() {
        assert_eq!(
            server_stat_error_code(&Aria2Error::Recoverable(RecoverableError::ServerError {
                code: 429
            })),
            429
        );
        assert_eq!(
            server_stat_error_code(&Aria2Error::Recoverable(
                RecoverableError::RangeNotSatisfiable {
                    range: "bytes=0-99".to_string(),
                }
            )),
            416
        );
        assert_eq!(
            server_stat_error_code(&Aria2Error::Recoverable(RecoverableError::Timeout)),
            408
        );
    }

    #[test]
    fn server_stat_error_code_keeps_default_for_non_status_errors() {
        assert_eq!(
            server_stat_error_code(&Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "connection reset".to_string(),
                }
            )),
            crate::constants::HTTP_DEFAULT_ERROR_CODE
        );
    }

    #[test]
    fn min_split_size_caps_concurrent_segment_count() {
        assert_eq!(
            effective_segment_count(10 * 1024 * 1024, 16, 20 * 1024 * 1024),
            1
        );
        assert_eq!(
            effective_segment_count(40 * 1024 * 1024, 16, 20 * 1024 * 1024),
            2
        );
        assert_eq!(
            effective_segment_count(100 * 1024 * 1024, 16, 20 * 1024 * 1024),
            5
        );
    }

    #[test]
    fn zero_min_split_size_keeps_requested_segment_count() {
        assert_eq!(effective_segment_count(10 * 1024 * 1024, 4, 0), 4);
    }
}

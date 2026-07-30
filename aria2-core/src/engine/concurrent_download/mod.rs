//! Concurrent download module — split into sub-modules for maintainability.

mod pipeline;
mod segment;

use std::sync::Arc;

use crate::engine::download_cookie::CookieHelper;
use crate::engine::download_progress::ProgressUpdater;
use crate::error::Result;
use crate::util::rwlock_ext::RwLockRecover;
use crate::filesystem::resume_helper::ResumeState;
use crate::request::request_group::{AtomicProgress, RequestGroup};

pub use pipeline::execute_with_coordinator;
pub use segment::execute;

/// Outcome of a concurrent download attempt.
pub enum ConcurrentDownloadResult {
    /// All segments completed successfully.
    Complete,
    /// The server does not support Range requests well enough; fall back to
    /// sequential mode, preserving already-completed byte ranges.
    Fallback {
        completed_ranges: Vec<(u64, u64)>,
    },
}

/// Orchestrates concurrent (multi-segment / multi-mirror) HTTP downloads.
///
/// Fields are pub(crate) so that the sibling modules segment and
/// pipeline can access them without going through accessor methods on the
/// hot path.
pub struct ConcurrentDownloader {
    pub(crate) client: Arc<reqwest::Client>,
    pub(crate) output_path: std::path::PathBuf,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) cookie_helper: CookieHelper,
    pub(crate) progress_updater: ProgressUpdater,
    pub(crate) group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters — avoids RwLock on the hot path.
    pub(crate) progress: Arc<AtomicProgress>,
    pub(crate) mmap_threshold: u64,
    pub(crate) file_allocation: String,
}

impl ConcurrentDownloader {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Arc<reqwest::Client>,
        output_path: std::path::PathBuf,
        headers: Vec<(String, String)>,
        cookie_helper: CookieHelper,
        progress_updater: ProgressUpdater,
        group: Arc<std::sync::RwLock<RequestGroup>>,
        progress: Arc<AtomicProgress>,
        mmap_threshold: u64,
        file_allocation: String,
    ) -> Self {
        Self {
            client,
            output_path,
            headers,
            cookie_helper,
            progress_updater,
            group,
            progress,
            mmap_threshold,
            file_allocation,
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
            _ => Ok(()),
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
            segment::execute(self, uri, total_length, resume_state, max_retries_per_segment).await
        }
    }
}
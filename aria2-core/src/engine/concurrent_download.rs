use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use reqwest;
use tokio::sync::mpsc;

use crate::constants;
use crate::engine::command::ProgressUpdate;
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::download_cookie::CookieHelper;
use crate::engine::download_progress::ProgressUpdater;
use crate::engine::http_segment_downloader::{HttpSegmentDownloader, WriteChunk};
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::filesystem::resume_helper::ResumeState;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use crate::request::request_group::{AtomicProgress, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

type SegmentFetchFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = (
                    u32,
                    std::result::Result<u64, crate::error::Aria2Error>,
                ),
            > + Send,
    >,
>;

pub enum ConcurrentDownloadResult {
    Complete,
    Fallback { completed_ranges: Vec<(u64, u64)> },
}

pub struct ConcurrentDownloader {
    client: Arc<reqwest::Client>,
    output_path: std::path::PathBuf,
    headers: Vec<(String, String)>,
    cookie_helper: CookieHelper,
    progress_updater: ProgressUpdater,
    group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters — avoids `RwLock` on the hot path.
    progress: Arc<AtomicProgress>,
    mmap_threshold: u64,
    file_allocation: String,
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
    /// Returns `Err` when the underlying RequestGroup has been marked
    /// removed or paused. Uses `try_read` on the outer group lock so it is
    /// safe to call from the download loop; a contended lock is treated as
    /// "not cancelled" and the caller will re-check on the next iteration.
    fn check_cancelled(&self) -> Result<()> {
        match self.group.try_read() {
            Ok(g) if g.is_removed() => Err(Aria2Error::DownloadFailed(
                "Download cancelled by user".into(),
            )),
            Ok(g) if g.is_paused_flag() => Err(Aria2Error::DownloadFailed(
                "Download paused".into(),
            )),
            _ => Ok(()),
        }
    }

    pub async fn execute(
        &mut self,
        uri: &str,
        total_length: u64,
        resume_state: &ResumeState,
        max_retries_per_segment: u32,
    ) -> Result<ConcurrentDownloadResult> {
        {
            self.group.recover().set_total_length(total_length);
        }

        let options = self.group.recover().options_arc();
        let split = options.split.unwrap_or(constants::DEFAULT_SPLIT) as usize;
        let max_conn = options
            .max_connection_per_server
            .unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
            as usize;
        let seg_size = total_length / split as u64;

        tracing::info!(
            "Concurrent download started: split={}, max_conn={}, segment_size={} bytes, total={}",
            split,
            max_conn,
            seg_size,
            total_length
        );

        let mut manager =
            ConcurrentSegmentManager::new(total_length, vec![uri.to_string()], Some(seg_size));
        manager.set_max_connections_per_mirror(max_conn.min(split));
        manager.set_max_retries(max_retries_per_segment);

        let mut consecutive_416_count = 0u32;
        let mut total_416_count = 0u32;
        let fallback_threshold_consecutive = 3u32;
        let fallback_threshold_ratio = 0.2f64;
        let mut should_fallback = false;

        if resume_state.should_resume {
            manager.mark_completed_up_to(resume_state.start_offset, resume_state.existing_length);
            self.progress_updater.reset(resume_state.start_offset);
            tracing::debug!(
                "Resume: marked {} bytes as completed, continuing from offset {}",
                resume_state.existing_length,
                resume_state.start_offset
            );
        } else {
            self.progress_updater.reset(0);
        }

        let cookie_hdr = self.cookie_helper.build_cookie_header(uri);

        let use_mmap = self.file_allocation == "mmap" && total_length >= self.mmap_threshold;
        let mut writer =
            CachedDiskWriter::new_with_mmap(&self.output_path, Some(total_length), None, use_mmap);

        let limiter = options
            .max_download_limit
            .filter(|&r| r > 0)
            .map(|r| RateLimiter::new(&RateLimiterConfig::new(Some(r), None)));
        if let Some(ref limiter) = limiter {
            let g = self.group.recover();
            g.set_rate_limiter(limiter.clone());
        }

        // ADR-0001: Create a control file so pause-resume works reliably.
        // Without a control file, ResumeHelper cannot distinguish a
        // preallocated file from a complete one, causing "unpause shows
        // completed". The control file stores completed_length and a
        // piece bitfield that survive across process restarts.
        let num_pieces = manager.num_segments().max(1);
        let ctrl_path = ControlFile::control_path_for(&self.output_path);
        let mut ctrl_file = match ControlFile::open_or_create(&ctrl_path, total_length, num_pieces).await {
            Ok(cf) => Some(cf),
            Err(e) => {
                tracing::warn!(
                    "Failed to create control file {}: {}. Resume will be less reliable.",
                    ctrl_path.display(), e
                );
                None
            }
        };
        // If resuming, update the control file with existing progress
        if let Some(ref mut cf) = ctrl_file {
            if resume_state.should_resume && resume_state.start_offset > 0 {
                cf.update_completed_length(resume_state.start_offset);
            }
            if let Err(e) = cf.save().await {
                tracing::warn!("Failed to save initial control file: {}", e);
            }
        }
        // Track how many bytes have been saved to the control file so we
        // only write it periodically (every CTRL_SAVE_INTERVAL_BYTES) rather
        // than after every single chunk.
        let ctrl_save_interval = total_length / num_pieces.max(1) as u64;
        let mut ctrl_bytes_since_save: u64 = 0;

        let mut active: FuturesUnordered<SegmentFetchFuture> = FuturesUnordered::new();
        let mut active_segs: HashMap<u32, u64> = HashMap::new();
        let mut progress_handles: HashMap<u32, tokio::task::JoinHandle<()>> = HashMap::new();
        // Per-segment tracker: stores the number of bytes the listener has
        // added to total_inflight_bytes so we can roll back on failure.
        let mut seg_reported: HashMap<u32, Arc<AtomicU64>> = HashMap::new();
        let initial_completed = if resume_state.should_resume {
            resume_state.start_offset
        } else {
            0
        };
        let total_inflight_bytes = Arc::new(AtomicU64::new(initial_completed));
        let mut completed_bytes = initial_completed;

        // Write channel: segment futures send chunks as they arrive,
        // the main loop drains them to disk via tokio::select!
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<WriteChunk>();

        // Pause/remove check interval — allows the download loop to detect
        // `aria2.pause` / `aria2.remove` within ~200ms even when segment
        // futures are blocked on slow network reads.
        let mut cancel_tick = tokio::time::interval(std::time::Duration::from_millis(200));

        loop {
            // Check whether the task was removed. This is the primary
            // cancellation signal: `aria2.remove` / `aria2.forceRemove` sets
            // the RequestGroup status to `Removed`, which `is_removed()`
            // observes without blocking. We check at the top of the loop so a
            // cancellation is detected before spawning new segment fetches and
            // before awaiting the next segment completion.
            if let Err(e) = self.check_cancelled() {
                // ADR-0001: Save control file before exiting on pause/remove.
                if let Some(ref mut cf) = ctrl_file {
                    cf.update_completed_length(completed_bytes);
                    if let Err(save_err) = cf.save().await {
                        tracing::warn!("Control file save on pause/remove failed: {}", save_err);
                    }
                }
                return Err(e);
            }

            while active.len() < max_conn {
                match manager.next_pending_segment_for_mirror(0) {
                    Some((seg_idx, offset, length)) => {
                        let url = uri.to_string();
                        let dl = HttpSegmentDownloader::new(&self.client);
                        let ch = cookie_hdr.clone();
                        let headers = self.headers.clone();
                        let seg_write_tx = write_tx.clone();
                        active_segs.insert(seg_idx, offset);

                        // Create per-segment progress channel for real-time updates
                        let (seg_progress_tx, mut seg_progress_rx) =
                            mpsc::unbounded_channel::<ProgressUpdate>();
                        let progress_for_listener = Arc::clone(&self.progress);
                        let total_inflight = Arc::clone(&total_inflight_bytes);
                        let seg_offset = offset;
                        let seg_reported_arc = Arc::new(AtomicU64::new(0));

                        let seg_reported_clone = Arc::clone(&seg_reported_arc);
                        let speed_interval = std::time::Duration::from_millis(
                            constants::HTTP_SPEED_UPDATE_INTERVAL_MS,
                        );
                        let ph = tokio::spawn(async move {
                            let mut last_reported = 0u64;
                            let mut speed_sample_start = std::time::Instant::now();
                            let mut speed_sample_bytes = 0u64;
                            while let Some(update) = seg_progress_rx.recv().await {
                                // Compute delta for this segment
                                let downloaded =
                                    update.completed_bytes.saturating_sub(seg_offset);
                                let delta = downloaded.saturating_sub(last_reported);
                                if delta > 0 {
                                    last_reported = downloaded;
                                    seg_reported_clone.store(last_reported, Ordering::Relaxed);
                                    let total =
                                        total_inflight.fetch_add(delta, Ordering::Relaxed)
                                            + delta;

                                    // Lock-free progress update — no RwLock acquisition needed.
                                    progress_for_listener.set_completed_length(total);

                                    // Update download speed at regular intervals
                                    speed_sample_bytes += delta;
                                    let elapsed = speed_sample_start.elapsed();
                                    if elapsed >= speed_interval && elapsed.as_secs_f64() > 0.0 {
                                        let speed = (speed_sample_bytes as f64
                                            / elapsed.as_secs_f64())
                                            as u64;
                                        progress_for_listener.set_download_speed(speed);
                                        speed_sample_start = std::time::Instant::now();
                                        speed_sample_bytes = 0;
                                    }
                                }
                            }
                        });
                        progress_handles.insert(seg_idx, ph);
                        seg_reported.insert(seg_idx, seg_reported_arc);

                        let fut = Box::pin(async move {
                            let result = dl
                                .download_range_streaming(
                                    &url,
                                    offset,
                                    length,
                                    ch.as_deref(),
                                    &headers,
                                    Some(&seg_progress_tx),
                                    &seg_write_tx,
                                )
                                .await;
                            // Drop sender to signal progress listener to stop
                            drop(seg_progress_tx);
                            (seg_idx, result)
                        });
                        active.push(fut);
                        tracing::debug!(
                            seg_idx = seg_idx,
                            offset = offset,
                            length = length,
                            "Spawned segment fetch with progress channel"
                        );
                    }
                    None => break,
                }
            }

            if active.is_empty() {
                // Drain any remaining write chunks before checking completion
                while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
                    if let Some(ref lim) = limiter {
                        lim.acquire_download(data.len() as u64).await;
                    }
                    writer.write_bytes_at(offset, data).await.map_err(|e| {
                        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                            "Write failed: {}",
                            e
                        )))
                    })?;
                }
                if manager.is_complete() {
                    tracing::debug!("All segments complete");
                    break;
                }
                if manager.has_failed_segments() && !manager.has_pending_segments() {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: "Concurrent download: all segments failed".into(),
                        },
                    ));
                }
                tracing::warn!(
                    "Concurrent download stuck: no active or pending segments but not complete"
                );
                break;
            }

            // Drain any pending writes first (non-blocking)
            while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
                if let Some(ref lim) = limiter {
                    lim.acquire_download(data.len() as u64).await;
                }
                writer.write_bytes_at(offset, data).await.map_err(|e| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Write failed: {}",
                        e
                    )))
                })?;
            }

            // Use tokio::select! to drain writes concurrently while waiting
            // for segment completions — prevents chunks from piling up in the
            // channel while other segments are still downloading.
            tokio::select! {
                // A segment completed
                Some((seg_idx, result)) = active.next() => {
                    // Drain writes again after a segment completes
                    while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
                        if let Some(ref lim) = limiter {
                            lim.acquire_download(data.len() as u64).await;
                        }
                        writer.write_bytes_at(offset, data).await.map_err(|e| {
                            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                                "Write failed: {}",
                                e
                            )))
                        })?;
                    }

                    let _offset = active_segs.remove(&seg_idx).unwrap_or(0);

                    // Await the per-segment progress listener so all in-flight
                    // updates have been flushed before we reconcile the total.
                    if let Some(ph) = progress_handles.remove(&seg_idx) {
                        let _ = ph.await;
                    }

                    match result {
                        Ok(total_written) => {
                            manager.complete_segment(seg_idx, total_written as usize);
                            completed_bytes += total_written;

                            // ADR-0001: Update control file with segment progress.
                            // Mark the piece done and periodically save to disk.
                            if let Some(ref mut cf) = ctrl_file {
                                cf.mark_piece_done(seg_idx as usize);
                                ctrl_bytes_since_save += total_written;
                                if ctrl_bytes_since_save >= ctrl_save_interval {
                                    cf.update_completed_length(completed_bytes);
                                    if let Err(e) = cf.save().await {
                                        tracing::warn!("Control file save failed: {}", e);
                                    }
                                    ctrl_bytes_since_save = 0;
                                }
                            }
                            // The listener's progress is now committed; remove the
                            // per-segment tracker so it does not accumulate stale
                            // entries if the same seg_idx is reused on retry.
                            seg_reported.remove(&seg_idx);

                            // Note: we do NOT reset total_inflight_bytes here
                            // because other segments may have in-flight progress
                            // already accumulated via their listeners.  The atomic
                            // tracks "bytes received" (for real-time display) while
                            // completed_bytes tracks "bytes committed to disk".

                            // Use the atomic total for progress updates so that
                            // in-flight progress from concurrent segments is not
                            // overwritten by the committed-only value.
                            let display_total = total_inflight_bytes.load(Ordering::Relaxed);

                            self.progress_updater
                                .update_progress(
                                    display_total,
                                    constants::PROGRESS_UPDATE_BYTES as u64,
                                    constants::HTTP_SPEED_UPDATE_INTERVAL_MS,
                                )
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!(seg_idx = seg_idx, error = %e, "Segment download failed");
                            let is_416 = matches!(
                                &e,
                                Aria2Error::Recoverable(RecoverableError::RangeNotSatisfiable { .. })
                            );
                            if is_416 {
                                consecutive_416_count += 1;
                                total_416_count += 1;
                                tracing::warn!(
                                    seg_idx = seg_idx,
                                    consecutive_416 = consecutive_416_count,
                                    total_416 = total_416_count,
                                    "RangeNotSatisfiable (416) detected"
                                );
                                let failure_ratio = total_416_count as f64 / split as f64;
                                let threshold_exceeded = consecutive_416_count
                                    >= fallback_threshold_consecutive
                                    || failure_ratio >= fallback_threshold_ratio;
                                if threshold_exceeded {
                                    tracing::warn!(
                                        uri = uri,
                                        consecutive_416 = consecutive_416_count,
                                        failure_ratio = failure_ratio,
                                        "Fallback to sequential mode triggered due to RangeNotSatisfiable errors"
                                    );
                                    should_fallback = true;
                                    break;
                                }
                            } else {
                                consecutive_416_count = 0;
                            }
                            // Roll back in-flight progress reported by this
                            // segment's listener so the atomic does not overcount.
                            if let Some(reported) = seg_reported.remove(&seg_idx) {
                                let rollback = reported.load(Ordering::Relaxed);
                                if rollback > 0 {
                                    total_inflight_bytes.fetch_sub(rollback, Ordering::Relaxed);
                                }
                            }
                            manager.fail_segment(seg_idx);
                        }
                    }
                }
                // A write chunk arrived while segments are still running
                Some(WriteChunk { offset, data }) = write_rx.recv() => {
                    if let Some(ref lim) = limiter {
                        lim.acquire_download(data.len() as u64).await;
                    }
                    writer.write_bytes_at(offset, data).await.map_err(|e| {
                        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                            "Write failed: {}",
                            e
                        )))
                    })?;
                }
                // Periodic pause/remove check — ensures the download loop
                // detects `aria2.pause` / `aria2.remove` within ~200ms even
                // when segment futures are blocked on slow network reads.
                _ = cancel_tick.tick() => {
                    if let Err(e) = self.check_cancelled() {
                        // ADR-0001: Save control file before exiting on pause/remove.
                        if let Some(ref mut cf) = ctrl_file {
                            cf.update_completed_length(completed_bytes);
                            if let Err(save_err) = cf.save().await {
                                tracing::warn!("Control file save on pause/remove failed: {}", save_err);
                            }
                        }
                        return Err(e);
                    }
                }
            }
        }

        // Final drain: ensure all pending write chunks are flushed to disk
        while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
            if let Some(ref lim) = limiter {
                lim.acquire_download(data.len() as u64).await;
            }
            writer.write_bytes_at(offset, data).await.map_err(|e| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "Write failed: {}",
                    e
                )))
            })?;
        }

        writer.flush().await.map_err(|e| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Flush failed: {}",
                e
            )))
        })?;

        if should_fallback {
            let completed_ranges = manager.completed_ranges();
            // ADR-0001: Save control file on fallback so progress is preserved.
            if let Some(ref mut cf) = ctrl_file {
                cf.update_completed_length(completed_bytes);
                if let Err(e) = cf.save().await {
                    tracing::warn!("Control file save on fallback failed: {}", e);
                }
            }
            tracing::warn!(
                "Fallback: {} completed ranges will be preserved",
                completed_ranges.len()
            );
            return Ok(ConcurrentDownloadResult::Fallback { completed_ranges });
        }

        let final_speed = {
            let g = self.group.recover();
            let elapsed = g.elapsed_time();
            match elapsed {
                Some(d) if d.as_secs_f64() > 0.0 => {
                    (completed_bytes as f64 / d.as_secs_f64()) as u64
                }
                _ => 0,
            }
        };
        {
            self.progress.set_completed_length(completed_bytes);
            self.progress.set_download_speed(final_speed);
            self.progress.set_upload_speed(0);
            let mut g = self.group.recover_mut();
            g.complete()?;
        }

        tracing::info!(
            "Concurrent download complete: {} ({} bytes)",
            self.output_path.display(),
            completed_bytes
        );
        // ADR-0001: Delete control file on successful completion.
        // The download is done; the .aria2 file is no longer needed.
        drop(ctrl_file);
        if ctrl_path.exists() {
            if let Err(e) = tokio::fs::remove_file(&ctrl_path).await {
                tracing::debug!("Failed to delete control file on completion: {}", e);
            }
        }
        self.cookie_helper.save_cookies_if_configured();
        Ok(ConcurrentDownloadResult::Complete)
    }

    pub async fn execute_with_retry(
        &mut self,
        uri: &str,
        total_length: u64,
        resume_state: &ResumeState,
        max_retries_per_segment: u32,
    ) -> Result<ConcurrentDownloadResult> {
        tracing::info!(
            "Using concurrent download mode (split={}, max_retries/segment={})",
            self.group.recover().options().split.unwrap_or(constants::DEFAULT_SPLIT),
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
            self.execute_with_coordinator(
                &all_uris,
                total_length,
                resume_state,
                max_retries_per_segment,
            )
            .await
        } else {
            self.execute(uri, total_length, resume_state, max_retries_per_segment)
                .await
        }
    }

    async fn execute_with_coordinator(
        &mut self,
        uris: &[String],
        total_length: u64,
        resume_state: &ResumeState,
        max_retries_per_segment: u32,
    ) -> Result<ConcurrentDownloadResult> {
        let split = self.group.recover().options().split.unwrap_or(1) as u64;
        let segment_size = total_length.div_ceil(split);
        let max_conn = self
            .group
            .read()
            .unwrap()
            .options()
            .max_connection_per_server
            .unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
            as usize;

        let mirror_config = crate::engine::mirror_coordinator::MirrorConfig {
            max_connections_per_mirror: max_conn.min(split as usize),
            max_total_connections: max_conn * uris.len(),
            speed_threshold: constants::MIRROR_SPEED_THRESHOLD,
            cooldown_secs: constants::MIRROR_COOLDOWN_SECS,
            max_retries: max_retries_per_segment,
        };

        let selector = Box::new(
            crate::selector::adaptive_uri_selector::AdaptiveUriSelector::new_with_uris(
                Arc::new(crate::selector::server_stat_man::ServerStatMan::new()),
                uris.to_vec(),
            ),
        );

        let segment_manager = ConcurrentSegmentManager::new_with_selector(
            total_length,
            uris.to_vec(),
            Some(segment_size),
            Arc::new(crate::selector::server_stat_man::ServerStatMan::new()),
            selector,
        );

        let mut coordinator =
            crate::engine::mirror_coordinator::MirrorCoordinator::with_segment_manager(
                Arc::new(crate::selector::server_stat_man::ServerStatMan::new()),
                Box::new(crate::selector::uri_selector::InorderUriSelector::new()),
                segment_manager,
                mirror_config,
                uris.to_vec(),
            );

        if resume_state.should_resume {
            tracing::debug!(
                "Resume: existing {} bytes, continuing from offset {}",
                resume_state.existing_length,
                resume_state.start_offset
            );
        }

        let use_mmap = self.file_allocation == "mmap" && total_length >= self.mmap_threshold;
        let mut writer =
            CachedDiskWriter::new_with_mmap(&self.output_path, Some(total_length), None, use_mmap);
        self.progress_updater.reset(0);

        let mut consecutive_416_count = 0u32;
        let mut total_416_count = 0u32;
        let fallback_threshold_consecutive = 3u32;
        let fallback_threshold_ratio = 0.2f64;
        let mut should_fallback = false;

        while coordinator.has_pending_segments() || !coordinator.is_complete() {
            // Check whether the task was removed before spawning the next
            // segment download. This is the primary cancellation signal:
            // `aria2.remove` / `aria2.forceRemove` sets the RequestGroup
            // status to `Removed`, which `is_removed()` observes without
            // blocking.
            self.check_cancelled()?;

            while let Some((mirror_idx, mirror_url, (seg_idx, offset, length))) =
                coordinator.select_mirror_for_segment()
            {
                tracing::info!(
                    "Starting segment {} download: mirror={}, offset={}, size={}",
                    seg_idx,
                    mirror_idx,
                    offset,
                    length
                );

                let downloader = HttpSegmentDownloader::new(&self.client);
                let seg_start = Instant::now();

                let cookie_hdr = self.cookie_helper.build_cookie_header(&mirror_url);

                // Create progress channel for per-chunk progress reporting during this segment
                let (seg_progress_tx, mut seg_progress_rx) =
                    mpsc::unbounded_channel::<crate::engine::command::ProgressUpdate>();

                // Spawn listener for per-chunk progress updates
                let progress_for_listener = Arc::clone(&self.progress);
                let progress_handle = tokio::spawn(async move {
                    while let Some(update) = seg_progress_rx.recv().await {
                        // Lock-free progress update — no RwLock acquisition needed.
                        progress_for_listener.set_completed_length(update.completed_bytes);
                    }
                });

                let (write_tx, mut write_rx) =
                    mpsc::unbounded_channel::<WriteChunk>();
                let result = downloader
                    .download_range_streaming(
                        &mirror_url,
                        offset,
                        length,
                        cookie_hdr.as_deref(),
                        &self.headers,
                        Some(&seg_progress_tx),
                        &write_tx,
                    )
                    .await;

                // Drop sender to signal progress listener to stop
                drop(seg_progress_tx);
                let _ = progress_handle.await;

                // Drain all pending write chunks to disk — the download is
                // complete so all chunks have been sent into the channel.
                while let Ok(WriteChunk { offset: chunk_off, data }) = write_rx.try_recv() {
                    writer.write_bytes_at(chunk_off, data).await.map_err(|e| {
                        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                            "Write failed: {}",
                            e
                        )))
                    })?;
                }

                match result {
                    Ok(bytes_downloaded) => {
                        let elapsed = seg_start.elapsed();
                        let speed = if elapsed.as_secs_f64() > 0.0 {
                            (bytes_downloaded as f64 / elapsed.as_secs_f64()) as u64
                        } else {
                            0
                        };

                        tracing::debug!(
                            "Segment {} complete: {} bytes, speed={} B/s",
                            seg_idx,
                            bytes_downloaded,
                            speed
                        );

                        coordinator.on_segment_complete(
                            mirror_idx,
                            seg_idx,
                            bytes_downloaded as usize,
                            speed,
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Segment {} download failed (mirror={}): {}",
                            seg_idx,
                            mirror_idx,
                            e
                        );

                        let is_416 = matches!(
                            &e,
                            Aria2Error::Recoverable(RecoverableError::RangeNotSatisfiable { .. })
                        );
                        if is_416 {
                            consecutive_416_count += 1;
                            total_416_count += 1;
                            tracing::warn!(
                                seg_idx = seg_idx,
                                consecutive_416 = consecutive_416_count,
                                total_416 = total_416_count,
                                "RangeNotSatisfiable (416) detected"
                            );
                            let failure_ratio = total_416_count as f64 / split as f64;
                            let threshold_exceeded = consecutive_416_count
                                >= fallback_threshold_consecutive
                                || failure_ratio >= fallback_threshold_ratio;
                            if threshold_exceeded {
                                tracing::warn!(
                                    uri = mirror_url,
                                    consecutive_416 = consecutive_416_count,
                                    failure_ratio = failure_ratio,
                                    "Fallback to sequential mode triggered due to RangeNotSatisfiable errors"
                                );
                                should_fallback = true;
                                break;
                            }
                        } else {
                            consecutive_416_count = 0;
                        }

                        let error_code = constants::HTTP_DEFAULT_ERROR_CODE;
                        coordinator.on_segment_failed(mirror_idx, seg_idx, error_code);
                    }
                }

                let completed_bytes = coordinator.completed_bytes();

                self.progress_updater
                    .update_progress(
                        completed_bytes,
                        constants::PROGRESS_UPDATE_BYTES as u64,
                        constants::HTTP_SPEED_UPDATE_INTERVAL_MS,
                    )
                    .await;
            }

            if coordinator.is_complete() {
                break;
            }

            if coordinator.has_failed_segments() {
                tracing::error!("Permanently failed download segments exist");
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "Some download segments permanently failed".into(),
                    },
                ));
            }

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        writer.flush().await.map_err(|e| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Flush failed: {}",
                e
            )))
        })?;

        if should_fallback {
            let completed_ranges = coordinator.completed_ranges();
            tracing::warn!(
                "Fallback: {} completed ranges will be preserved",
                completed_ranges.len()
            );
            return Ok(ConcurrentDownloadResult::Fallback { completed_ranges });
        }

        let final_speed = {
            let g = self.group.recover();
            let elapsed = g.elapsed_time();
            match elapsed {
                Some(d) if d.as_secs_f64() > 0.0 => {
                    (self.progress_updater.last_progress_update() as f64 / d.as_secs_f64()) as u64
                }
                _ => 0,
            }
        };

        {
            let last = self.progress_updater.last_progress_update();
            self.progress.set_total_length(last);
            self.progress.set_completed_length(last);
            self.progress.set_download_speed(final_speed);
            self.progress.set_upload_speed(0);
            let mut g = self.group.recover_mut();
            g.complete()?;
        }

        tracing::info!(
            "Multi-mirror concurrent download complete: {} ({} bytes, {} B/s)",
            self.output_path.display(),
            self.progress_updater.last_progress_update(),
            final_speed
        );
        self.cookie_helper.save_cookies_if_configured();
        Ok(ConcurrentDownloadResult::Complete)
    }
}

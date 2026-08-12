//! Single-mirror concurrent download loop.
//!
//! Contains the execute function that runs the pooled segment download
//! pipeline for a single URI.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;

use crate::constants;
use crate::engine::command::ProgressUpdate;
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::http_adaptive_concurrency::{AdaptiveOutcome, HttpAdaptiveConcurrency};
use crate::engine::http_connection_pool::{HttpConnectionPool, HttpSegmentJob, server_key};
use crate::engine::http_segment_downloader::WriteChunk;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::filesystem::resume_helper::ResumeState;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use crate::util::rwlock_ext::RwLockRecover;

use super::{ConcurrentDownloadResult, ConcurrentDownloader};

/// Run the single-mirror concurrent download pipeline.
///
/// Schedules segments onto a long-lived HTTP connection pool, drains write
/// chunks via tokio::select!, and handles 416-based fallback detection.
pub async fn execute(
    dl: &mut ConcurrentDownloader,
    uri: &str,
    total_length: u64,
    resume_state: &ResumeState,
    max_retries_per_segment: u32,
) -> Result<ConcurrentDownloadResult> {
    {
        dl.group.recover().set_total_length(total_length);
    }

    let options = dl.group.recover().options_arc();
    let split = options.split.unwrap_or(constants::DEFAULT_SPLIT) as usize;
    let max_conn = options
        .max_connection_per_server
        .unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
        .clamp(1, 16) as usize;
    let server_key = server_key(uri).unwrap_or_else(|| uri.to_string());
    let per_server_limit = max_conn.min(split);
    let mut adaptive = HttpAdaptiveConcurrency::new(per_server_limit, options.retry_wait);
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
        dl.progress_updater.reset(resume_state.start_offset);
        tracing::debug!(
            "Resume: marked {} bytes as completed, continuing from offset {}",
            resume_state.existing_length,
            resume_state.start_offset
        );
    } else {
        dl.progress_updater.reset(0);
    }

    let cookie_hdr = dl.cookie_helper.build_cookie_header(uri);

    let use_mmap = dl.file_allocation == "mmap" && total_length >= dl.mmap_threshold;
    let mut writer =
        CachedDiskWriter::new_with_mmap(&dl.output_path, Some(total_length), None, use_mmap);

    let limiter = options
        .max_download_limit
        .filter(|&r| r > 0)
        .map(|r| RateLimiter::new(&RateLimiterConfig::new(Some(r), None)));
    if let Some(ref limiter) = limiter {
        let g = dl.group.recover();
        g.set_rate_limiter(limiter.clone());
    }

    // ADR-0001: Create a control file so pause-resume works reliably.
    // Without a control file, ResumeHelper cannot distinguish a
    // preallocated file from a complete one, causing "unpause shows
    // completed". The control file stores completed_length and a
    // piece bitfield that survive across process restarts.
    let num_pieces = manager.num_segments().max(1);
    let ctrl_path = ControlFile::control_path_for(&dl.output_path);
    dl.group.recover().set_control_file_path(ctrl_path.clone());
    let mut ctrl_file =
        match ControlFile::open_or_create(&ctrl_path, total_length, num_pieces).await {
            Ok(cf) => Some(cf),
            Err(e) => {
                tracing::warn!(
                    "Failed to create control file {}: {}. Resume will be less reliable.",
                    ctrl_path.display(),
                    e
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
    let mut pool = HttpConnectionPool::new(
        &dl.client,
        dl.request_policy.clone(),
        dl.cookie_helper.clone(),
        dl.auth_options.clone(),
        dl.netrc_path.clone(),
        per_server_limit,
        std::slice::from_ref(&server_key),
        per_server_limit,
    );

    // Pause/remove check interval — allows the download loop to detect
    // aria2.pause / aria2.remove within ~200ms even when segment
    // futures are blocked on slow network reads.
    let mut cancel_tick = tokio::time::interval(std::time::Duration::from_millis(200));

    loop {
        // Check whether the task was removed. This is the primary
        // cancellation signal: aria2.remove / aria2.forceRemove sets
        // the RequestGroup status to Removed, which is_removed()
        // observes without blocking. We check at the top of the loop so a
        // cancellation is detected before spawning new segment fetches and
        // before awaiting the next segment completion.
        if let Err(e) = dl.check_cancelled() {
            // ADR-0001: Save control file before exiting on pause/remove.
            if let Some(ref mut cf) = ctrl_file {
                cf.update_completed_length(completed_bytes);
                if let Err(save_err) = cf.save().await {
                    tracing::warn!("Control file save on pause/remove failed: {}", save_err);
                }
            }
            return Err(e);
        }

        while adaptive.can_start(pool.in_flight_for(&server_key)) {
            match manager.next_pending_segment_for_mirror(0) {
                Some((seg_idx, offset, length)) => {
                    // Create per-segment progress channel for real-time updates
                    let (seg_progress_tx, mut seg_progress_rx) =
                        mpsc::unbounded_channel::<ProgressUpdate>();
                    let progress_for_listener = Arc::clone(&dl.progress);
                    let total_inflight = Arc::clone(&total_inflight_bytes);
                    let seg_offset = offset;
                    let seg_reported_arc = Arc::new(AtomicU64::new(0));

                    let seg_reported_clone = Arc::clone(&seg_reported_arc);
                    let speed_interval =
                        std::time::Duration::from_millis(constants::HTTP_SPEED_UPDATE_INTERVAL_MS);
                    let ph = tokio::spawn(async move {
                        let mut last_reported = 0u64;
                        let mut speed_sample_start = std::time::Instant::now();
                        let mut speed_sample_bytes = 0u64;
                        while let Some(update) = seg_progress_rx.recv().await {
                            // Compute delta for this segment
                            let downloaded = update.completed_bytes.saturating_sub(seg_offset);
                            let delta = downloaded.saturating_sub(last_reported);
                            if delta > 0 {
                                last_reported = downloaded;
                                seg_reported_clone.store(last_reported, Ordering::Relaxed);
                                let total =
                                    total_inflight.fetch_add(delta, Ordering::Relaxed) + delta;

                                // Lock-free progress update — no RwLock acquisition needed.
                                progress_for_listener.set_completed_length(total);

                                // Update download speed at regular intervals
                                speed_sample_bytes += delta;
                                let elapsed = speed_sample_start.elapsed();
                                if elapsed >= speed_interval && elapsed.as_secs_f64() > 0.0 {
                                    let speed =
                                        (speed_sample_bytes as f64 / elapsed.as_secs_f64()) as u64;
                                    progress_for_listener.set_download_speed(speed);
                                    speed_sample_start = std::time::Instant::now();
                                    speed_sample_bytes = 0;
                                }
                            }
                        }
                    });
                    progress_handles.insert(seg_idx, ph);
                    seg_reported.insert(seg_idx, seg_reported_arc);
                    active_segs.insert(seg_idx, offset);

                    let submitted = pool.try_submit(HttpSegmentJob {
                        mirror_index: 0,
                        segment_index: seg_idx,
                        server_key: server_key.clone(),
                        url: uri.to_string(),
                        offset,
                        length,
                        cookie_header: cookie_hdr.clone(),
                        progress_tx: seg_progress_tx,
                        write_tx: write_tx.clone(),
                        expected_entity_length: total_length,
                    });
                    if !submitted {
                        active_segs.remove(&seg_idx);
                        if let Some(ph) = progress_handles.remove(&seg_idx) {
                            ph.abort();
                        }
                        seg_reported.remove(&seg_idx);
                        manager.requeue_segment(seg_idx);
                        break;
                    }
                    tracing::debug!(
                        seg_idx = seg_idx,
                        offset = offset,
                        length = length,
                        "Submitted segment to HTTP connection pool"
                    );
                }
                None => break,
            }
        }

        if pool.in_flight() == 0 {
            // Drain any remaining write chunks before checking completion
            while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
                if let Some(ref lim) = limiter {
                    lim.acquire_download(data.len() as u64).await;
                }
                if let Some(ref gl) = dl.global_limiter
                    && gl.is_download_limited()
                {
                    gl.acquire_download(data.len() as u64).await;
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
            if let Some(new_target) = adaptive.finish_round() {
                pool.set_target(&server_key, new_target);
                tracing::info!(
                    old_target = adaptive.hard_limit().min(max_conn),
                    new_target,
                    "HTTP adaptive concurrency reduced after 429/503"
                );
            }
            if let Some(wait) = adaptive.cooldown_remaining() {
                tokio::time::sleep(wait).await;
                continue;
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
            if let Some(ref gl) = dl.global_limiter
                && gl.is_download_limited()
            {
                gl.acquire_download(data.len() as u64).await;
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
            Some(pool_result) = pool.next_result() => {
                let seg_idx = pool_result.segment_index;
                let result = pool_result.result;
                let peer_addr = pool_result.peer_addr;
                if let Some(peer_addr) = peer_addr
                    && let Ok(url) = reqwest::Url::parse(uri)
                        && let Some(host) = url.host_str()
                    {
                        dl.group.recover().set_connection_context(
                            crate::network::ConnectionContext::new(
                                host,
                                url.port_or_known_default().unwrap_or(80),
                                peer_addr,
                            ),
                        );
                    }
                // Drain writes again after a segment completes
                while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
                    if let Some(ref lim) = limiter {
                        lim.acquire_download(data.len() as u64).await;
                    }
                    if let Some(ref gl) = dl.global_limiter
                        && gl.is_download_limited() {
                            gl.acquire_download(data.len() as u64).await;
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
                        adaptive.record(AdaptiveOutcome::Success);
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

                        dl.progress_updater
                            .update_progress(
                                display_total,
                                constants::PROGRESS_UPDATE_BYTES as u64,
                                constants::HTTP_SPEED_UPDATE_INTERVAL_MS,
                            )
                            .await;
                    }
                    Err(e) => {
                        tracing::warn!(seg_idx = seg_idx, error = %e, "Segment download failed");
                        let is_capacity_limited = matches!(
                            &e,
                            Aria2Error::Recoverable(RecoverableError::ServerError { code })
                                if matches!(*code, 429 | 503)
                        );
                        adaptive.record(if is_capacity_limited {
                            AdaptiveOutcome::CapacityLimited
                        } else {
                            AdaptiveOutcome::OtherFailure
                        });
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
                        if is_capacity_limited && adaptive.preserve_retry_budget() {
                            manager.requeue_segment(seg_idx);
                        } else {
                            manager.fail_segment(seg_idx);
                        }
                    }
                }
            }
            // A write chunk arrived while segments are still running
            Some(WriteChunk { offset, data }) = write_rx.recv() => {
                if let Some(ref lim) = limiter {
                    lim.acquire_download(data.len() as u64).await;
                }
                if let Some(ref gl) = dl.global_limiter
                    && gl.is_download_limited() {
                        gl.acquire_download(data.len() as u64).await;
                    }
                writer.write_bytes_at(offset, data).await.map_err(|e| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Write failed: {}",
                        e
                    )))
                })?;
            }
            // Periodic pause/remove check — ensures the download loop
            // detects aria2.pause / aria2.remove within ~200ms even
            // when segment futures are blocked on slow network reads.
            _ = cancel_tick.tick() => {
                if let Err(e) = dl.check_cancelled() {
                    // Flush already-received chunks before persisting progress.
                    while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
                        writer.write_bytes_at(offset, data).await.map_err(|write_err| {
                            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                                "Write failed while cancelling: {}",
                                write_err
                            )))
                        })?;
                    }
                    writer.flush().await.map_err(|flush_err| {
                        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                            "Flush failed while cancelling: {}",
                            flush_err
                        )))
                    })?;
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

    // Stop workers before the final drain. This is immediate on Range
    // fallback so cancelled requests cannot enqueue more chunks afterward.
    if should_fallback {
        pool.cancel().await;
    } else {
        pool.shutdown().await;
    }

    // Final drain: ensure all pending write chunks are flushed to disk
    while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
        if let Some(ref lim) = limiter {
            lim.acquire_download(data.len() as u64).await;
        }
        if let Some(ref gl) = dl.global_limiter
            && gl.is_download_limited()
        {
            gl.acquire_download(data.len() as u64).await;
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
        // Only fully completed segments are reusable. Any bytes already
        // written for an active/failed segment remain outside this list
        // and are deliberately covered by the subsequent full gap.
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
        let g = dl.group.recover();
        let elapsed = g.elapsed_time();
        match elapsed {
            Some(d) if d.as_secs_f64() > 0.0 => (completed_bytes as f64 / d.as_secs_f64()) as u64,
            _ => 0,
        }
    };
    {
        dl.progress.set_completed_length(completed_bytes);
        dl.progress.set_download_speed(final_speed);
        dl.progress.set_upload_speed(0);
        let mut g = dl.group.recover_mut();
        g.complete()?;
    }

    tracing::info!(
        "Concurrent download complete: {} ({} bytes)",
        dl.output_path.display(),
        completed_bytes
    );
    // ADR-0001: Delete control file on successful completion.
    // The download is done; the .aria2 file is no longer needed.
    drop(ctrl_file);
    if ctrl_path.exists()
        && let Err(e) = tokio::fs::remove_file(&ctrl_path).await
    {
        tracing::debug!("Failed to delete control file on completion: {}", e);
    }
    dl.cookie_helper.save_cookies_if_configured();
    Ok(ConcurrentDownloadResult::Complete)
}

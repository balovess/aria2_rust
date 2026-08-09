//! Multi-mirror concurrent download pipeline.
//!
//! Contains the execute_with_coordinator function that uses a
//! MirrorCoordinator to assign segments to mirrors adaptively.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::constants;
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::http_segment_downloader::{HttpSegmentDownloader, WriteChunk};
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::filesystem::resume_helper::ResumeState;
use crate::util::rwlock_ext::RwLockRecover;

use super::{ConcurrentDownloadResult, ConcurrentDownloader};

/// Run the multi-mirror concurrent download pipeline.
///
/// Uses MirrorCoordinator with an AdaptiveUriSelector to assign
/// segments to the best-performing mirror. Handles 416-based fallback
/// detection so the caller can fall back to sequential mode when the
/// server does not support Range requests.
pub async fn execute_with_coordinator(
    dl: &mut ConcurrentDownloader,
    uris: &[String],
    total_length: u64,
    resume_state: &ResumeState,
    max_retries_per_segment: u32,
) -> Result<ConcurrentDownloadResult> {
    let split = dl.group.recover().options().split.unwrap_or(1) as u64;
    let segment_size = total_length.div_ceil(split);
    let max_conn = dl
        .group
        .read()
        .unwrap()
        .options()
        .max_connection_per_server
        .unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16) as usize;

    let mirror_config = crate::engine::mirror_coordinator::MirrorConfig {
        max_connections_per_mirror: max_conn.min(split as usize),
        max_total_connections: max_conn * uris.len(),
        speed_threshold: constants::MIRROR_SPEED_THRESHOLD,
        cooldown_secs: constants::MIRROR_COOLDOWN_SECS,
        max_retries: max_retries_per_segment,
    };

    let selector = Box::new(
        crate::selector::adaptive_uri_selector::AdaptiveUriSelector::new_with_uris(
            crate::selector::server_stat_man::ServerStatMan::shared().clone(),
            uris.to_vec(),
        ),
    );

    let segment_manager = ConcurrentSegmentManager::new_with_selector(
        total_length,
        uris.to_vec(),
        Some(segment_size),
        crate::selector::server_stat_man::ServerStatMan::shared().clone(),
        selector,
    );

    let mut coordinator =
        crate::engine::mirror_coordinator::MirrorCoordinator::with_segment_manager(
            crate::selector::server_stat_man::ServerStatMan::shared().clone(),
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

    let use_mmap = dl.file_allocation == "mmap" && total_length >= dl.mmap_threshold;
    let mut writer =
        CachedDiskWriter::new_with_mmap(&dl.output_path, Some(total_length), None, use_mmap);
    dl.progress_updater.reset(0);

    let mut consecutive_416_count = 0u32;
    let mut total_416_count = 0u32;
    let fallback_threshold_consecutive = 3u32;
    let fallback_threshold_ratio = 0.2f64;
    let mut should_fallback = false;

    while coordinator.has_pending_segments() || !coordinator.is_complete() {
        // Check whether the task was removed before spawning the next
        // segment download. This is the primary cancellation signal:
        // aria2.remove / aria2.forceRemove sets the RequestGroup
        // status to Removed, which is_removed() observes without
        // blocking.
        dl.check_cancelled()?;

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

            let downloader = HttpSegmentDownloader::new(&dl.client);
            let seg_start = Instant::now();

            let cookie_hdr = dl.cookie_helper.build_cookie_header(&mirror_url);

            // Create progress channel for per-chunk progress reporting during this segment
            let (seg_progress_tx, mut seg_progress_rx) =
                mpsc::unbounded_channel::<crate::engine::command::ProgressUpdate>();

            // Spawn listener for per-chunk progress updates
            let progress_for_listener = Arc::clone(&dl.progress);
            let progress_handle = tokio::spawn(async move {
                while let Some(update) = seg_progress_rx.recv().await {
                    // Lock-free progress update — no RwLock acquisition needed.
                    progress_for_listener.set_completed_length(update.completed_bytes);
                }
            });

            let (write_tx, mut write_rx) = mpsc::unbounded_channel::<WriteChunk>();
            let result = downloader
                .download_range_streaming(
                    &mirror_url,
                    offset,
                    length,
                    cookie_hdr.as_deref(),
                    &dl.headers,
                    Some(&seg_progress_tx),
                    &write_tx,
                    total_length,
                )
                .await;

            // Drop sender to signal progress listener to stop
            drop(seg_progress_tx);
            let _ = progress_handle.await;

            // Drain all pending write chunks to disk — the download is
            // complete so all chunks have been sent into the channel.
            while let Ok(WriteChunk {
                offset: chunk_off,
                data,
            }) = write_rx.try_recv()
            {
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

                    let completed_len = usize::try_from(bytes_downloaded).map_err(|_| {
                        Aria2Error::Fatal(crate::error::FatalError::Config(
                            "Completed segment length exceeds platform limits".into(),
                        ))
                    })?;
                    if !coordinator.on_segment_complete(mirror_idx, seg_idx, completed_len, speed) {
                        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                            format!(
                                "Segment {} completed with invalid length {}",
                                seg_idx, bytes_downloaded
                            ),
                        )));
                    }
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

            dl.progress_updater
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
        let g = dl.group.recover();
        let elapsed = g.elapsed_time();
        match elapsed {
            Some(d) if d.as_secs_f64() > 0.0 => {
                (dl.progress_updater.last_progress_update() as f64 / d.as_secs_f64()) as u64
            }
            _ => 0,
        }
    };

    {
        let last = dl.progress_updater.last_progress_update();
        dl.progress.set_total_length(last);
        dl.progress.set_completed_length(last);
        dl.progress.set_download_speed(final_speed);
        dl.progress.set_upload_speed(0);
        let mut g = dl.group.recover_mut();
        g.complete()?;
    }

    tracing::info!(
        "Multi-mirror concurrent download complete: {} ({} bytes, {} B/s)",
        dl.output_path.display(),
        dl.progress_updater.last_progress_update(),
        final_speed
    );
    dl.cookie_helper.save_cookies_if_configured();
    Ok(ConcurrentDownloadResult::Complete)
}

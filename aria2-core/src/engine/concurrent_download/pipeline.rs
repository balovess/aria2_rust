//! Multi-mirror concurrent download pipeline.
//!
//! Mirror selection remains owned by `MirrorCoordinator`; all HTTP range
//! requests are executed by one bounded, long-lived connection pool.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::constants;
use crate::engine::command::ProgressUpdate;
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::http_adaptive_concurrency::{AdaptiveOutcome, HttpAdaptiveConcurrency};
use crate::engine::http_connection_pool::{HttpConnectionPool, HttpSegmentJob, server_key};
use crate::engine::http_segment_downloader::WriteChunk;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::filesystem::resume_helper::ResumeState;
use crate::util::rwlock_ext::RwLockRecover;

use super::{ConcurrentDownloadResult, ConcurrentDownloader};

/// Run the multi-mirror concurrent download pipeline.
pub async fn execute_with_coordinator(
    dl: &mut ConcurrentDownloader,
    uris: &[String],
    total_length: u64,
    resume_state: &ResumeState,
    max_retries_per_segment: u32,
) -> Result<ConcurrentDownloadResult> {
    let split = dl.group.recover().options().split.unwrap_or(1) as usize;
    let segment_size = total_length.div_ceil(split as u64);
    let max_conn = dl
        .group
        .recover()
        .options()
        .max_connection_per_server
        .unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
        .clamp(1, 16) as usize;

    let mirror_config = crate::engine::mirror_coordinator::MirrorConfig {
        max_connections_per_mirror: max_conn.min(split),
        max_total_connections: max_conn.min(split),
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
    let mut segment_manager = segment_manager;
    segment_manager.set_max_connections_per_mirror(max_conn.min(split));
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

    let fallback_threshold_consecutive = 3u32;
    let fallback_threshold_ratio = 0.2f64;
    let mut consecutive_416_count = 0u32;
    let mut total_416_count = 0u32;
    let mut should_fallback = false;

    let server_keys: Vec<String> = uris
        .iter()
        .map(|uri| server_key(uri).unwrap_or_else(|| uri.clone()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let per_server_limit = max_conn.min(split).max(1);
    let pool_limit = per_server_limit
        .saturating_mul(server_keys.len())
        .min(split)
        .max(1);
    let retry_wait = dl.group.recover().options().retry_wait;
    let mut adaptive = HashMap::new();
    for key in &server_keys {
        adaptive.insert(
            key.clone(),
            HttpAdaptiveConcurrency::new(per_server_limit, retry_wait),
        );
    }
    let mut pool = HttpConnectionPool::new(
        &dl.client,
        dl.request_policy.clone(),
        dl.cookie_helper.clone(),
        dl.auth_options.clone(),
        dl.netrc_path.clone(),
        pool_limit,
        &server_keys,
        per_server_limit,
    );
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<WriteChunk>();
    let mut active: HashMap<u32, (usize, Instant)> = HashMap::new();
    let mut progress_handles: HashMap<u32, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut cancel_tick = tokio::time::interval(std::time::Duration::from_millis(200));

    while coordinator.has_pending_segments() || !coordinator.is_complete() {
        dl.check_cancelled()?;

        // Each authority closes its feedback round independently. A slow
        // mirror must not delay a capacity decision for another server.
        for (key, controller) in &mut adaptive {
            if pool.in_flight_for(key) == 0
                && let Some(new_target) = controller.finish_round()
            {
                pool.set_target(key, new_target);
                tracing::info!(
                    server = key,
                    new_target,
                    "HTTP adaptive concurrency reduced after 429/503"
                );
            }
        }

        let mut scheduling_attempts = 0usize;
        while scheduling_attempts < uris.len().max(1) * per_server_limit {
            let excluded_mirrors: Vec<usize> = uris
                .iter()
                .enumerate()
                .filter_map(|(mirror_idx, key)| {
                    let controller = adaptive.get_mut(key)?;
                    (!controller.can_start(pool.in_flight_for(key))).then_some(mirror_idx)
                })
                .collect();
            let Some((mirror_idx, mirror_url, (seg_idx, offset, length))) =
                coordinator.select_mirror_for_segment_excluding(&excluded_mirrors)
            else {
                break;
            };
            scheduling_attempts += 1;
            let key = server_key(&mirror_url).unwrap_or_else(|| mirror_url.clone());

            let (seg_progress_tx, mut seg_progress_rx) =
                mpsc::unbounded_channel::<ProgressUpdate>();
            let progress_for_listener = Arc::clone(&dl.progress);
            let progress_handle = tokio::spawn(async move {
                while let Some(update) = seg_progress_rx.recv().await {
                    progress_for_listener.set_completed_length(update.completed_bytes);
                }
            });

            let submitted = pool.try_submit(HttpSegmentJob {
                mirror_index: mirror_idx,
                segment_index: seg_idx,
                server_key: key,
                url: mirror_url.clone(),
                offset,
                length,
                cookie_header: dl.cookie_helper.build_cookie_header(&mirror_url),
                progress_tx: seg_progress_tx,
                write_tx: write_tx.clone(),
                expected_entity_length: total_length,
            });
            if !submitted {
                progress_handle.abort();
                coordinator.requeue_segment(seg_idx);
                break;
            }

            active.insert(seg_idx, (mirror_idx, Instant::now()));
            progress_handles.insert(seg_idx, progress_handle);
            tracing::debug!(
                seg_idx,
                mirror_idx,
                offset,
                length,
                "Submitted segment to HTTP connection pool"
            );
        }

        while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
            writer.write_bytes_at(offset, data).await.map_err(|e| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "Write failed: {}",
                    e
                )))
            })?;
        }

        if coordinator.is_complete() {
            break;
        }

        if pool.in_flight() == 0 {
            if let Some(wait) = adaptive
                .values()
                .filter_map(|c| c.cooldown_remaining())
                .max()
            {
                tokio::time::sleep(wait).await;
                continue;
            }
            if coordinator.has_failed_segments() {
                tracing::error!("Permanently failed download segments exist");
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "Some download segments permanently failed".into(),
                    },
                ));
            }
            if coordinator.has_pending_segments() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        }

        tokio::select! {
            Some(pool_result) = pool.next_result() => {
                let seg_idx = pool_result.segment_index;
                let Some((mirror_idx, seg_start)) = active.remove(&seg_idx) else {
                    continue;
                };
                if let Some(progress_handle) = progress_handles.remove(&seg_idx) {
                    let _ = progress_handle.await;
                }

                let result_server_key = pool_result.server_key.clone();
                match pool_result.result {
                    Ok(bytes_downloaded) => {
                        if let Some(controller) = adaptive.get_mut(&result_server_key) {
                            controller.record(AdaptiveOutcome::Success);
                        }
                        let elapsed = seg_start.elapsed();
                        let speed = if elapsed.as_secs_f64() > 0.0 {
                            (bytes_downloaded as f64 / elapsed.as_secs_f64()) as u64
                        } else {
                            0
                        };
                        let completed_len = usize::try_from(bytes_downloaded).map_err(|_| {
                            Aria2Error::Fatal(crate::error::FatalError::Config(
                                "Completed segment length exceeds platform limits".into(),
                            ))
                        })?;
                        if !coordinator.on_segment_complete(mirror_idx, seg_idx, completed_len, speed) {
                            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                                format!("Segment {} completed with invalid length {}", seg_idx, bytes_downloaded),
                            )));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(seg_idx, mirror_idx, error = %e, "Pooled segment download failed");
                        let is_capacity_limited = matches!(
                            &e,
                            Aria2Error::Recoverable(RecoverableError::ServerError { code })
                                if matches!(*code, 429 | 503)
                        );
                        if let Some(controller) = adaptive.get_mut(&result_server_key) {
                            controller.record(if is_capacity_limited {
                                AdaptiveOutcome::CapacityLimited
                            } else {
                                AdaptiveOutcome::OtherFailure
                            });
                        }
                        let is_416 = matches!(
                            &e,
                            Aria2Error::Recoverable(RecoverableError::RangeNotSatisfiable { .. })
                        );
                        if is_416 {
                            consecutive_416_count += 1;
                            total_416_count += 1;
                            let failure_ratio = total_416_count as f64 / split as f64;
                            should_fallback = consecutive_416_count >= fallback_threshold_consecutive
                                || failure_ratio >= fallback_threshold_ratio;
                        } else {
                            consecutive_416_count = 0;
                        }
                        if should_fallback {
                            break;
                        }
                        let preserve_retry_budget = adaptive
                            .get(&result_server_key)
                            .is_some_and(HttpAdaptiveConcurrency::preserve_retry_budget);
                        if is_capacity_limited && preserve_retry_budget {
                            coordinator.requeue_segment(seg_idx);
                        } else {
                            coordinator.on_segment_failed(
                                mirror_idx,
                                seg_idx,
                                constants::HTTP_DEFAULT_ERROR_CODE,
                            );
                        }
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
            Some(WriteChunk { offset, data }) = write_rx.recv() => {
                writer.write_bytes_at(offset, data).await.map_err(|e| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Write failed: {}",
                        e
                    )))
                })?;
            }
            _ = cancel_tick.tick() => {
                dl.check_cancelled()?;
            }
        }

        if should_fallback {
            break;
        }
    }

    if should_fallback {
        pool.cancel().await;
    } else {
        pool.shutdown().await;
    }
    while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
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

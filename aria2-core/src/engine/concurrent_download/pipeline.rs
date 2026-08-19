//! Multi-mirror concurrent download pipeline.
//!
//! Mirror selection remains owned by `MirrorCoordinator`; all HTTP range
//! requests are executed by one dynamic request executor.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::constants;
use crate::engine::command::WRITE_CHANNEL_CAPACITY;
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::http_adaptive_concurrency::{AdaptiveOutcome, HttpAdaptiveConcurrency};
use crate::engine::http_segment_downloader::{SegmentProgress, SegmentProgressTracker, WriteChunk};
use crate::engine::http_segment_request_executor::{
    HttpSegmentRequest, HttpSegmentRequestExecutor, authority_key,
};
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::filesystem::resume_helper::ResumeState;
use crate::util::rwlock_ext::RwLockRecover;

use super::{
    ConcurrentDownloadResult, ConcurrentDownloader, effective_segment_count,
    flush_requested_control_file,
};

/// Run the multi-mirror concurrent download pipeline.
pub async fn execute_with_coordinator(
    dl: &mut ConcurrentDownloader,
    uris: &[String],
    total_length: u64,
    resume_state: &ResumeState,
    max_retries_per_segment: u32,
) -> Result<ConcurrentDownloadResult> {
    let requested_split = dl
        .group
        .recover()
        .options()
        .split
        .unwrap_or(constants::DEFAULT_SPLIT);
    let min_split_size = dl.group.recover().effective_min_split_size();
    let split = effective_segment_count(total_length, requested_split, min_split_size);
    let segment_size = total_length.div_ceil(split as u64).max(1);
    let max_conn = dl
        .group
        .recover()
        .options()
        .max_connection_per_server
        .unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
        .clamp(1, 16) as usize;

    let mirror_config = crate::engine::mirror_coordinator::MirrorConfig {
        max_connections_per_mirror: split,
        max_total_connections: split,
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
    // Segment selection may offer any mirror up to the per-download split
    // budget. Authority admission below applies the real server cap and
    // shares it across mirrors with the same scheme/host/port.
    segment_manager.set_max_connections_per_mirror(split);
    let mut coordinator =
        crate::engine::mirror_coordinator::MirrorCoordinator::with_segment_manager(
            crate::selector::server_stat_man::ServerStatMan::shared().clone(),
            Box::new(crate::selector::uri_selector::InorderUriSelector::new()),
            segment_manager,
            mirror_config,
            uris.to_vec(),
        );

    let use_mmap = dl.file_allocation == "mmap" && total_length >= dl.mmap_threshold;
    let disk_cache = dl.group.recover().options().disk_cache_size_bytes();
    let mut writer = CachedDiskWriter::new_with_mmap_bytes(
        &dl.output_path,
        Some(total_length),
        disk_cache,
        use_mmap,
    );

    let num_pieces = coordinator.num_segments().max(1);
    let ctrl_path = ControlFile::control_path_for(&dl.output_path);
    dl.group.recover().set_control_file_path(ctrl_path.clone());
    let expected_bitfield_len = num_pieces.div_ceil(8);
    let persisted_prefix = resume_state
        .control_file
        .as_ref()
        .map(ControlFile::completed_length)
        .filter(|&length| length > 0)
        .or_else(|| {
            (resume_state.control_file.is_none() && resume_state.should_resume)
                .then_some(resume_state.start_offset)
        })
        .unwrap_or(0);
    let compatible_control_file = resume_state.control_file.as_ref().filter(|control_file| {
        control_file.total_length() == total_length
            && control_file.bitfield().len() == expected_bitfield_len
    });

    // ResumeHelper is the authority for whether a sidecar belongs to this
    // attempt. In particular, continue=false deliberately returns no control
    // file even when an old sidecar is present; do not let open_or_create()
    // resurrect that state behind the resume seam.
    let has_untrusted_control_file = resume_state.control_file.is_none() && ctrl_path.exists();
    let can_initialize_new_control_file = if compatible_control_file.is_some() {
        true
    } else if has_untrusted_control_file || resume_state.control_file.is_some() {
        match tokio::fs::remove_file(&ctrl_path).await {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                tracing::warn!(
                    path = %ctrl_path.display(),
                    %error,
                    "Failed to replace stale multi-mirror control file"
                );
                false
            }
        }
    } else {
        true
    };
    // A file-length-only resume is safe only when there is no untrusted
    // sidecar left behind. If replacing that sidecar failed, the file and
    // its progress metadata cannot be reconciled, so restart the segments
    // instead of trusting ResumeState::start_offset.
    let can_restore_prefix = resume_state.control_file.is_none()
        && resume_state.should_resume
        && (!has_untrusted_control_file || can_initialize_new_control_file);

    let mut ctrl_file = if let Some(control_file) = compatible_control_file {
        Some(control_file.clone())
    } else if can_initialize_new_control_file {
        if has_untrusted_control_file || resume_state.control_file.is_some() {
            tracing::debug!(
                path = %ctrl_path.display(),
                "Discarding stale control-file layout before multi-mirror resume"
            );
        }
        match ControlFile::open_or_create(&ctrl_path, total_length, num_pieces).await {
            Ok(control_file) => Some(control_file),
            Err(error) => {
                tracing::warn!(
                    path = %ctrl_path.display(),
                    %error,
                    "Failed to create multi-mirror control file; resume will be less reliable"
                );
                None
            }
        }
    } else {
        None
    };

    let restored_bytes = if let Some(control_file) = ctrl_file.as_ref() {
        let restored = if compatible_control_file.is_some() && control_file.completed_pieces() > 0 {
            coordinator.restore_completed_from_bitfield(control_file.bitfield())
        } else if compatible_control_file.is_some() || can_restore_prefix {
            coordinator.restore_completed_prefix(persisted_prefix)
        } else {
            0
        };
        if resume_state.should_resume {
            tracing::debug!(
                existing_length = resume_state.existing_length,
                start_offset = resume_state.start_offset,
                restored_bytes = restored,
                "Resuming multi-mirror download from persisted segment state"
            );
        }
        restored
    } else if can_restore_prefix {
        let restored = coordinator.restore_completed_prefix(resume_state.start_offset);
        tracing::debug!(
            existing_length = resume_state.existing_length,
            start_offset = resume_state.start_offset,
            restored_bytes = restored,
            "Resuming multi-mirror download from conservative completed prefix"
        );
        restored
    } else {
        0
    };
    dl.progress_updater.reset(restored_bytes);
    dl.progress.set_completed_length(restored_bytes);

    if let Some(control_file) = ctrl_file.as_mut() {
        control_file.update_completed_length(restored_bytes);
        if let Err(error) = control_file.save().await {
            tracing::warn!(%error, "Failed to save initial multi-mirror control file");
        }
    }
    flush_requested_control_file(
        dl,
        &mut writer,
        &mut ctrl_file,
        coordinator.completed_bytes(),
    )
    .await?;
    let ctrl_save_interval = (total_length / num_pieces as u64).max(1);
    let mut ctrl_bytes_since_save = 0u64;

    let fallback_threshold_consecutive = 3u32;
    let fallback_threshold_ratio = 0.2f64;
    let mut consecutive_416_count = 0u32;
    let mut total_416_count = 0u32;
    let mut should_fallback = false;

    let server_keys: Vec<String> = uris
        .iter()
        .map(|uri| authority_key(uri).unwrap_or_else(|| uri.clone()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let retry_wait = dl.group.recover().options().retry_wait;
    let retry_policy = RetryPolicy::new(max_retries_per_segment, retry_wait.saturating_mul(1000));
    let mut adaptive = HashMap::new();
    for key in &server_keys {
        adaptive.insert(
            key.clone(),
            HttpAdaptiveConcurrency::new(max_conn, retry_wait),
        );
    }
    let mut executor = HttpSegmentRequestExecutor::new(
        &dl.client,
        dl.request_policy.clone(),
        dl.cookie_helper.clone(),
        dl.auth_options.clone(),
        dl.netrc_path.clone(),
        split,
        &server_keys,
        max_conn,
    );
    let (write_tx, mut write_rx) = mpsc::channel::<WriteChunk>(WRITE_CHANNEL_CAPACITY);
    let mut active: HashMap<u32, (usize, Instant)> = HashMap::new();
    let progress_tracker =
        SegmentProgressTracker::new(coordinator.completed_bytes(), Arc::clone(&dl.progress));
    let mut segment_progress: HashMap<u32, Arc<SegmentProgress>> = HashMap::new();
    // Lifecycle changes wake the scheduler even when all segment requests are
    // blocked on slow network reads.
    let lifecycle_notify = dl.group.recover().lifecycle_notifier();

    while coordinator.has_pending_segments() || !coordinator.is_complete() {
        let lifecycle_changed = lifecycle_notify.notified();
        tokio::pin!(lifecycle_changed);
        lifecycle_changed.as_mut().enable();

        if let Err(error) = dl.check_cancelled() {
            super::segment::cancel_and_persist(
                executor,
                &mut write_rx,
                &mut writer,
                None,
                dl.global_limiter.as_ref(),
                &mut ctrl_file,
                coordinator.completed_bytes(),
            )
            .await?;
            return Err(error);
        }

        // Each authority closes its feedback round independently. A slow
        // mirror must not delay a capacity decision for another server.
        for (key, controller) in &mut adaptive {
            if executor.in_flight_for(key) == 0
                && let Some(new_target) = controller.finish_round()
            {
                executor.set_target(key, new_target);
                tracing::info!(
                    server = key,
                    new_target,
                    "HTTP adaptive concurrency reduced after 429/503"
                );
            }
        }

        let mut scheduling_attempts = 0usize;
        while scheduling_attempts < uris.len().max(1) * split {
            let excluded_mirrors: Vec<usize> = uris
                .iter()
                .enumerate()
                .filter_map(|(mirror_idx, uri)| {
                    let key = authority_key(uri).unwrap_or_else(|| uri.clone());
                    let controller = adaptive.get_mut(&key)?;
                    (!controller.can_start(executor.in_flight_for(&key))).then_some(mirror_idx)
                })
                .collect();
            let Some((mirror_idx, mirror_url, (seg_idx, offset, length))) =
                coordinator.select_mirror_for_segment_excluding(&excluded_mirrors)
            else {
                break;
            };
            scheduling_attempts += 1;
            let key = authority_key(&mirror_url).unwrap_or_else(|| mirror_url.clone());

            let progress = progress_tracker.new_segment();

            let submitted = executor.try_submit(HttpSegmentRequest {
                mirror_index: mirror_idx,
                segment_index: seg_idx,
                authority_key: key,
                url: mirror_url.clone(),
                offset,
                length,
                cookie_header: dl.cookie_helper.build_cookie_header(&mirror_url),
                progress: Arc::clone(&progress),
                write_tx: write_tx.clone(),
                expected_entity_length: total_length,
            });
            if !submitted {
                segment_progress.remove(&seg_idx);
                coordinator.requeue_segment(seg_idx);
                break;
            }

            active.insert(seg_idx, (mirror_idx, Instant::now()));
            segment_progress.insert(seg_idx, progress);
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
        flush_requested_control_file(
            dl,
            &mut writer,
            &mut ctrl_file,
            coordinator.completed_bytes(),
        )
        .await?;

        if coordinator.is_complete() {
            break;
        }

        if executor.in_flight() == 0 {
            if let Some(wait) = adaptive
                .values()
                .filter_map(|c| c.cooldown_remaining())
                .max()
            {
                dl.wait_for_retry(wait).await?;
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
                let message = if coordinator.any_mirror_available() {
                    "HTTP segment scheduler made no progress"
                } else {
                    "HTTP segment download has no available mirrors"
                };
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: message.into(),
                    },
                ));
            }
        }

        tokio::select! {
            Some(pool_result) = executor.next_result() => {
                let seg_idx = pool_result.segment_index;
                let Some((mirror_idx, seg_start)) = active.remove(&seg_idx) else {
                    continue;
                };
                let segment_progress_for_result = segment_progress.remove(&seg_idx);

                let result_authority_key = pool_result.authority_key.clone();
                match pool_result.result {
                    Ok(bytes_downloaded) => {
                        if let Some(controller) = adaptive.get_mut(&result_authority_key) {
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
                        if let Some(control_file) = ctrl_file.as_mut() {
                            control_file.mark_piece_done(seg_idx as usize);
                            ctrl_bytes_since_save =
                                ctrl_bytes_since_save.saturating_add(bytes_downloaded);
                            if ctrl_bytes_since_save >= ctrl_save_interval {
                                control_file.update_completed_length(coordinator.completed_bytes());
                                if let Err(error) = control_file.save().await {
                                    tracing::warn!(%error, "Failed to save multi-mirror control file");
                                }
                                ctrl_bytes_since_save = 0;
                            }
                        }
                    }
                    Err(e) => {
                        if let Some(progress) = segment_progress_for_result {
                            progress.rollback();
                        }
                        let e = if matches!(
                            &e,
                            Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
                        ) {
                            dl.group.recover().file_not_found_error()
                        } else {
                            e
                        };
                        tracing::warn!(seg_idx, mirror_idx, error = %e, "Pooled segment download failed");
                        let file_not_found_retry_allowed = !matches!(
                            &e,
                            Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
                        ) && dl.group.recover().can_retry_file_not_found();
                        if matches!(
                            &e,
                            Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
                        ) {
                            super::segment::cancel_and_persist(
                                executor,
                                &mut write_rx,
                                &mut writer,
                                None,
                                dl.global_limiter.as_ref(),
                                &mut ctrl_file,
                                coordinator.completed_bytes(),
                            )
                            .await?;
                            return Err(e);
                        }
                        let error_code = super::server_stat_error_code(&e);
                        let is_capacity_limited = matches!(
                            &e,
                            Aria2Error::Recoverable(RecoverableError::ServerError { code })
                                if matches!(*code, 429 | 503)
                        );
                        if let Some(controller) = adaptive.get_mut(&result_authority_key) {
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
                        let retry_count = coordinator.segment_retry_count(seg_idx);
                        let retry_allowed = super::should_retry_segment(
                            &retry_policy,
                            retry_count,
                            &e,
                            file_not_found_retry_allowed,
                        );
                        if !retry_allowed {
                            let failed_over = coordinator.num_mirrors() > 1
                                && coordinator
                                    .on_terminal_segment_failed(seg_idx, error_code)
                                    .is_some();
                            if !failed_over {
                                super::segment::cancel_and_persist(
                                    executor,
                                    &mut write_rx,
                                    &mut writer,
                                    None,
                                    dl.global_limiter.as_ref(),
                                    &mut ctrl_file,
                                    coordinator.completed_bytes(),
                                )
                                .await?;
                                return Err(e);
                            }
                        } else {
                            let preserve_retry_budget = adaptive
                                .get(&result_authority_key)
                                .is_some_and(HttpAdaptiveConcurrency::preserve_retry_budget);
                            if is_capacity_limited && preserve_retry_budget {
                                coordinator.requeue_segment(seg_idx);
                            } else {
                                coordinator.on_segment_failed(
                                    mirror_idx,
                                    seg_idx,
                                    error_code,
                                );
                            }
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
                flush_requested_control_file(
                    dl,
                    &mut writer,
                    &mut ctrl_file,
                    coordinator.completed_bytes(),
                )
                .await?;
            }
            Some(WriteChunk { offset, data }) = write_rx.recv() => {
                writer.write_bytes_at(offset, data).await.map_err(|e| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Write failed: {}",
                        e
                    )))
                })?;
                flush_requested_control_file(
                    dl,
                    &mut writer,
                    &mut ctrl_file,
                    coordinator.completed_bytes(),
                )
                .await?;
            }
            _ = &mut lifecycle_changed => {
                flush_requested_control_file(
                    dl,
                    &mut writer,
                    &mut ctrl_file,
                    coordinator.completed_bytes(),
                )
                .await?;
                if let Err(error) = dl.check_cancelled() {
                    super::segment::cancel_and_persist(
                        executor,
                        &mut write_rx,
                        &mut writer,
                        None,
                        dl.global_limiter.as_ref(),
                        &mut ctrl_file,
                        coordinator.completed_bytes(),
                    )
                    .await?;
                    return Err(error);
                }
            }
        }

        if should_fallback {
            break;
        }
    }

    if should_fallback {
        super::segment::cancel_and_persist(
            executor,
            &mut write_rx,
            &mut writer,
            None,
            dl.global_limiter.as_ref(),
            &mut ctrl_file,
            coordinator.completed_bytes(),
        )
        .await?;
    } else {
        executor.shutdown().await;
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
    let progress_stats = progress_tracker.stats();
    tracing::debug!(
        segments = progress_stats.segments,
        progress_updates = progress_stats.updates,
        progress_rollbacks = progress_stats.rollbacks,
        "HTTP multi-mirror progress aggregation summary"
    );
    if let Some(control_file) = ctrl_file.as_mut() {
        control_file.update_completed_length(coordinator.completed_bytes());
        if let Err(error) = control_file.save().await {
            tracing::warn!(%error, "Failed to save final multi-mirror control file");
        }
    }
    drop(ctrl_file);
    if ctrl_path.exists()
        && let Err(error) = tokio::fs::remove_file(&ctrl_path).await
    {
        tracing::debug!(%error, "Failed to delete multi-mirror control file on completion");
    }
    dl.cookie_helper.save_cookies_if_configured();
    Ok(ConcurrentDownloadResult::Complete)
}

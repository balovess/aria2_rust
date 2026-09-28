//! Single-mirror concurrent download loop.
//!
//! Contains the execute function that runs the pooled segment download
//! pipeline for a single URI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::constants;
use crate::engine::command::WRITE_CHANNEL_CAPACITY;
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::http_adaptive_concurrency::HttpAdaptiveConcurrency;
use crate::engine::http_segment_downloader::{SegmentProgress, SegmentProgressTracker, WriteChunk};
use crate::engine::http_segment_request_executor::{
    HttpSegmentRequest, HttpSegmentRequestExecutor, authority_key,
};
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::filesystem::resume_helper::ResumeState;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use crate::request::request_group::ActiveConnectionGuard;
use crate::util::rwlock_ext::RwLockRecover;

use super::fixed_piece_size::calculate_fixed_piece_size;
use super::{ConcurrentDownloadResult, ConcurrentDownloader, effective_segment_count};

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
    let requested_split = options.split.unwrap_or(constants::DEFAULT_SPLIT);
    let min_split_size = dl.group.recover().effective_min_split_size();
    let split = effective_segment_count(total_length, requested_split, min_split_size);
    let piece_size = resume_state
        .control_file
        .as_ref()
        .filter(|control_file| {
            control_file.total_length() == total_length && !control_file.is_torrent_checkpoint()
        })
        .and_then(ControlFile::piece_length)
        .map(u64::from)
        .unwrap_or_else(|| calculate_fixed_piece_size(total_length));
    let piece_length = u32::try_from(piece_size).map_err(|_| {
        Aria2Error::InvalidArgument(format!(
            "HTTP fixed piece length is not representable: {piece_size}"
        ))
    })?;
    let max_conn = options
        .max_connection_per_server
        .unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
        .clamp(1, 16) as usize;
    let retry_policy = RetryPolicy::new(
        max_retries_per_segment,
        options.retry_wait.saturating_mul(1000),
    );
    let authority_key = authority_key(uri).unwrap_or_else(|| uri.to_string());
    let session_limit = options
        .max_http2_sessions_per_server
        .unwrap_or(constants::DEFAULT_HTTP2_SESSIONS_PER_SERVER as u16)
        .clamp(1, max_conn as u16) as usize;
    let session_limit = session_limit.min(dl.range_clients.len().max(1));
    let mut adaptive = HttpAdaptiveConcurrency::new(
        split,
        max_conn,
        session_limit,
        options.http2_streams_per_session(),
        options.retry_wait,
    );
    let seg_size = piece_size;

    tracing::info!(
        "Concurrent download started: split_budget={}, requested_split={}, min_split_size={}, max_conn={}, fixed_piece_size={} bytes, total={}",
        split,
        requested_split,
        min_split_size,
        max_conn,
        seg_size,
        total_length
    );

    let mut manager =
        ConcurrentSegmentManager::new(total_length, vec![uri.to_string()], Some(seg_size));
    // MirrorState is only a segment-selection capacity. The actual server
    // limit is enforced by HttpSegmentRequestExecutor, which also merges
    // multiple URLs sharing one authority.
    manager.set_max_connections_per_mirror(split);
    manager.set_max_retries(max_retries_per_segment);

    let mut consecutive_416_count = 0u32;
    let mut total_416_count = 0u32;
    let fallback_threshold_consecutive = 3u32;
    let fallback_threshold_ratio = 0.2f64;
    let mut should_fallback = false;

    let cookie_hdr = dl.cookie_helper.build_cookie_header(uri);

    let use_mmap = dl.file_allocation == "mmap" && total_length >= dl.mmap_threshold;
    let mut writer = CachedDiskWriter::new_with_mmap_bytes(
        &dl.output_path,
        Some(total_length),
        options.disk_cache_size_bytes(),
        use_mmap,
    );

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
    let expected_bitfield_len = num_pieces.div_ceil(8);
    let compatible_control_file = resume_state.control_file.as_ref().filter(|control_file| {
        control_file.total_length() == total_length
            && !control_file.is_torrent_checkpoint()
            && control_file.piece_length() == Some(piece_length)
            && control_file.bitfield().len() == expected_bitfield_len
    });
    let has_untrusted_control_file = resume_state.control_file.is_none() && ctrl_path.exists();
    let can_initialize_control_file = if compatible_control_file.is_some() {
        true
    } else if has_untrusted_control_file || resume_state.control_file.is_some() {
        match tokio::fs::remove_file(&ctrl_path).await {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                tracing::warn!(
                    path = %ctrl_path.display(),
                    %error,
                    "Failed to replace stale single-source control file"
                );
                false
            }
        }
    } else {
        true
    };
    let can_restore_prefix = resume_state.control_file.is_none()
        && resume_state.should_resume
        && (!has_untrusted_control_file || can_initialize_control_file);
    let mut ctrl_file = if let Some(control_file) = compatible_control_file {
        Some(control_file.clone())
    } else if can_initialize_control_file {
        match ControlFile::open_or_create_with_piece_length(&ctrl_path, total_length, piece_length)
            .await
        {
            Ok(cf) => Some(cf),
            Err(e) => {
                tracing::warn!(
                    "Failed to create control file {}: {}. Resume will be less reliable.",
                    ctrl_path.display(),
                    e
                );
                None
            }
        }
    } else {
        None
    };

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
    let initial_completed = if let Some(control_file) = ctrl_file.as_ref() {
        if compatible_control_file.is_some() && control_file.completed_pieces() > 0 {
            manager.restore_completed_from_bitfield(control_file.bitfield())
        } else if compatible_control_file.is_some() || can_restore_prefix {
            manager.restore_completed_prefix(persisted_prefix)
        } else {
            0
        }
    } else if can_restore_prefix {
        manager.restore_completed_prefix(resume_state.start_offset)
    } else {
        0
    };
    dl.progress_updater.reset(initial_completed);
    dl.progress.set_completed_length(initial_completed);
    if resume_state.should_resume {
        tracing::debug!(
            existing_length = resume_state.existing_length,
            start_offset = resume_state.start_offset,
            restored_bytes = initial_completed,
            "Resuming single-source download from persisted segment state"
        );
    }

    // Persist the restored progress before issuing more ranges.
    if let Some(ref mut cf) = ctrl_file {
        cf.update_completed_length(initial_completed);
        if let Err(e) = cf.save().await {
            tracing::warn!("Failed to save initial control file: {}", e);
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

    let mut active_segs: HashMap<u32, (u64, std::time::Instant, u64)> = HashMap::new();
    let progress_tracker = SegmentProgressTracker::new(initial_completed, Arc::clone(&dl.progress));
    let mut segment_progress: HashMap<u32, Arc<SegmentProgress>> = HashMap::new();
    let mut completed_bytes = initial_completed;
    let segment_stall_timeout =
        Duration::from_secs(constants::HTTP_DEFAULT_SEGMENT_STALL_TIMEOUT_SECS);
    let stall_check_interval = Duration::from_secs(1);
    let mut stall_check = tokio::time::interval(stall_check_interval);
    stall_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    super::flush_requested_control_file(dl, &mut writer, &mut ctrl_file, completed_bytes).await?;

    // Write channel: segment futures send chunks as they arrive,
    // the main loop drains them to disk via tokio::select!
    let (write_tx, mut write_rx) = mpsc::channel::<WriteChunk>(WRITE_CHANNEL_CAPACITY);
    let mut executor = HttpSegmentRequestExecutor::new_with_clients(
        &dl.client,
        dl.range_clients.as_slice(),
        dl.request_policy.clone(),
        dl.cookie_helper.clone(),
        dl.auth_options.clone(),
        dl.netrc_path.clone(),
        split,
        std::slice::from_ref(&authority_key),
        max_conn,
    );
    if let Some((known_authority, version)) = &dl.initial_http_protocol
        && known_authority == &authority_key
    {
        executor.set_protocol(&authority_key, *version);
        let is_http2 = *version == reqwest::Version::HTTP_2;
        executor.set_target(&authority_key, adaptive.range_target(is_http2));
        if is_http2 {
            executor.set_active_h2_sessions(&authority_key, adaptive.connection_target(is_http2));
        }
    }
    let connection_guard = ActiveConnectionGuard::new(Arc::clone(&dl.group));

    // Lifecycle changes wake the scheduler even when all segment requests are
    // blocked on slow network reads.
    let lifecycle_notify = dl.group.recover().lifecycle_notifier();

    loop {
        let lifecycle_changed = lifecycle_notify.notified();
        tokio::pin!(lifecycle_changed);
        lifecycle_changed.as_mut().enable();

        // Check whether the task was removed. This is the primary
        // cancellation signal: aria2.remove / aria2.forceRemove sets
        // the RequestGroup status to Removed, which is_removed()
        // observes without blocking. We check at the top of the loop so a
        // cancellation is detected before spawning new segment fetches and
        // before awaiting the next segment completion.
        if let Err(e) = dl.check_cancelled() {
            cancel_and_persist(
                executor,
                &mut write_rx,
                &mut writer,
                limiter.as_ref(),
                dl.global_limiter.as_ref(),
                &mut ctrl_file,
                completed_bytes,
            )
            .await?;
            return Err(e);
        }

        let is_http2 = executor.is_http2(&authority_key);
        if executor.in_flight_for(&authority_key) == 0 && !manager.is_complete() {
            let update = adaptive.finish_round(is_http2);
            if let Some(connections) = update.connection_target {
                tracing::info!(
                    target_connections = connections,
                    range_target = adaptive.range_target(is_http2),
                    split_budget = split,
                    "HTTP adaptive physical connection count changed"
                );
            }
        }
        let is_http2 = executor.is_http2(&authority_key);
        executor.set_target(&authority_key, adaptive.range_target(is_http2));
        if is_http2 {
            executor.set_active_h2_sessions(&authority_key, adaptive.connection_target(is_http2));
        }

        while adaptive.can_start(
            executor.in_flight_for(&authority_key),
            executor.is_http2(&authority_key),
        ) {
            match manager.next_pending_segment_for_mirror(0) {
                Some((seg_idx, offset, length)) => {
                    let progress = progress_tracker.new_segment();
                    segment_progress.insert(seg_idx, Arc::clone(&progress));
                    active_segs.insert(seg_idx, (offset, std::time::Instant::now(), 0));
                    let submitted = executor.try_submit(HttpSegmentRequest {
                        segment_index: seg_idx,
                        authority_key: authority_key.clone(),
                        url: uri.to_string(),
                        offset,
                        length,
                        cookie_header: cookie_hdr.clone(),
                        progress,
                        write_tx: write_tx.clone(),
                        expected_entity_length: total_length,
                    });
                    let Some(task_id) = submitted else {
                        active_segs.remove(&seg_idx);
                        segment_progress.remove(&seg_idx);
                        manager.requeue_segment(seg_idx);
                        break;
                    };
                    if let Some(active) = active_segs.get_mut(&seg_idx) {
                        active.2 = task_id;
                    }
                    connection_guard.set(executor.in_flight());
                    tracing::debug!(
                        seg_idx = seg_idx,
                        task_id,
                        offset = offset,
                        length = length,
                        "Submitted segment to HTTP connection pool"
                    );
                }
                None => break,
            }
        }

        if executor.in_flight() == 0 {
            // Drain any remaining write chunks before checking completion
            while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
                super::acquire_download_tokens(
                    limiter.as_ref(),
                    dl.global_limiter.as_ref(),
                    data.len(),
                )
                .await;
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
            if let Some(wait) = adaptive.cooldown_remaining() {
                dl.wait_for_retry(wait).await?;
                continue;
            }
            if manager.has_failed_segments() && !manager.has_pending_segments() {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "Concurrent download: all segments failed".into(),
                    },
                ));
            }
            let incomplete_segments = (0..manager.num_segments())
                .filter_map(|index| {
                    let status = manager.segment_status(index)?;
                    (status != crate::engine::concurrent_segment_manager::SegmentStatus::Done)
                        .then_some((index, status))
                })
                .collect::<Vec<_>>();
            tracing::warn!(
                completed_bytes = manager.completed_bytes(),
                total_bytes = total_length,
                pending_segments = manager.has_pending_segments(),
                failed_segments = manager.has_failed_segments(),
                tracked_active_segments = ?active_segs.keys().collect::<Vec<_>>(),
                tracked_active_requests = ?active_segs
                    .iter()
                    .map(|(segment_index, (offset, _, task_id))| (*segment_index, *offset, *task_id))
                    .collect::<Vec<_>>(),
                finished_executor_tasks = ?executor.finished_tasks(),
                incomplete_segments = ?incomplete_segments,
                mirror_active_segments = manager.mirror_active_segments(0),
                mirror_connection_limit = manager.get_mirror_max_connections(0),
                "Concurrent download stalled; falling back to fill incomplete ranges"
            );
            should_fallback = true;
            break;
        }

        // Drain any pending writes first (non-blocking)
        while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
            super::acquire_download_tokens(
                limiter.as_ref(),
                dl.global_limiter.as_ref(),
                data.len(),
            )
            .await;
            writer.write_bytes_at(offset, data).await.map_err(|e| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "Write failed: {}",
                    e
                )))
            })?;
        }
        super::flush_requested_control_file(dl, &mut writer, &mut ctrl_file, completed_bytes)
            .await?;

        // Use tokio::select! to drain writes concurrently while waiting
        // for segment completions — prevents chunks from piling up in the
        // channel while other segments are still downloading.
        tokio::select! {
            // A segment completed
            Some(pool_result) = executor.next_result() => {
                executor.reap_task(pool_result.task_id).await;
                connection_guard.set(executor.in_flight());
                let seg_idx = pool_result.segment_index;
                let Some((_, _segment_started_at, active_task_id)) =
                    active_segs.get(&seg_idx).copied()
                else {
                    tracing::warn!(
                        segment_index = seg_idx,
                        result_task_id = pool_result.task_id,
                        in_flight = executor.in_flight_for(&authority_key),
                        tracked_active_segments = ?active_segs.keys().collect::<Vec<_>>(),
                        "Received HTTP Range completion for an untracked segment"
                    );
                    continue;
                };
                if active_task_id != pool_result.task_id {
                    tracing::warn!(
                        segment_index = seg_idx,
                        active_task_id,
                        result_task_id = pool_result.task_id,
                        "Received HTTP Range completion for a replaced task"
                    );
                    continue;
                }
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
                    super::acquire_download_tokens(
                        limiter.as_ref(),
                        dl.global_limiter.as_ref(),
                        data.len(),
                    )
                    .await;
                    writer.write_bytes_at(offset, data).await.map_err(|e| {
                        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                            "Write failed: {}",
                            e
                        )))
                    })?;
                }

                let _active = active_segs.remove(&seg_idx);

                // The request has emitted its completion only after all
                // progress writes, so the atomic segment handle is already
                // fully reconciled here.
                let segment_progress_for_result = segment_progress.remove(&seg_idx);

                match result {
                    Ok(total_written) => {
                        let Some(parent_complete) = manager.complete_range(seg_idx, total_written)
                        else {
                            return Err(Aria2Error::Fatal(
                                crate::error::FatalError::Config(format!(
                                    "HTTP segment scheduler rejected completed range for parent {seg_idx}"
                                )),
                            ));
                        };
                        completed_bytes += total_written;

                        // ADR-0001: Update control file with segment progress.
                        // Mark the piece done and periodically save to disk.
                        if let Some(ref mut cf) = ctrl_file {
                            if parent_complete {
                                cf.mark_piece_done(seg_idx as usize);
                            }
                            ctrl_bytes_since_save += total_written;
                            if ctrl_bytes_since_save >= ctrl_save_interval {
                                cf.update_completed_length(completed_bytes);
                                if let Err(e) = cf.save().await {
                                    tracing::warn!("Control file save failed: {}", e);
                                }
                                ctrl_bytes_since_save = 0;
                            }
                        }
                        // Use the atomic total for progress updates so that
                        // in-flight progress from concurrent segments is not
                        // overwritten by the committed-only value.
                        let display_total = progress_tracker.total();

                        dl.progress_updater
                            .update_progress(
                                display_total,
                                constants::PROGRESS_UPDATE_BYTES as u64,
                                constants::HTTP_SPEED_UPDATE_INTERVAL_MS,
                            )
                            .await;
                        super::flush_requested_control_file(
                            dl,
                            &mut writer,
                            &mut ctrl_file,
                            completed_bytes,
                        )
                        .await?;
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
                        tracing::warn!(seg_idx = seg_idx, error = %e, "Segment download failed");
                        let is_http2 = executor.is_http2(&authority_key);
                        let is_file_not_found = matches!(
                            &e,
                            Aria2Error::Recoverable(
                                RecoverableError::ResourceNotFound
                                    | RecoverableError::MaxFileNotFound
                            )
                        );
                        let file_not_found_retry_allowed = !matches!(
                            &e,
                            Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
                        ) && dl.group.recover().can_retry_file_not_found();
                        if is_file_not_found && !file_not_found_retry_allowed {
                            cancel_and_persist(
                                executor,
                                &mut write_rx,
                                &mut writer,
                                limiter.as_ref(),
                                dl.global_limiter.as_ref(),
                                &mut ctrl_file,
                                completed_bytes,
                            )
                            .await?;
                            return Err(e);
                        }
                        let is_capacity_limited = super::is_capacity_limited_error(&e);
                        if is_capacity_limited {
                            adaptive.record_capacity_failure(is_http2);
                        }
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
                        let retry_count = manager.segment_retry_count(seg_idx);
                        let adaptive_capacity_retry = is_capacity_limited
                            && adaptive.preserve_retry_budget(is_http2);
                        let retry_allowed = adaptive_capacity_retry
                            || super::should_retry_segment(
                                &retry_policy,
                                retry_count,
                                &e,
                                file_not_found_retry_allowed,
                            );
                        if !retry_allowed {
                            cancel_and_persist(
                                executor,
                                &mut write_rx,
                                &mut writer,
                                limiter.as_ref(),
                                dl.global_limiter.as_ref(),
                                &mut ctrl_file,
                                completed_bytes,
                            )
                            .await?;
                            return Err(e);
                        }
                        if adaptive_capacity_retry {
                            manager.requeue_segment(seg_idx);
                        } else {
                            manager.fail_segment(seg_idx);
                        }
                    }
                }
            }
            // A write chunk arrived while segments are still running
            Some(WriteChunk { offset, data }) = write_rx.recv() => {
                super::acquire_download_tokens(
                    limiter.as_ref(),
                    dl.global_limiter.as_ref(),
                    data.len(),
                )
                .await;
                writer.write_bytes_at(offset, data).await.map_err(|e| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Write failed: {}",
                        e
                    )))
                })?;
                super::flush_requested_control_file(
                    dl,
                    &mut writer,
                    &mut ctrl_file,
                    completed_bytes,
                )
                .await?;
            }
            // Lifecycle changes wake the loop immediately, even when segment
            // futures are blocked on slow network reads.
            _ = &mut lifecycle_changed => {
                super::flush_requested_control_file(
                    dl,
                    &mut writer,
                    &mut ctrl_file,
                    completed_bytes,
                )
                .await?;
                if let Err(e) = dl.check_cancelled() {
                    cancel_and_persist(
                        executor,
                        &mut write_rx,
                        &mut writer,
                        limiter.as_ref(),
                        dl.global_limiter.as_ref(),
                        &mut ctrl_file,
                        completed_bytes,
                    )
                    .await?;
                    return Err(e);
                }
            }
            _ = stall_check.tick() => {
                let stalled_segment = active_segs.keys().find_map(|seg_idx| {
                    segment_progress
                        .get(seg_idx)
                        .filter(|progress| progress.is_stalled(segment_stall_timeout))
                        .map(|_| *seg_idx)
                });
                if let Some(seg_idx) = stalled_segment
                    && executor.abort_segment(seg_idx).await
                {
                    connection_guard.set(executor.in_flight());
                    active_segs.remove(&seg_idx);
                    let last_throughput_bps = if let Some(progress) = segment_progress.remove(&seg_idx) {
                        let throughput = progress.recent_throughput_bps();
                        progress.rollback();
                        throughput
                    } else {
                        0
                    };
                    manager.fail_segment(seg_idx);
                    tracing::warn!(
                        seg_idx,
                        stall_timeout_secs = segment_stall_timeout.as_secs(),
                        last_throughput_bps,
                        "Reclaimed fully stalled HTTP Range request"
                    );
                }
            }
        }
    }

    // Stop workers before the final drain. This is immediate on Range
    // fallback so cancelled requests cannot enqueue more chunks afterward.
    if should_fallback {
        executor.cancel().await;
    } else {
        executor.shutdown().await;
    }

    // Final drain: ensure all pending write chunks are flushed to disk
    while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
        super::acquire_download_tokens(limiter.as_ref(), dl.global_limiter.as_ref(), data.len())
            .await;
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
    let progress_stats = progress_tracker.stats();
    tracing::debug!(
        segments = progress_stats.segments,
        progress_updates = progress_stats.updates,
        progress_rollbacks = progress_stats.rollbacks,
        "HTTP segment progress aggregation summary"
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

#[allow(clippy::too_many_arguments)]
pub(super) async fn cancel_and_persist(
    executor: HttpSegmentRequestExecutor,
    write_rx: &mut mpsc::Receiver<WriteChunk>,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    global_limiter: Option<&RateLimiter>,
    ctrl_file: &mut Option<ControlFile>,
    completed_bytes: u64,
) -> Result<()> {
    executor.cancel().await;

    while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
        super::acquire_download_tokens(limiter, global_limiter, data.len()).await;
        writer.write_bytes_at(offset, data).await.map_err(|error| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Write failed while cancelling: {error}"
            )))
        })?;
    }

    writer.flush().await.map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Flush failed while cancelling: {error}"
        )))
    })?;

    if let Some(ctrl_file) = ctrl_file {
        ctrl_file.update_completed_length(completed_bytes);
        if let Err(error) = ctrl_file.save().await {
            tracing::warn!("Control file save on pause/remove failed: {error}");
        }
    }

    Ok(())
}

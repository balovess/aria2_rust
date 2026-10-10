//! Multi-mirror concurrent download pipeline.
//!
//! Mirror selection remains owned by `MirrorCoordinator`; all HTTP range
//! requests are executed by one dynamic request executor.

use std::sync::Arc;
use std::time::Instant;

use crate::constants;
use crate::engine::http::request_executor::{HttpSegmentRequest, authority_key};
use crate::engine::http::segment_downloader::WriteChunk;
use crate::engine::work_scheduler::{
    RetryOutcome, WorkId, WorkItem, WorkLease, WorkScheduleError, WorkScheduler,
};
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::resume_helper::ResumeState;
use crate::util::rwlock_ext::RwLockRecover;

use super::range_commit::{HttpRangeCommitOptions, commit_http_range_chunk};
use super::{ConcurrentDownloadResult, ConcurrentDownloader, flush_requested_control_file};

mod finalize;
mod scheduler;
mod setup;

fn http_work_scheduler_error(context: &str, error: WorkScheduleError) -> Aria2Error {
    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
        "HTTP work scheduler could not {context}: {error}"
    )))
}

fn record_http_work_failure(
    work_queue: &mut WorkScheduler<u32>,
    lease: WorkLease<u32>,
    retry: bool,
) -> Result<bool> {
    let retry_at = retry.then(Instant::now);
    let outcome = work_queue
        .fail(lease, retry_at)
        .map_err(|error| http_work_scheduler_error("record a failed attempt", error))?;
    match (retry, outcome) {
        (true, RetryOutcome::Scheduled) => Ok(true),
        (true, RetryOutcome::Exhausted(_)) | (false, RetryOutcome::Exhausted(_)) => Ok(false),
        (false, RetryOutcome::Scheduled) => {
            Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                "HTTP terminal work failure was unexpectedly retried".into(),
            )))
        }
    }
}

/// Run the multi-mirror concurrent download pipeline.
pub async fn execute_with_coordinator(
    dl: &mut ConcurrentDownloader,
    uris: &[String],
    total_length: u64,
    resume_state: &ResumeState,
    max_retries_per_segment: u32,
) -> Result<ConcurrentDownloadResult> {
    let setup::PreparedMultiMirrorDownload {
        options,
        split,
        fixed_piece_size,
        max_conn,
        session_limit,
        mut coordinator,
        mut writer,
        limiter,
        ctrl_path,
        mut ctrl_file,
        ctrl_save_interval,
        mut ctrl_bytes_since_save,
    } = setup::prepare(
        dl,
        uris,
        total_length,
        resume_state,
        max_retries_per_segment,
    )
    .await?;
    let minimum_range_size_limit = options.min_http_range_size_bytes();
    let fallback_threshold_consecutive = 3u32;
    let fallback_threshold_ratio = 0.2f64;
    let mut consecutive_416_count = 0u32;
    let mut total_416_count = 0u32;
    let mut should_fallback = false;

    let scheduler::RangeScheduler {
        retry_policy,
        mut adaptive,
        mut range_size_limits,
        mut executor,
        connection_guard,
        write_tx,
        mut write_rx,
        mut active,
        progress_tracker,
        mut segment_progress,
        mut slow_range_recovery,
        segment_stall_timeout,
        mut stall_check,
        lifecycle_notify,
    } = scheduler::create(
        dl,
        uris,
        &options,
        max_retries_per_segment,
        split,
        max_conn,
        session_limit,
        fixed_piece_size,
        &coordinator,
    );
    let mut work_queue = WorkScheduler::new();
    for segment_index in 0..coordinator.num_segments() {
        let segment_index = u32::try_from(segment_index).map_err(|_| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "HTTP segment index exceeds the work scheduler limit".into(),
            ))
        })?;
        work_queue
            .enqueue(WorkItem::new(
                WorkId::new(u64::from(segment_index)),
                segment_index,
                retry_policy.max_tries(),
            ))
            .map_err(|error| http_work_scheduler_error("enqueue", error))?;
    }
    while coordinator.has_pending_segments() || !coordinator.is_complete() {
        let lifecycle_changed = lifecycle_notify.notified();
        tokio::pin!(lifecycle_changed);
        lifecycle_changed.as_mut().enable();

        if let Err(error) = dl.check_cancelled() {
            work_queue.cancel();
            super::segment::cancel_and_persist(
                dl,
                executor,
                &mut write_rx,
                &mut writer,
                limiter.as_ref(),
                &mut ctrl_file,
                coordinator.completed_bytes(),
            )
            .await?;
            return Err(error);
        }

        // Each authority closes its feedback round independently. A slow
        // mirror must not delay a capacity decision for another server.
        for (key, controller) in &mut adaptive {
            let is_http2 = executor.is_http2(key);
            if executor.in_flight_for(key) == 0 {
                let update = controller.finish_round(is_http2);
                if let Some(connections) = update.connection_target {
                    tracing::info!(
                        server = key,
                        target_connections = connections,
                        range_target = controller.range_target(is_http2),
                        split_budget = split,
                        "HTTP adaptive physical connection count changed"
                    );
                }
            }
            executor.set_target(key, controller.range_target(is_http2));
            if is_http2 {
                executor.set_active_h2_sessions(key, controller.connection_target(is_http2));
            }
        }

        let mut scheduling_attempts = 0usize;
        while scheduling_attempts < uris.len().max(1) * split {
            if work_queue.in_flight_count() >= split {
                break;
            }
            let Some(lease) = work_queue.admit_one(split, Instant::now()) else {
                break;
            };
            let seg_idx = *lease.payload();
            let excluded_mirrors: Vec<usize> = uris
                .iter()
                .enumerate()
                .filter_map(|(mirror_idx, uri)| {
                    let key = authority_key(uri).unwrap_or_else(|| uri.clone());
                    let controller = adaptive.get_mut(&key)?;
                    (!controller.can_start(executor.in_flight_for(&key), executor.is_http2(&key)))
                        .then_some(mirror_idx)
                })
                .collect();
            let Some((mirror_idx, mirror_url, (_, offset, length))) = coordinator
                .select_mirror_for_work_range_excluding(
                    seg_idx,
                    &excluded_mirrors,
                    &range_size_limits,
                )
            else {
                if coordinator.segment_is_complete(seg_idx) {
                    work_queue
                        .complete(lease)
                        .map_err(|error| http_work_scheduler_error("complete", error))?;
                    continue;
                }
                work_queue
                    .requeue_unstarted(lease)
                    .map_err(|error| http_work_scheduler_error("requeue", error))?;
                break;
            };
            scheduling_attempts += 1;
            let key = authority_key(&mirror_url).unwrap_or_else(|| mirror_url.clone());

            let progress = progress_tracker.new_segment();

            let submitted = executor.try_submit(HttpSegmentRequest {
                segment_index: seg_idx,
                authority_key: key.clone(),
                url: mirror_url.clone(),
                offset,
                length,
                range_size_limit: range_size_limits[mirror_idx],
                cookie_header: dl.cookie_helper.build_cookie_header(&mirror_url),
                progress: Arc::clone(&progress),
                write_tx: write_tx.clone(),
                expected_entity_length: total_length,
            });
            let Some(task_id) = submitted else {
                segment_progress.remove(&seg_idx);
                coordinator.requeue_segment(seg_idx);
                work_queue
                    .requeue_unstarted(lease)
                    .map_err(|error| http_work_scheduler_error("requeue", error))?;
                break;
            };

            connection_guard.set(executor.in_flight());
            active.insert(
                seg_idx,
                scheduler::ActiveRangeWork {
                    lease,
                    mirror_idx,
                    length,
                    started_at: Instant::now(),
                    task_id,
                },
            );
            segment_progress.insert(seg_idx, progress);
            tracing::debug!(
                seg_idx,
                mirror_idx,
                offset,
                length,
                "Submitted segment to HTTP connection pool"
            );
        }

        while let Ok(chunk) = write_rx.try_recv() {
            commit_http_range_chunk(
                dl,
                &mut writer,
                limiter.as_ref(),
                &mut ctrl_file,
                HttpRangeCommitOptions {
                    completed_bytes: coordinator.completed_bytes(),
                    flush_checkpoint: false,
                    error_context: "",
                },
                chunk,
            )
            .await?;
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

            if !coordinator.is_complete() {
                let active_segments: Vec<u32> = active.keys().copied().collect();
                tracing::warn!(
                    completed_bytes = coordinator.completed_bytes(),
                    total_bytes = total_length,
                    segment_count = coordinator.num_segments(),
                    active_segments = ?active_segments,
                    active_progress_segments = ?segment_progress.keys().collect::<Vec<_>>(),
                    "Multi-mirror HTTP download has incomplete ranges but no active work; falling back to fill gaps"
                );
                should_fallback = true;
                break;
            }
        }

        tokio::select! {
            Some(pool_result) = executor.next_result() => {
                executor.reap_task(pool_result.task_id).await;
                connection_guard.set(executor.in_flight());
                let seg_idx = pool_result.segment_index;
                let Some(active_task_id) = active.get(&seg_idx).map(|work| work.task_id) else {
                    continue;
                };
                if active_task_id != pool_result.task_id {
                    continue;
                }
                let Some(active_work) = active.remove(&seg_idx) else {
                    continue;
                };
                let scheduler::ActiveRangeWork {
                    lease,
                    mirror_idx,
                    started_at: seg_start,
                    ..
                } = active_work;
                let segment_progress_for_result = segment_progress.remove(&seg_idx);
                let rejected_range_size_limit = pool_result.range_size_limit;
                let explicit_range_size_rejection = pool_result.range_size_rejected;

                let result_authority_key = pool_result.authority_key.clone();
                match pool_result.result {
                    Ok(bytes_downloaded) => {
                        let elapsed = seg_start.elapsed();
                        let speed = if elapsed.as_secs_f64() > 0.0 {
                            (bytes_downloaded as f64 / elapsed.as_secs_f64()) as u64
                        } else {
                            0
                        };
                        let Some(parent_complete) = coordinator.on_range_complete(
                            mirror_idx,
                            seg_idx,
                            bytes_downloaded,
                            speed,
                        ) else {
                            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                                format!("Segment {} completed with invalid length {}", seg_idx, bytes_downloaded),
                            )));
                        };
                        slow_range_recovery.record_success(
                            &result_authority_key,
                            bytes_downloaded,
                            elapsed,
                        );
                        if let Some(control_file) = ctrl_file.as_mut() {
                            if parent_complete {
                                control_file.mark_piece_done(seg_idx as usize);
                            }
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
                        if parent_complete {
                            work_queue
                                .complete(lease)
                                .map_err(|error| http_work_scheduler_error("complete work", error))?;
                        } else {
                            work_queue
                                .reschedule_after_success(lease)
                                .map_err(|error| http_work_scheduler_error("reschedule a partial range", error))?;
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
                        let current_range_size_limit = scheduler::current_authority_range_size_limit(
                            uris,
                            &range_size_limits,
                            &result_authority_key,
                        );
                        let rejection_targets_current_or_old_limit = explicit_range_size_rejection
                            && current_range_size_limit
                                .is_some_and(|current| rejected_range_size_limit >= current);
                        let reduced_range_size_limit = if current_range_size_limit
                            == Some(rejected_range_size_limit)
                        {
                            scheduler::lower_authority_range_size_limit(
                                uris,
                                &mut range_size_limits,
                                &result_authority_key,
                                rejected_range_size_limit,
                                minimum_range_size_limit,
                            )
                        } else {
                            None
                        };
                        if let Some(next_limit) = reduced_range_size_limit {
                            tracing::warn!(
                                authority = %result_authority_key,
                                previous_range_size_limit = rejected_range_size_limit,
                                range_size_limit = next_limit,
                                fixed_piece_size,
                                "Reduced HTTP Range size limit after explicit server rejection"
                            );
                        }
                        let range_size_rejection_handled = rejection_targets_current_or_old_limit
                            && current_range_size_limit.is_some_and(|current| {
                                rejected_range_size_limit > current
                                    || reduced_range_size_limit.is_some()
                            });
                        if range_size_rejection_handled {
                            if !coordinator.requeue_segment(seg_idx) {
                                return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                                    format!(
                                        "Multi-mirror scheduler could not requeue size-rejected parent {seg_idx}"
                                    ),
                                )));
                            }
                            work_queue
                                .retry_without_consuming_attempt(lease)
                                .map_err(|error| http_work_scheduler_error("retry a smaller range", error))?;
                            consecutive_416_count = 0;
                        } else {
                        let file_not_found_retry_allowed = !matches!(
                            &e,
                            Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
                        ) && dl.group.recover().can_retry_file_not_found();
                        if matches!(
                            &e,
                            Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
                        ) {
                            record_http_work_failure(&mut work_queue, lease, false)?;
                            super::segment::cancel_and_persist(
                                dl,
                                executor,
                                &mut write_rx,
                                &mut writer,
                                limiter.as_ref(),
                                &mut ctrl_file,
                                coordinator.completed_bytes(),
                            )
                            .await?;
                            return Err(e);
                        }
                        let error_code = super::server_stat_error_code(&e);
                        let is_capacity_limited = super::is_capacity_limited_error(&e);
                        let is_http2 = executor.is_http2(&result_authority_key);
                        if let Some(controller) = adaptive.get_mut(&result_authority_key)
                            && is_capacity_limited
                        {
                            controller.record_capacity_failure(is_http2);
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
                            record_http_work_failure(&mut work_queue, lease, false)?;
                            break;
                        }
                        let adaptive_capacity_retry = is_capacity_limited
                            && adaptive
                                .get(&result_authority_key)
                                .is_some_and(|controller| {
                                    controller.preserve_retry_budget(is_http2)
                                });
                        let retry_count = lease.attempt().saturating_sub(1);
                        let retry_allowed = adaptive_capacity_retry
                            || super::should_retry_segment(
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
                                record_http_work_failure(&mut work_queue, lease, false)?;
                                super::segment::cancel_and_persist(
                                    dl,
                                    executor,
                                    &mut write_rx,
                                    &mut writer,
                                    limiter.as_ref(),
                                    &mut ctrl_file,
                                    coordinator.completed_bytes(),
                                )
                                .await?;
                                return Err(e);
                            }
                            work_queue
                                .retry_with_fresh_attempt_budget(lease)
                                .map_err(|error| http_work_scheduler_error("retry on another mirror", error))?;
                        } else {
                            if adaptive_capacity_retry {
                                coordinator.requeue_segment(seg_idx);
                                work_queue
                                    .retry_without_consuming_attempt(lease)
                                    .map_err(|error| http_work_scheduler_error("retry after capacity adaptation", error))?;
                            } else {
                                coordinator.on_segment_failed(
                                    mirror_idx,
                                    seg_idx,
                                    error_code,
                                );
                                let _ = record_http_work_failure(&mut work_queue, lease, true)?;
                            }
                        }
                        }
                    }
                }

                let completed_bytes = coordinator.completed_bytes();
                dl.progress_updater
                    .update_progress_without_speed(
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
                commit_http_range_chunk(
                    dl,
                    &mut writer,
                    limiter.as_ref(),
                    &mut ctrl_file,
                    HttpRangeCommitOptions {
                        completed_bytes: coordinator.completed_bytes(),
                        flush_checkpoint: true,
                        error_context: "",
                    },
                    WriteChunk { offset, data },
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
                    work_queue.cancel();
                    super::segment::cancel_and_persist(
                        dl,
                        executor,
                        &mut write_rx,
                        &mut writer,
                        limiter.as_ref(),
                        &mut ctrl_file,
                        coordinator.completed_bytes(),
                    )
                    .await?;
                    return Err(error);
                }
            }
            _ = stall_check.tick() => {
                progress_tracker.refresh_speed();
                let stalled_segment = active.iter().find_map(|(seg_idx, _)| {
                    segment_progress
                        .get(seg_idx)
                        .filter(|progress| progress.is_stalled(segment_stall_timeout))
                        .map(|_| *seg_idx)
                });
                if let Some(seg_idx) = stalled_segment
                    && executor.abort_segment(seg_idx).await
                {
                    connection_guard.set(executor.in_flight());
                    let Some(active_work) = active.remove(&seg_idx) else {
                        continue;
                    };
                    let lease = active_work.lease;
                    let mirror_idx = active_work.mirror_idx;
                    let last_throughput_bps = if let Some(progress) = segment_progress.remove(&seg_idx) {
                        let throughput = progress.recent_throughput_bps();
                        progress.rollback();
                        throughput
                    } else {
                        0
                    };
                    coordinator.on_segment_failed(mirror_idx, seg_idx, 408);
                    record_http_work_failure(&mut work_queue, lease, true)?;
                    tracing::warn!(
                        seg_idx,
                        stall_timeout_secs = segment_stall_timeout.as_secs(),
                        last_throughput_bps,
                        "Reclaimed fully stalled HTTP Range request"
                    );
                } else {
                    let slow_outlier = active.iter().find_map(
                        |(seg_idx, active_work)| {
                            let progress = segment_progress.get(seg_idx)?;
                            let mirror_url = uris.get(active_work.mirror_idx)?;
                            let key = authority_key(mirror_url)
                                .unwrap_or_else(|| mirror_url.clone());
                            slow_range_recovery
                                .slow_outlier(
                                    &key,
                                    *seg_idx,
                                    progress.downloaded_bytes(),
                                    progress.recent_throughput_bps(),
                                    active_work.length,
                                    active_work.started_at.elapsed(),
                                )
                                .map(|observation| {
                                    (*seg_idx, active_work.mirror_idx, observation)
                                })
                        },
                    );
                    if let Some((seg_idx, mirror_idx, observation)) = slow_outlier
                        && executor.abort_segment(seg_idx).await
                    {
                        connection_guard.set(executor.in_flight());
                        let Some(active_work) = active.remove(&seg_idx) else {
                            continue;
                        };
                        let lease = active_work.lease;
                        if let Some(progress) = segment_progress.remove(&seg_idx) {
                            progress.rollback();
                        }
                        if coordinator.requeue_segment(seg_idx) {
                            work_queue
                                .retry_without_consuming_attempt(lease)
                                .map_err(|error| http_work_scheduler_error("retry a slow range", error))?;
                            slow_range_recovery.mark_recovered(seg_idx);
                            tracing::warn!(
                                seg_idx,
                                mirror_idx,
                                goodput_bps = observation.goodput_bps,
                                reference_goodput_bps = observation.reference_goodput_bps,
                                estimated_remaining_ms = observation.estimated_remaining.as_millis(),
                                estimated_retry_ms = observation.estimated_retry.as_millis(),
                                downloaded_bytes = observation.downloaded_bytes,
                                range_length = observation.range_length,
                                "Requeued slow HTTP Range outlier once"
                            );
                        } else {
                            coordinator.on_segment_failed(mirror_idx, seg_idx, 408);
                            let _ = record_http_work_failure(&mut work_queue, lease, true)?;
                            tracing::warn!(
                                seg_idx,
                                mirror_idx,
                                "Slow HTTP Range recovery could not requeue its active segment"
                            );
                        }
                    }
                }
            }
        }

        if should_fallback {
            break;
        }
    }

    if should_fallback {
        work_queue.cancel();
    }

    finalize::finish(
        dl,
        should_fallback,
        executor,
        &mut write_rx,
        &mut writer,
        limiter.as_ref(),
        &coordinator,
        &mut ctrl_file,
        &ctrl_path,
        &progress_tracker,
    )
    .await
}

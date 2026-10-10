//! Single-mirror concurrent download loop.
//!
//! Contains the execute function that runs the pooled segment download
//! pipeline for a single URI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::constants;
use crate::engine::concurrent_segment_manager::SegmentStatus;
use crate::engine::http::request_executor::HttpSegmentRequest;
use crate::engine::http::segment_downloader::{
    SegmentProgress, SegmentProgressTracker, WriteChunk,
};
use crate::engine::work_scheduler::{
    RetryOutcome, WorkId, WorkItem, WorkLease, WorkScheduleError, WorkScheduler,
};
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::disk_writer::SeekableDiskWriter;
use crate::filesystem::resume_helper::ResumeState;
use crate::util::rwlock_ext::RwLockRecover;

use super::range_size_limit::lower_rejected_range_size_limit;
use super::slow_range::SlowRangeRecovery;
use super::{ConcurrentDownloadResult, ConcurrentDownloader};

mod cancel;
mod executor;
mod finalize;
mod io;
mod output;
mod plan;
mod resume;

use super::range_commit::{HttpRangeCommitOptions, write_http_range_chunk};
pub(super) use cancel::cancel_and_persist;

fn work_scheduler_error(context: &str, error: WorkScheduleError) -> Aria2Error {
    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
        "HTTP work scheduler could not {context}: {error}"
    )))
}

fn fail_work(
    work_queue: &mut WorkScheduler<u32>,
    lease: WorkLease<u32>,
    retry_at: Option<std::time::Instant>,
) -> Result<RetryOutcome<u32>> {
    work_queue
        .fail(lease, retry_at)
        .map_err(|error| work_scheduler_error("record a failed range", error))
}

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

    let plan = plan::create(dl, uri, total_length, resume_state, max_retries_per_segment)?;
    let plan::SegmentPlan {
        split,
        piece_length,
        max_conn,
        retry_policy,
        authority_key,
        mut adaptive,
        mut manager,
    } = plan;
    let options = dl.group.recover().options_arc();
    let mut range_size_limit = u64::from(piece_length);
    let minimum_range_size_limit = options.min_http_range_size_bytes();

    let mut consecutive_416_count = 0u32;
    let mut total_416_count = 0u32;
    let fallback_threshold_consecutive = 3u32;
    let fallback_threshold_ratio = 0.2f64;
    let mut should_fallback = false;

    let cookie_hdr = dl.cookie_helper.build_cookie_header(uri);

    let output = output::prepare(dl, &options, total_length);
    let mut writer = output.writer;
    let limiter = output.limiter;

    let resume::PreparedResume {
        ctrl_path,
        mut ctrl_file,
        ctrl_save_interval,
        completed_bytes,
    } = resume::prepare(
        dl,
        &mut manager,
        resume_state,
        total_length,
        piece_length,
        &mut writer,
    )
    .await?;
    let initial_completed = completed_bytes;
    let mut ctrl_bytes_since_save = 0;

    let mut active_segs: HashMap<u32, (u64, u64, std::time::Instant, u64)> = HashMap::new();
    let progress_tracker = SegmentProgressTracker::new(initial_completed, Arc::clone(&dl.progress));
    let mut segment_progress: HashMap<u32, Arc<SegmentProgress>> = HashMap::new();
    let mut active_work: HashMap<u32, WorkLease<u32>> = HashMap::new();
    let mut work_queue = WorkScheduler::new();
    for index in 0..manager.num_segments() {
        if manager.segment_status(index) != Some(SegmentStatus::Pending) {
            continue;
        }
        let segment_index = u32::try_from(index).map_err(|_| {
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
            .map_err(|error| work_scheduler_error("enqueue a range", error))?;
    }
    let mut slow_range_recovery = SlowRangeRecovery::new();
    let mut completed_bytes = initial_completed;
    let segment_stall_timeout =
        Duration::from_secs(constants::HTTP_DEFAULT_SEGMENT_STALL_TIMEOUT_SECS);
    let stall_check_interval = Duration::from_secs(1);
    let mut stall_check = tokio::time::interval(stall_check_interval);
    stall_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let executor::SegmentRequests {
        write_tx,
        mut write_rx,
        mut executor,
        connection_guard,
        lifecycle_notify,
    } = executor::create(dl, split, max_conn, &authority_key, &adaptive);

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
            work_queue.cancel();
            cancel_and_persist(
                dl,
                executor,
                &mut write_rx,
                &mut writer,
                limiter.as_ref(),
                &mut ctrl_file,
                completed_bytes,
            )
            .await?;
            return Err(e);
        }

        executor::update_capacity(
            &mut executor,
            &mut adaptive,
            &authority_key,
            split,
            manager.is_complete(),
        );

        while adaptive.can_start(
            executor.in_flight_for(&authority_key),
            executor.is_http2(&authority_key),
        ) {
            let Some(lease) = work_queue.admit_one(split, std::time::Instant::now()) else {
                break;
            };
            let seg_idx = *lease.payload();
            match manager.next_pending_range_for_segment(0, seg_idx, range_size_limit) {
                Some((_, offset, length)) => {
                    let progress = progress_tracker.new_segment();
                    segment_progress.insert(seg_idx, Arc::clone(&progress));
                    active_segs.insert(seg_idx, (offset, length, std::time::Instant::now(), 0));
                    let submitted = executor.try_submit(HttpSegmentRequest {
                        segment_index: seg_idx,
                        authority_key: authority_key.clone(),
                        url: uri.to_string(),
                        offset,
                        length,
                        range_size_limit,
                        cookie_header: cookie_hdr.clone(),
                        progress,
                        write_tx: write_tx.clone(),
                        expected_entity_length: total_length,
                    });
                    let Some(task_id) = submitted else {
                        active_segs.remove(&seg_idx);
                        segment_progress.remove(&seg_idx);
                        manager.requeue_segment(seg_idx);
                        work_queue.requeue_unstarted(lease).map_err(|error| {
                            work_scheduler_error("requeue an unstarted range", error)
                        })?;
                        break;
                    };
                    if let Some(active) = active_segs.get_mut(&seg_idx) {
                        active.3 = task_id;
                    }
                    active_work.insert(seg_idx, lease);
                    connection_guard.set(executor.in_flight());
                    tracing::debug!(
                        seg_idx = seg_idx,
                        task_id,
                        offset = offset,
                        length = length,
                        "Submitted segment to HTTP connection pool"
                    );
                }
                None => match manager.segment_status(seg_idx as usize) {
                    Some(SegmentStatus::Done | SegmentStatus::Failed) => {
                        work_queue.complete(lease).map_err(|error| {
                            work_scheduler_error("complete a terminal range", error)
                        })?;
                        continue;
                    }
                    _ => {
                        work_queue.requeue_unstarted(lease).map_err(|error| {
                            work_scheduler_error("requeue a blocked range", error)
                        })?;
                        break;
                    }
                },
            }
        }

        if executor.in_flight() == 0 {
            // Drain any remaining write chunks before checking completion
            io::drain_write_chunks(
                dl,
                &mut write_rx,
                &mut writer,
                limiter.as_ref(),
                &mut ctrl_file,
                completed_bytes,
                "",
            )
            .await?;
            if manager.is_complete() {
                tracing::debug!("All segments complete");
                break;
            }
            if let Some(wait) = adaptive.cooldown_remaining() {
                dl.wait_for_retry(wait).await?;
                continue;
            }
            if manager.has_failed_segments() && !manager.has_pending_segments() {
                work_queue.cancel();
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
                    .map(|(segment_index, (offset, _, _, task_id))| (*segment_index, *offset, *task_id))
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
        io::drain_write_chunks(
            dl,
            &mut write_rx,
            &mut writer,
            limiter.as_ref(),
            &mut ctrl_file,
            completed_bytes,
            "",
        )
        .await?;
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
                let Some((_, _, _segment_started_at, active_task_id)) =
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
                let rejected_range_size_limit = pool_result.range_size_limit;
                let explicit_range_size_rejection = pool_result.range_size_rejected;
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
                io::drain_write_chunks(
                    dl,
                    &mut write_rx,
                    &mut writer,
                    limiter.as_ref(),
                    &mut ctrl_file,
                    completed_bytes,
                    "",
                )
                .await?;

                let active_segment = active_segs.remove(&seg_idx);
                let work_lease = active_work.remove(&seg_idx).ok_or_else(|| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "HTTP work scheduler lost the active lease for range {seg_idx}"
                    )))
                })?;

                // The request has emitted its completion only after all
                // progress writes, so the atomic segment handle is already
                // fully reconciled here.
                let segment_progress_for_result = segment_progress.remove(&seg_idx);

                match result {
                    Ok(total_written) => {
                        if let Some((_, _, started_at, _)) = active_segment {
                            slow_range_recovery.record_success(
                                &authority_key,
                                total_written,
                                started_at.elapsed(),
                            );
                        }
                        let Some(parent_complete) = manager.complete_range(seg_idx, total_written)
                        else {
                            return Err(Aria2Error::Fatal(
                                crate::error::FatalError::Config(format!(
                                    "HTTP segment scheduler rejected completed range for parent {seg_idx}"
                                )),
                            ));
                        };
                        let mut payload_synced = false;
                        if parent_complete {
                            writer.sync_all().await.map_err(|error| {
                                Aria2Error::FileIo(format!(
                                    "Failed to durably sync completed HTTP range: {error}"
                                ))
                            })?;
                            payload_synced = true;
                        }
                        completed_bytes += total_written;

                        // ADR-0001: Update control file with segment progress.
                        // Mark the piece done and periodically save to disk.
                        if let Some(ref mut cf) = ctrl_file {
                            if parent_complete {
                                cf.mark_piece_done(seg_idx as usize);
                            }
                            ctrl_bytes_since_save += total_written;
                            if ctrl_bytes_since_save >= ctrl_save_interval {
                                if !payload_synced {
                                    writer.sync_all().await.map_err(|error| {
                                        Aria2Error::FileIo(format!(
                                            "Failed to durably sync HTTP range checkpoint payload: {error}"
                                        ))
                                    })?;
                                }
                                cf.update_completed_length(completed_bytes);
                                if let Err(e) = cf.save().await {
                                    tracing::warn!("Control file save failed: {}", e);
                                }
                                ctrl_bytes_since_save = 0;
                            }
                        }
                        if parent_complete {
                            work_queue
                                .complete(work_lease)
                                .map_err(|error| work_scheduler_error("complete a range", error))?;
                        } else {
                            work_queue
                                .reschedule_after_success(work_lease)
                                .map_err(|error| work_scheduler_error("reschedule a partial range", error))?;
                        }
                        // Use the atomic total for progress updates so that
                        // in-flight progress from concurrent segments is not
                        // overwritten by the committed-only value.
                        let display_total = progress_tracker.total();

                        dl.progress_updater
                            .update_progress_without_speed(
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
                        let current_limit_rejected = explicit_range_size_rejection
                            && rejected_range_size_limit >= range_size_limit;
                        let rejection_is_from_an_older_limit =
                            rejected_range_size_limit > range_size_limit;
                        let reduced_limit = if current_limit_rejected
                            && rejected_range_size_limit == range_size_limit
                        {
                            lower_rejected_range_size_limit(
                                range_size_limit,
                                rejected_range_size_limit,
                                minimum_range_size_limit,
                            )
                        } else {
                            None
                        };
                        if let Some(next_limit) = reduced_limit {
                            range_size_limit = next_limit;
                            tracing::warn!(
                                authority = %authority_key,
                                previous_range_size_limit = rejected_range_size_limit,
                                range_size_limit,
                                fixed_piece_size = piece_length,
                                "Reduced HTTP Range size limit after explicit server rejection"
                            );
                        }
                        let range_size_rejection_handled = current_limit_rejected
                            && (reduced_limit.is_some() || rejection_is_from_an_older_limit);
                        if range_size_rejection_handled {
                            if !manager.requeue_segment(seg_idx) {
                                return Err(Aria2Error::Fatal(
                                    crate::error::FatalError::Config(format!(
                                        "HTTP segment scheduler could not requeue size-rejected parent {seg_idx}"
                                    )),
                                ));
                            }
                            work_queue
                                .retry_without_consuming_attempt(work_lease)
                                .map_err(|error| work_scheduler_error("retry a smaller range", error))?;
                            consecutive_416_count = 0;
                        } else {
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
                            fail_work(&mut work_queue, work_lease, None)?;
                            work_queue.cancel();
                            cancel_and_persist(
                                dl,
                                executor,
                                &mut write_rx,
                                &mut writer,
                                limiter.as_ref(),
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
                                fail_work(&mut work_queue, work_lease, None)?;
                                work_queue.cancel();
                                break;
                            }
                        } else {
                            consecutive_416_count = 0;
                        }
                        let retry_count = work_lease.attempt().saturating_sub(1);
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
                            fail_work(&mut work_queue, work_lease, None)?;
                            work_queue.cancel();
                            cancel_and_persist(
                                dl,
                                executor,
                                &mut write_rx,
                                &mut writer,
                                limiter.as_ref(),
                                &mut ctrl_file,
                                completed_bytes,
                            )
                            .await?;
                            return Err(e);
                        }
                        if adaptive_capacity_retry {
                            manager.requeue_segment(seg_idx);
                            work_queue
                                .retry_without_consuming_attempt(work_lease)
                                .map_err(|error| work_scheduler_error("retry after capacity adaptation", error))?;
                        } else {
                            manager.fail_segment(seg_idx);
                            let _ = fail_work(
                                &mut work_queue,
                                work_lease,
                                Some(std::time::Instant::now()),
                            )?;
                        }
                        }
                    }
                }
            }
            // A write chunk arrived while segments are still running
            Some(WriteChunk { offset, data }) = write_rx.recv() => {
                write_http_range_chunk(
                    dl,
                    &mut writer,
                    limiter.as_ref(),
                    &mut ctrl_file,
                    HttpRangeCommitOptions {
                        completed_bytes,
                        flush_checkpoint: true,
                        error_context: "",
                    },
                    WriteChunk { offset, data },
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
                        dl,
                        executor,
                        &mut write_rx,
                        &mut writer,
                        limiter.as_ref(),
                        &mut ctrl_file,
                        completed_bytes,
                    )
                    .await?;
                    return Err(e);
                }
            }
            _ = stall_check.tick() => {
                progress_tracker.refresh_speed();
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
                    let lease = active_work.remove(&seg_idx).ok_or_else(|| {
                        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                            "HTTP work scheduler lost the stalled range lease {seg_idx}"
                        )))
                    })?;
                    let last_throughput_bps = if let Some(progress) = segment_progress.remove(&seg_idx) {
                        let throughput = progress.recent_throughput_bps();
                        progress.rollback();
                        throughput
                    } else {
                        0
                    };
                    manager.fail_segment(seg_idx);
                    let _ = fail_work(
                        &mut work_queue,
                        lease,
                        Some(std::time::Instant::now()),
                    )?;
                    tracing::warn!(
                        seg_idx,
                        stall_timeout_secs = segment_stall_timeout.as_secs(),
                        last_throughput_bps,
                        "Reclaimed fully stalled HTTP Range request"
                    );
                } else {
                    let slow_outlier = active_segs.iter().find_map(
                        |(seg_idx, (_, length, started_at, _))| {
                            let progress = segment_progress.get(seg_idx)?;
                            slow_range_recovery
                                .slow_outlier(
                                    &authority_key,
                                    *seg_idx,
                                    progress.downloaded_bytes(),
                                    progress.recent_throughput_bps(),
                                    *length,
                                    started_at.elapsed(),
                                )
                                .map(|observation| (*seg_idx, observation))
                        },
                    );
                    if let Some((seg_idx, observation)) = slow_outlier
                        && executor.abort_segment(seg_idx).await
                    {
                        connection_guard.set(executor.in_flight());
                        active_segs.remove(&seg_idx);
                        let lease = active_work.remove(&seg_idx).ok_or_else(|| {
                            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                                "HTTP work scheduler lost the slow range lease {seg_idx}"
                            )))
                        })?;
                        if let Some(progress) = segment_progress.remove(&seg_idx) {
                            progress.rollback();
                        }
                        if manager.requeue_segment(seg_idx) {
                            work_queue
                                .retry_without_consuming_attempt(lease)
                                .map_err(|error| work_scheduler_error("retry a slow range", error))?;
                            slow_range_recovery.mark_recovered(seg_idx);
                            tracing::warn!(
                                seg_idx,
                                goodput_bps = observation.goodput_bps,
                                reference_goodput_bps = observation.reference_goodput_bps,
                                estimated_remaining_ms = observation.estimated_remaining.as_millis(),
                                estimated_retry_ms = observation.estimated_retry.as_millis(),
                                downloaded_bytes = observation.downloaded_bytes,
                                range_length = observation.range_length,
                                "Requeued slow HTTP Range outlier once"
                            );
                        } else {
                            manager.fail_segment(seg_idx);
                            fail_work(&mut work_queue, lease, None)?;
                            tracing::warn!(
                                seg_idx,
                                "Slow HTTP Range recovery could not requeue its active segment"
                            );
                        }
                    }
                }
            }
        }
    }

    finalize::finish(
        dl,
        should_fallback,
        executor,
        &manager,
        &mut write_rx,
        &mut writer,
        limiter.as_ref(),
        &mut ctrl_file,
        &ctrl_path,
        completed_bytes,
        &progress_tracker,
    )
    .await
}

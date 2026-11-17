use std::sync::Arc;
use std::time::Instant;

use crate::constants;
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::http::adaptive_concurrency::HttpAdaptiveConcurrency;
use crate::engine::http::segment_downloader::{SegmentProgress, SegmentProgressTracker};
use crate::engine::retry_policy::RetryPolicy;
use crate::engine::work_scheduler::{RetryOutcome, WorkLease, WorkScheduler};
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::util::rwlock_ext::RwLockRecover;

use super::super::in_flight::InFlightProgress;
use super::super::range_size_limit::lower_rejected_range_size_limit;
use super::super::slow_range::SlowRangeRecovery;
use super::{ActiveSegment, ConcurrentDownloader, fail_work, work_scheduler_error};

const FALLBACK_THRESHOLD_CONSECUTIVE: u32 = 3;
const FALLBACK_THRESHOLD_RATIO: f64 = 0.2;

pub(super) enum CompletionAction {
    Continue,
    Fallback,
    Abort(Aria2Error),
}

pub(super) struct CompletionData {
    pub(super) segment_index: u32,
    pub(super) result: Result<u64>,
    pub(super) active_segment: ActiveSegment,
    pub(super) work_lease: WorkLease<u32>,
    pub(super) segment_progress: Option<Arc<SegmentProgress>>,
    pub(super) rejected_range_size_limit: u64,
    pub(super) explicit_range_size_rejection: bool,
    pub(super) retry_after: Option<std::time::Duration>,
    pub(super) is_http2: bool,
}

pub(super) struct CompletionState<'a> {
    pub(super) downloader: &'a mut ConcurrentDownloader,
    pub(super) uri: &'a str,
    pub(super) authority_key: &'a str,
    pub(super) split: usize,
    pub(super) piece_length: u32,
    pub(super) range_size_limit: &'a mut u64,
    pub(super) minimum_range_size_limit: u64,
    pub(super) max_retries: u32,
    pub(super) retry_policy: &'a RetryPolicy,
    pub(super) adaptive: &'a mut HttpAdaptiveConcurrency,
    pub(super) manager: &'a mut ConcurrentSegmentManager,
    pub(super) work_queue: &'a mut WorkScheduler<u32>,
    pub(super) slow_range_recovery: &'a mut SlowRangeRecovery,
    pub(super) last_range_finished: &'a mut Instant,
    pub(super) consecutive_416_count: &'a mut u32,
    pub(super) total_416_count: &'a mut u32,
    pub(super) writer: &'a mut CachedDiskWriter,
    pub(super) ctrl_file: &'a mut Option<ControlFile>,
    pub(super) ctrl_save_interval: u64,
    pub(super) ctrl_bytes_since_save: &'a mut u64,
    pub(super) completed_bytes: &'a mut u64,
    pub(super) in_flight_progress: &'a mut InFlightProgress,
    pub(super) progress_tracker: &'a Arc<SegmentProgressTracker>,
}

pub(super) async fn handle(
    state: CompletionState<'_>,
    completion: CompletionData,
) -> Result<CompletionAction> {
    let segment_index = completion.segment_index;
    let result_progress = completion.segment_progress;
    match completion.result {
        Ok(total_written) => {
            *state.last_range_finished = Instant::now();
            state.adaptive.record_success(completion.is_http2);
            state.slow_range_recovery.record_success(
                state.authority_key,
                total_written,
                completion.active_segment.started_at.elapsed(),
            );
            let Some(parent_complete) = state.manager.complete_range(segment_index, total_written)
            else {
                return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                    format!(
                        "HTTP segment scheduler rejected completed range for parent {segment_index}"
                    ),
                )));
            };

            let mut payload_synced = false;
            if parent_complete {
                state.writer.sync_all().await.map_err(|error| {
                    Aria2Error::FileIo(format!(
                        "Failed to durably sync completed HTTP range: {error}"
                    ))
                })?;
                payload_synced = true;
            }
            *state.completed_bytes += total_written;

            if let Some(control_file) = state.ctrl_file {
                if parent_complete {
                    control_file.mark_piece_done(segment_index as usize);
                }
                *state.ctrl_bytes_since_save += total_written;
                if *state.ctrl_bytes_since_save >= state.ctrl_save_interval {
                    if !payload_synced {
                        state.writer.sync_all().await.map_err(|error| {
                            Aria2Error::FileIo(format!(
                                "Failed to durably sync HTTP range checkpoint payload: {error}"
                            ))
                        })?;
                    }
                    control_file.set_in_flight_pieces(
                        state.in_flight_progress.snapshot(control_file.bitfield()),
                    );
                    control_file.update_completed_length(*state.completed_bytes);
                    if let Err(error) = control_file.save().await {
                        tracing::warn!("Control file save failed: {}", error);
                    }
                    *state.ctrl_bytes_since_save = 0;
                }
            }

            if parent_complete {
                state
                    .work_queue
                    .complete(completion.work_lease)
                    .map_err(|error| work_scheduler_error("complete a range", error))?;
            } else {
                state
                    .work_queue
                    .reschedule_after_success(completion.work_lease)
                    .map_err(|error| work_scheduler_error("reschedule a partial range", error))?;
            }

            let display_total = state.progress_tracker.total();
            state
                .downloader
                .progress_updater
                .update_progress_without_speed(
                    display_total,
                    constants::PROGRESS_UPDATE_BYTES as u64,
                    constants::HTTP_SPEED_UPDATE_INTERVAL_MS,
                )
                .await;
            super::super::flush_requested_control_file(
                state.downloader,
                state.writer,
                state.ctrl_file,
                *state.completed_bytes,
                Some(state.in_flight_progress),
            )
            .await?;
            Ok(CompletionAction::Continue)
        }
        Err(error) => {
            if let Some(progress) = result_progress {
                progress.rollback();
            }
            let error = if matches!(
                &error,
                Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
            ) {
                state.downloader.group.recover().file_not_found_error()
            } else {
                error
            };
            tracing::warn!(segment_index, error = %error, "Segment download failed");
            let is_capacity_limited = super::super::is_capacity_limited_error(&error);
            if !is_capacity_limited {
                state.adaptive.record_failure(completion.is_http2);
            }

            let current_limit_rejected = completion.explicit_range_size_rejection
                && completion.rejected_range_size_limit >= *state.range_size_limit;
            let rejection_is_from_an_older_limit =
                completion.rejected_range_size_limit > *state.range_size_limit;
            let reduced_limit = if current_limit_rejected
                && completion.rejected_range_size_limit == *state.range_size_limit
            {
                lower_rejected_range_size_limit(
                    *state.range_size_limit,
                    completion.rejected_range_size_limit,
                    state.minimum_range_size_limit,
                )
            } else {
                None
            };
            if let Some(next_limit) = reduced_limit {
                let previous_limit = completion.rejected_range_size_limit;
                *state.range_size_limit = next_limit;
                tracing::warn!(
                    authority = %state.authority_key,
                    previous_range_size_limit = previous_limit,
                    range_size_limit = next_limit,
                    fixed_piece_size = state.piece_length,
                    "Reduced HTTP Range size limit after explicit server rejection"
                );
            }
            let range_size_rejection_handled = current_limit_rejected
                && (reduced_limit.is_some() || rejection_is_from_an_older_limit);
            if range_size_rejection_handled {
                if !state.manager.requeue_segment(segment_index) {
                    return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                        format!(
                            "HTTP segment scheduler could not requeue size-rejected parent {segment_index}"
                        ),
                    )));
                }
                state
                    .work_queue
                    .retry_without_consuming_attempt(completion.work_lease)
                    .map_err(|error| work_scheduler_error("retry a smaller range", error))?;
                *state.consecutive_416_count = 0;
                return Ok(CompletionAction::Continue);
            }

            let is_file_not_found = matches!(
                &error,
                Aria2Error::Recoverable(
                    RecoverableError::ResourceNotFound | RecoverableError::MaxFileNotFound
                )
            );
            let file_not_found_retry_allowed =
                !matches!(
                    &error,
                    Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
                ) && state.downloader.group.recover().can_retry_file_not_found();
            if is_file_not_found && !file_not_found_retry_allowed {
                fail_work(state.work_queue, completion.work_lease, None)?;
                state.work_queue.cancel();
                return Ok(CompletionAction::Abort(error));
            }

            if is_capacity_limited {
                let wait = state
                    .adaptive
                    .record_capacity_rejection(completion.is_http2, completion.retry_after);
                tracing::warn!(
                    segment_index,
                    target_connections = state.adaptive.connection_target(completion.is_http2),
                    wait_ms = wait.as_millis().min(u128::from(u64::MAX)) as u64,
                    "Server limited HTTP Range requests; backing off"
                );
                let gave_up = state.last_range_finished.elapsed() >= super::CAPACITY_GIVE_UP
                    || (state.max_retries != 0
                        && state.adaptive.backoff_steps() > state.max_retries);
                if gave_up {
                    fail_work(state.work_queue, completion.work_lease, None)?;
                    state.work_queue.cancel();
                    return Ok(CompletionAction::Abort(error));
                }
                if !state.manager.requeue_segment(segment_index) {
                    return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                        format!(
                            "HTTP segment scheduler could not requeue rate-limited parent {segment_index}"
                        ),
                    )));
                }
                let retry_at = Instant::now()
                    .checked_add(wait)
                    .unwrap_or_else(Instant::now);
                let max_capacity_retries = if state.adaptive.is_enabled() {
                    0
                } else {
                    state.max_retries
                };
                let outcome = state
                    .work_queue
                    .retry_capacity_limited_at(
                        completion.work_lease,
                        retry_at,
                        max_capacity_retries,
                    )
                    .map_err(|error| {
                        work_scheduler_error("schedule a rate-limited range", error)
                    })?;
                if matches!(outcome, RetryOutcome::Exhausted(_)) {
                    state.manager.fail_segment(segment_index);
                    state.work_queue.cancel();
                    return Ok(CompletionAction::Abort(error));
                }
                *state.consecutive_416_count = 0;
                return Ok(CompletionAction::Continue);
            }

            let is_416 = matches!(
                &error,
                Aria2Error::Recoverable(RecoverableError::RangeNotSatisfiable { .. })
            );
            if is_416 {
                *state.consecutive_416_count += 1;
                *state.total_416_count += 1;
                tracing::warn!(
                    segment_index,
                    consecutive_416 = *state.consecutive_416_count,
                    total_416 = *state.total_416_count,
                    "RangeNotSatisfiable (416) detected"
                );
                let failure_ratio = *state.total_416_count as f64 / state.split as f64;
                let threshold_exceeded = *state.consecutive_416_count
                    >= FALLBACK_THRESHOLD_CONSECUTIVE
                    || failure_ratio >= FALLBACK_THRESHOLD_RATIO;
                if threshold_exceeded {
                    tracing::warn!(
                        uri = state.uri,
                        consecutive_416 = *state.consecutive_416_count,
                        failure_ratio,
                        "Fallback to sequential mode triggered due to RangeNotSatisfiable errors"
                    );
                    fail_work(state.work_queue, completion.work_lease, None)?;
                    state.work_queue.cancel();
                    return Ok(CompletionAction::Fallback);
                }
            } else {
                *state.consecutive_416_count = 0;
            }

            let retry_count = completion.work_lease.attempt().saturating_sub(1);
            let retry_allowed = super::super::should_retry_segment(
                state.retry_policy,
                retry_count,
                &error,
                file_not_found_retry_allowed,
            );
            if !retry_allowed {
                fail_work(state.work_queue, completion.work_lease, None)?;
                state.work_queue.cancel();
                return Ok(CompletionAction::Abort(error));
            }
            state.manager.fail_segment(segment_index);
            let _ = fail_work(
                state.work_queue,
                completion.work_lease,
                Some(Instant::now()),
            )?;
            Ok(CompletionAction::Continue)
        }
    }
}

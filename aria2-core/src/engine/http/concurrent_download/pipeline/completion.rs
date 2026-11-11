use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::http::adaptive_concurrency::HttpAdaptiveConcurrency;
use crate::engine::http::concurrent_download::pipeline::scheduler::{self, ActiveRangeWork};
use crate::engine::http::concurrent_download::slow_range::SlowRangeRecovery;
use crate::engine::http::request_executor::{HttpSegmentRequestExecutor, HttpSegmentRequestResult};
use crate::engine::http::segment_downloader::{SegmentProgress, WriteChunk};
use crate::engine::mirror_coordinator::MirrorCoordinator;
use crate::engine::retry_policy::RetryPolicy;
use crate::engine::work_scheduler::WorkScheduler;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::RateLimiter;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::ConcurrentDownloader;
use super::super::in_flight::InFlightProgress;
use super::super::range_commit::{HttpRangeCommitOptions, write_http_range_chunk};
use super::work::{http_work_scheduler_error, record_http_work_failure};

const FALLBACK_THRESHOLD_CONSECUTIVE: u32 = 3;
const FALLBACK_THRESHOLD_RATIO: f64 = 0.2;

pub(super) enum CompletionAction {
    Continue,
    Fallback,
    Abort(Aria2Error),
}

pub(super) struct CompletionContext<'a> {
    pub(super) downloader: &'a mut ConcurrentDownloader,
    pub(super) uris: &'a [String],
    pub(super) split: usize,
    pub(super) fixed_piece_size: u64,
    pub(super) minimum_range_size_limit: u64,
    pub(super) retry_policy: &'a RetryPolicy,
    pub(super) adaptive: &'a mut HashMap<String, HttpAdaptiveConcurrency>,
    pub(super) range_size_limits: &'a mut [u64],
    pub(super) executor: &'a HttpSegmentRequestExecutor,
    pub(super) write_rx: &'a mut tokio::sync::mpsc::Receiver<WriteChunk>,
    pub(super) writer: &'a mut CachedDiskWriter,
    pub(super) limiter: Option<&'a RateLimiter>,
    pub(super) coordinator: &'a mut MirrorCoordinator,
    pub(super) ctrl_file: &'a mut Option<ControlFile>,
    pub(super) ctrl_save_interval: u64,
    pub(super) ctrl_bytes_since_save: &'a mut u64,
    pub(super) in_flight_progress: &'a mut InFlightProgress,
    pub(super) work_queue: &'a mut WorkScheduler<u32>,
    pub(super) segment_progress: Option<Arc<SegmentProgress>>,
    pub(super) slow_range_recovery: &'a mut SlowRangeRecovery,
    pub(super) consecutive_416_count: &'a mut u32,
    pub(super) total_416_count: &'a mut u32,
}

pub(super) struct CompletionData {
    pub(super) result: HttpSegmentRequestResult,
    pub(super) active_work: ActiveRangeWork,
}

pub(super) async fn handle(
    context: CompletionContext<'_>,
    completion: CompletionData,
) -> Result<CompletionAction> {
    let segment_index = completion.result.segment_index;
    let authority_key = completion.result.authority_key.clone();
    let scheduler::ActiveRangeWork {
        lease,
        mirror_idx,
        started_at,
        ..
    } = completion.active_work;

    match completion.result.result {
        Ok(bytes_downloaded) => {
            if let Some(controller) = context.adaptive.get_mut(&authority_key) {
                controller.record_success(context.executor.is_http2(&authority_key));
            }

            // The result is sent after its chunks enter the channel. Commit
            // them before the coordinator marks the range complete.
            while let Ok(WriteChunk { offset, data }) = context.write_rx.try_recv() {
                write_http_range_chunk(
                    context.downloader,
                    context.writer,
                    context.limiter,
                    context.ctrl_file,
                    HttpRangeCommitOptions {
                        completed_bytes: context.coordinator.completed_bytes(),
                        flush_checkpoint: false,
                        error_context: "",
                        in_flight: Some(context.in_flight_progress),
                    },
                    WriteChunk { offset, data },
                )
                .await?;
            }

            let elapsed = started_at.elapsed();
            let speed = if elapsed.as_secs_f64() > 0.0 {
                (bytes_downloaded as f64 / elapsed.as_secs_f64()) as u64
            } else {
                0
            };
            let Some(parent_complete) = context.coordinator.on_range_complete(
                mirror_idx,
                segment_index,
                bytes_downloaded,
                speed,
            ) else {
                return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                    format!(
                        "Segment {segment_index} completed with invalid length {bytes_downloaded}"
                    ),
                )));
            };
            context
                .slow_range_recovery
                .record_success(&authority_key, bytes_downloaded, elapsed);

            let mut payload_synced = false;
            if parent_complete {
                context.writer.sync_all().await.map_err(|error| {
                    Aria2Error::FileIo(format!(
                        "Failed to durably sync completed HTTP range: {error}"
                    ))
                })?;
                payload_synced = true;
            }
            if let Some(control_file) = context.ctrl_file.as_mut() {
                if parent_complete {
                    control_file.mark_piece_done(segment_index as usize);
                }
                *context.ctrl_bytes_since_save = context
                    .ctrl_bytes_since_save
                    .saturating_add(bytes_downloaded);
                if *context.ctrl_bytes_since_save >= context.ctrl_save_interval {
                    if !payload_synced {
                        context.writer.sync_all().await.map_err(|error| {
                            Aria2Error::FileIo(format!(
                                "Failed to durably sync HTTP range checkpoint payload: {error}"
                            ))
                        })?;
                    }
                    let in_flight_pieces =
                        context.in_flight_progress.snapshot(control_file.bitfield());
                    control_file.set_in_flight_pieces(in_flight_pieces);
                    control_file.update_completed_length(context.coordinator.completed_bytes());
                    if let Err(error) = control_file.save().await {
                        tracing::warn!(%error, "Failed to save multi-mirror control file");
                    }
                    *context.ctrl_bytes_since_save = 0;
                }
            }

            if parent_complete {
                context
                    .work_queue
                    .complete(lease)
                    .map_err(|error| http_work_scheduler_error("complete work", error))?;
            } else {
                context
                    .work_queue
                    .reschedule_after_success(lease)
                    .map_err(|error| {
                        http_work_scheduler_error("reschedule a partial range", error)
                    })?;
            }
            Ok(CompletionAction::Continue)
        }
        Err(error) => {
            if let Some(progress) = context.segment_progress {
                progress.rollback();
            }
            let error = if matches!(
                &error,
                Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
            ) {
                context.downloader.group.recover().file_not_found_error()
            } else {
                error
            };
            tracing::warn!(segment_index, mirror_idx, error = %error, "Pooled segment download failed");

            let current_limit = scheduler::current_authority_range_size_limit(
                context.uris,
                context.range_size_limits,
                &authority_key,
            );
            let rejected_limit = completion.result.range_size_limit;
            let explicit_rejection = completion.result.range_size_rejected;
            let rejection_targets_current_or_old_limit = explicit_rejection
                && current_limit.is_some_and(|current| rejected_limit >= current);
            let reduced_limit = if current_limit == Some(rejected_limit) {
                scheduler::lower_authority_range_size_limit(
                    context.uris,
                    context.range_size_limits,
                    &authority_key,
                    rejected_limit,
                    context.minimum_range_size_limit,
                )
            } else {
                None
            };
            if let Some(next_limit) = reduced_limit {
                tracing::warn!(
                    authority = %authority_key,
                    previous_range_size_limit = rejected_limit,
                    range_size_limit = next_limit,
                    fixed_piece_size = context.fixed_piece_size,
                    "Reduced HTTP Range size limit after explicit server rejection"
                );
            }
            let range_size_rejection_handled = rejection_targets_current_or_old_limit
                && current_limit
                    .is_some_and(|current| rejected_limit > current || reduced_limit.is_some());
            if range_size_rejection_handled {
                if !context.coordinator.requeue_segment(segment_index) {
                    return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                        format!(
                            "Multi-mirror scheduler could not requeue size-rejected parent {segment_index}"
                        ),
                    )));
                }
                context
                    .work_queue
                    .retry_without_consuming_attempt(lease)
                    .map_err(|error| http_work_scheduler_error("retry a smaller range", error))?;
                *context.consecutive_416_count = 0;
                return Ok(CompletionAction::Continue);
            }

            let file_not_found_retry_allowed = !matches!(
                &error,
                Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
            ) && context
                .downloader
                .group
                .recover()
                .can_retry_file_not_found();
            if matches!(
                &error,
                Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
            ) {
                record_http_work_failure(context.work_queue, lease, false)?;
                return Ok(CompletionAction::Abort(error));
            }

            let error_code = super::super::server_stat_error_code(&error);
            let is_capacity_limited = super::super::is_capacity_limited_error(&error);
            let is_http2 = context.executor.is_http2(&authority_key);
            if let Some(controller) = context.adaptive.get_mut(&authority_key) {
                if is_capacity_limited {
                    controller.record_capacity_failure(is_http2);
                } else {
                    controller.record_failure(is_http2);
                }
            }

            let is_416 = matches!(
                &error,
                Aria2Error::Recoverable(RecoverableError::RangeNotSatisfiable { .. })
            );
            if is_416 {
                *context.consecutive_416_count += 1;
                *context.total_416_count += 1;
                let failure_ratio = *context.total_416_count as f64 / context.split as f64;
                let should_fallback = *context.consecutive_416_count
                    >= FALLBACK_THRESHOLD_CONSECUTIVE
                    || failure_ratio >= FALLBACK_THRESHOLD_RATIO;
                if should_fallback {
                    record_http_work_failure(context.work_queue, lease, false)?;
                    return Ok(CompletionAction::Fallback);
                }
            } else {
                *context.consecutive_416_count = 0;
            }

            let adaptive_capacity_retry = is_capacity_limited
                && context
                    .adaptive
                    .get(&authority_key)
                    .is_some_and(|controller| controller.preserve_retry_budget(is_http2));
            let retry_count = lease.attempt().saturating_sub(1);
            let retry_allowed = adaptive_capacity_retry
                || super::super::should_retry_segment(
                    context.retry_policy,
                    retry_count,
                    &error,
                    file_not_found_retry_allowed,
                );
            if !retry_allowed {
                let failed_over = context.coordinator.num_mirrors() > 1
                    && context
                        .coordinator
                        .on_terminal_segment_failed(segment_index, error_code)
                        .is_some();
                if !failed_over {
                    record_http_work_failure(context.work_queue, lease, false)?;
                    return Ok(CompletionAction::Abort(error));
                }
                context
                    .work_queue
                    .retry_with_fresh_attempt_budget(lease)
                    .map_err(|error| http_work_scheduler_error("retry on another mirror", error))?;
            } else if adaptive_capacity_retry {
                context.coordinator.requeue_segment(segment_index);
                context
                    .work_queue
                    .retry_without_consuming_attempt(lease)
                    .map_err(|error| {
                        http_work_scheduler_error("retry after capacity adaptation", error)
                    })?;
            } else {
                context
                    .coordinator
                    .on_segment_failed(mirror_idx, segment_index, error_code);
                let _ = record_http_work_failure(context.work_queue, lease, true)?;
            }
            Ok(CompletionAction::Continue)
        }
    }
}

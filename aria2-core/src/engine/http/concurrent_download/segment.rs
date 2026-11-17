//! Single-mirror concurrent download loop.
//!
//! Contains the execute function that runs the pooled segment download
//! pipeline for a single URI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::constants;
use crate::engine::concurrent_segment_manager::SegmentStatus;
use crate::engine::http::segment_downloader::{
    SegmentProgress, SegmentProgressTracker, WriteChunk,
};
use crate::engine::work_scheduler::{
    RetryOutcome, WorkId, WorkItem, WorkLease, WorkScheduleError, WorkScheduler,
};
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::resume_helper::ResumeState;
use crate::util::rwlock_ext::RwLockRecover;

use super::in_flight::InFlightProgress;
use super::slow_range::SlowRangeRecovery;
use super::{ConcurrentDownloadResult, ConcurrentDownloader};

mod admission;
mod cancel;
mod completion;
mod executor;
mod finalize;
mod io;
mod output;
mod plan;
mod resume;
mod stall;

use super::range_commit::{HttpRangeCommitOptions, write_http_range_chunk};
pub(super) use cancel::cancel_and_persist;

/// How long 429/503 answers may go on with no range finishing before the
/// download fails.
const CAPACITY_GIVE_UP: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy)]
struct ActiveSegment {
    offset: u64,
    length: u64,
    started_at: std::time::Instant,
    task_id: u64,
}

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
    let mut should_fallback = false;

    let cookie_hdr = dl.cookie_helper.build_cookie_header(uri);

    let mut in_flight_progress = InFlightProgress::new(
        total_length,
        u64::from(piece_length),
        manager.num_segments(),
    );

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
        &mut in_flight_progress,
    )
    .await?;
    let initial_completed = completed_bytes;
    let mut ctrl_bytes_since_save = 0;

    let mut active_segments: HashMap<u32, ActiveSegment> = HashMap::new();
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
    // For giving up on a server that keeps answering 429/503.
    let mut last_range_finished = std::time::Instant::now();
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
                Some(&mut in_flight_progress),
            )
            .await?;
            return Err(e);
        }

        admission::admit_pending_ranges(admission::Admission {
            uri,
            total_length,
            split,
            authority_key: &authority_key,
            cookie_header: &cookie_hdr,
            range_size_limit,
            executor: &mut executor,
            adaptive: &mut adaptive,
            manager: &mut manager,
            progress_tracker: &progress_tracker,
            segment_progress: &mut segment_progress,
            active_segments: &mut active_segments,
            active_work: &mut active_work,
            work_queue: &mut work_queue,
            write_tx: &write_tx,
            connection_guard: &connection_guard,
        })?;

        if executor.in_flight() == 0 {
            // Drain any remaining write chunks before checking completion
            io::drain_write_chunks(
                dl,
                &mut write_rx,
                &mut writer,
                limiter.as_ref(),
                &mut ctrl_file,
                io::DrainWriteChunksOptions {
                    completed_bytes,
                    in_flight: Some(&mut in_flight_progress),
                    error_context: "",
                },
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
                cancel_and_persist(
                    dl,
                    executor,
                    &mut write_rx,
                    &mut writer,
                    limiter.as_ref(),
                    &mut ctrl_file,
                    completed_bytes,
                    Some(&mut in_flight_progress),
                )
                .await?;
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
                tracked_active_segments = ?active_segments.keys().collect::<Vec<_>>(),
                tracked_active_requests = ?active_segments
                    .iter()
                    .map(|(segment_index, active)| (*segment_index, active.offset, active.task_id))
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
            io::DrainWriteChunksOptions {
                completed_bytes,
                in_flight: Some(&mut in_flight_progress),
                error_context: "",
            },
        )
        .await?;
        super::flush_requested_control_file(
            dl,
            &mut writer,
            &mut ctrl_file,
            completed_bytes,
            Some(&in_flight_progress),
        )
        .await?;

        // Use tokio::select! to drain writes concurrently while waiting
        // for segment completions — prevents chunks from piling up in the
        // channel while other segments are still downloading.
        let completion_action = tokio::select! {
            // A segment completed
            Some(pool_result) = executor.next_result() => {
                executor.reap_task(pool_result.task_id).await;
                executor::update_connection_snapshot(&executor, &connection_guard);
                let seg_idx = pool_result.segment_index;
                let Some(active_segment) = active_segments.get(&seg_idx).copied()
                else {
                    tracing::warn!(
                        segment_index = seg_idx,
                        result_task_id = pool_result.task_id,
                        in_flight = executor.in_flight_for(&authority_key),
                        tracked_active_segments = ?active_segments.keys().collect::<Vec<_>>(),
                        "Received HTTP Range completion for an untracked segment"
                    );
                    continue;
                };
                if active_segment.task_id != pool_result.task_id {
                    tracing::warn!(
                        segment_index = seg_idx,
                        active_task_id = active_segment.task_id,
                        result_task_id = pool_result.task_id,
                        "Received HTTP Range completion for a replaced task"
                    );
                    continue;
                }
                if let Some(peer_addr) = pool_result.peer_addr
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
                // Drain committed bytes before reconciling the completed range.
                io::drain_write_chunks(
                    dl,
                    &mut write_rx,
                    &mut writer,
                    limiter.as_ref(),
                    &mut ctrl_file,
                    io::DrainWriteChunksOptions {
                        completed_bytes,
                        in_flight: Some(&mut in_flight_progress),
                        error_context: "",
                    },
                )
                .await?;

                let active_segment = active_segments.remove(&seg_idx).ok_or_else(|| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "HTTP work scheduler lost the active range {seg_idx}"
                    )))
                })?;
                let work_lease = active_work.remove(&seg_idx).ok_or_else(|| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "HTTP work scheduler lost the active lease for range {seg_idx}"
                    )))
                })?;
                let segment_progress = segment_progress.remove(&seg_idx);
                let action = completion::handle(
                    completion::CompletionState {
                        downloader: dl,
                        uri,
                        authority_key: &authority_key,
                        split,
                        piece_length,
                        range_size_limit: &mut range_size_limit,
                        minimum_range_size_limit,
                        max_retries: options.max_retries,
                        retry_policy: &retry_policy,
                        adaptive: &mut adaptive,
                        manager: &mut manager,
                        work_queue: &mut work_queue,
                        slow_range_recovery: &mut slow_range_recovery,
                        last_range_finished: &mut last_range_finished,
                        consecutive_416_count: &mut consecutive_416_count,
                        total_416_count: &mut total_416_count,
                        writer: &mut writer,
                        ctrl_file: &mut ctrl_file,
                        ctrl_save_interval,
                        ctrl_bytes_since_save: &mut ctrl_bytes_since_save,
                        completed_bytes: &mut completed_bytes,
                        in_flight_progress: &mut in_flight_progress,
                        progress_tracker: &progress_tracker,
                    },
                    completion::CompletionData {
                        segment_index: seg_idx,
                        result: pool_result.result,
                        active_segment,
                        work_lease,
                        segment_progress,
                        rejected_range_size_limit: pool_result.range_size_limit,
                        explicit_range_size_rejection: pool_result.range_size_rejected,
                        retry_after: pool_result.retry_after,
                        is_http2: executor.is_http2(&authority_key),
                    },
                )
                .await?;
                Some(action)
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
                        in_flight: Some(&mut in_flight_progress),
                    },
                    WriteChunk { offset, data },
                )
                .await?;
                None
            }
            // Lifecycle changes wake the loop immediately, even when segment
            // futures are blocked on slow network reads.
            _ = &mut lifecycle_changed => {
                super::flush_requested_control_file(
                    dl,
                    &mut writer,
                    &mut ctrl_file,
                    completed_bytes,
                    Some(&in_flight_progress),
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
                        Some(&mut in_flight_progress),
                    )
                    .await?;
                    return Err(e);
                }
                None
            }
            _ = stall_check.tick() => {
                stall::handle_tick(stall::StallTick {
                    executor: &mut executor,
                    connection_guard: &connection_guard,
                    authority_key: &authority_key,
                    progress_tracker: &progress_tracker,
                    segment_progress: &mut segment_progress,
                    active_segments: &mut active_segments,
                    active_work: &mut active_work,
                    manager: &mut manager,
                    work_queue: &mut work_queue,
                    slow_range_recovery: &mut slow_range_recovery,
                    stall_timeout: segment_stall_timeout,
                })
                .await?;
                None
            }
        };

        if let Some(action) = completion_action {
            match action {
                completion::CompletionAction::Continue => {}
                completion::CompletionAction::Fallback => {
                    should_fallback = true;
                    break;
                }
                completion::CompletionAction::Abort(error) => {
                    cancel_and_persist(
                        dl,
                        executor,
                        &mut write_rx,
                        &mut writer,
                        limiter.as_ref(),
                        &mut ctrl_file,
                        completed_bytes,
                        Some(&mut in_flight_progress),
                    )
                    .await?;
                    return Err(error);
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
        &mut in_flight_progress,
    )
    .await
}

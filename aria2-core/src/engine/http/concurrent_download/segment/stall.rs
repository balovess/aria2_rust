use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::http::request_executor::HttpSegmentRequestExecutor;
use crate::engine::http::segment_downloader::{SegmentProgress, SegmentProgressTracker};
use crate::engine::work_scheduler::{WorkLease, WorkScheduler};
use crate::error::{Aria2Error, Result};
use crate::request::request_group::ActiveConnectionGuard;

use super::super::slow_range::SlowRangeRecovery;
use super::{ActiveSegment, fail_work};

pub(super) struct StallTick<'a> {
    pub(super) executor: &'a mut HttpSegmentRequestExecutor,
    pub(super) connection_guard: &'a ActiveConnectionGuard,
    pub(super) authority_key: &'a str,
    pub(super) progress_tracker: &'a SegmentProgressTracker,
    pub(super) segment_progress: &'a mut HashMap<u32, Arc<SegmentProgress>>,
    pub(super) active_segments: &'a mut HashMap<u32, ActiveSegment>,
    pub(super) active_work: &'a mut HashMap<u32, WorkLease<u32>>,
    pub(super) manager: &'a mut ConcurrentSegmentManager,
    pub(super) work_queue: &'a mut WorkScheduler<u32>,
    pub(super) slow_range_recovery: &'a mut SlowRangeRecovery,
    pub(super) stall_timeout: Duration,
}

pub(super) async fn handle_tick(tick: StallTick<'_>) -> Result<()> {
    tick.progress_tracker.refresh_speed();
    let stalled_segment = tick.active_segments.keys().find_map(|segment_index| {
        tick.segment_progress
            .get(segment_index)
            .filter(|progress| progress.is_stalled(tick.stall_timeout))
            .map(|_| *segment_index)
    });

    if let Some(segment_index) = stalled_segment
        && tick.executor.abort_segment(segment_index).await
    {
        super::executor::update_connection_snapshot(tick.executor, tick.connection_guard);
        tick.active_segments.remove(&segment_index);
        let lease = remove_active_lease(tick.active_work, segment_index, "stalled")?;
        let last_throughput_bps =
            if let Some(progress) = tick.segment_progress.remove(&segment_index) {
                let throughput = progress.recent_throughput_bps();
                progress.rollback();
                throughput
            } else {
                0
            };
        tick.manager.fail_segment(segment_index);
        let _ = fail_work(tick.work_queue, lease, Some(Instant::now()))?;
        tracing::warn!(
            segment_index,
            stall_timeout_secs = tick.stall_timeout.as_secs(),
            last_throughput_bps,
            "Reclaimed fully stalled HTTP Range request"
        );
        return Ok(());
    }

    let slow_outlier = tick
        .active_segments
        .iter()
        .find_map(|(segment_index, active)| {
            let progress = tick.segment_progress.get(segment_index)?;
            tick.slow_range_recovery
                .slow_outlier(
                    tick.authority_key,
                    *segment_index,
                    progress.downloaded_bytes(),
                    progress.recent_throughput_bps(),
                    active.length,
                    active.started_at.elapsed(),
                )
                .map(|observation| (*segment_index, observation))
        });

    if let Some((segment_index, observation)) = slow_outlier
        && tick.executor.abort_segment(segment_index).await
    {
        super::executor::update_connection_snapshot(tick.executor, tick.connection_guard);
        tick.active_segments.remove(&segment_index);
        let lease = remove_active_lease(tick.active_work, segment_index, "slow")?;
        if let Some(progress) = tick.segment_progress.remove(&segment_index) {
            progress.rollback();
        }
        if tick.manager.requeue_segment(segment_index) {
            tick.work_queue
                .retry_without_consuming_attempt(lease)
                .map_err(|error| super::work_scheduler_error("retry a slow range", error))?;
            tick.slow_range_recovery.mark_recovered(segment_index);
            tracing::warn!(
                segment_index,
                goodput_bps = observation.goodput_bps,
                reference_goodput_bps = observation.reference_goodput_bps,
                estimated_remaining_ms = observation.estimated_remaining.as_millis(),
                estimated_retry_ms = observation.estimated_retry.as_millis(),
                downloaded_bytes = observation.downloaded_bytes,
                range_length = observation.range_length,
                "Requeued slow HTTP Range outlier once"
            );
        } else {
            tick.manager.fail_segment(segment_index);
            fail_work(tick.work_queue, lease, None)?;
            tracing::warn!(
                segment_index,
                "Slow HTTP Range recovery could not requeue its active segment"
            );
        }
    }

    Ok(())
}

fn remove_active_lease(
    active_work: &mut HashMap<u32, WorkLease<u32>>,
    segment_index: u32,
    reason: &str,
) -> Result<WorkLease<u32>> {
    active_work.remove(&segment_index).ok_or_else(|| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "HTTP work scheduler lost the {reason} range lease {segment_index}"
        )))
    })
}

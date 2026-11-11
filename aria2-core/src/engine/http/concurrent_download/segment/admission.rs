use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::engine::concurrent_segment_manager::{ConcurrentSegmentManager, SegmentStatus};
use crate::engine::http::adaptive_concurrency::HttpAdaptiveConcurrency;
use crate::engine::http::request_executor::{HttpSegmentRequest, HttpSegmentRequestExecutor};
use crate::engine::http::segment_downloader::{
    SegmentProgress, SegmentProgressTracker, WriteChunk,
};
use crate::engine::work_scheduler::{WorkLease, WorkScheduler};
use crate::error::Result;
use crate::request::request_group::ActiveConnectionGuard;

use super::ActiveSegment;

pub(super) struct Admission<'a> {
    pub(super) uri: &'a str,
    pub(super) total_length: u64,
    pub(super) split: usize,
    pub(super) authority_key: &'a str,
    pub(super) cookie_header: &'a Option<String>,
    pub(super) range_size_limit: u64,
    pub(super) executor: &'a mut HttpSegmentRequestExecutor,
    pub(super) adaptive: &'a mut HttpAdaptiveConcurrency,
    pub(super) manager: &'a mut ConcurrentSegmentManager,
    pub(super) progress_tracker: &'a Arc<SegmentProgressTracker>,
    pub(super) segment_progress: &'a mut HashMap<u32, Arc<SegmentProgress>>,
    pub(super) active_segments: &'a mut HashMap<u32, ActiveSegment>,
    pub(super) active_work: &'a mut HashMap<u32, WorkLease<u32>>,
    pub(super) work_queue: &'a mut WorkScheduler<u32>,
    pub(super) write_tx: &'a mpsc::Sender<WriteChunk>,
    pub(super) connection_guard: &'a ActiveConnectionGuard,
}

pub(super) fn admit_pending_ranges(admission: Admission<'_>) -> Result<()> {
    super::executor::update_capacity(
        admission.executor,
        admission.adaptive,
        admission.authority_key,
        admission.split,
        admission.manager.is_complete(),
    );

    while admission.adaptive.can_start(
        admission.executor.in_flight_for(admission.authority_key),
        admission.executor.is_http2(admission.authority_key),
    ) {
        let Some(lease) = admission
            .work_queue
            .admit_one(admission.split, Instant::now())
        else {
            break;
        };
        let segment_index = *lease.payload();
        match admission.manager.next_pending_range_for_segment(
            0,
            segment_index,
            admission.range_size_limit,
        ) {
            Some((_, offset, length)) => {
                let progress = admission.progress_tracker.new_segment();
                admission
                    .segment_progress
                    .insert(segment_index, Arc::clone(&progress));
                let started_at = Instant::now();
                let submitted = admission.executor.try_submit(HttpSegmentRequest {
                    segment_index,
                    authority_key: admission.authority_key.to_owned(),
                    url: admission.uri.to_owned(),
                    offset,
                    length,
                    range_size_limit: admission.range_size_limit,
                    cookie_header: admission.cookie_header.clone(),
                    progress,
                    write_tx: admission.write_tx.clone(),
                    expected_entity_length: admission.total_length,
                });
                let Some(task_id) = submitted else {
                    admission.segment_progress.remove(&segment_index);
                    admission.manager.requeue_segment(segment_index);
                    admission
                        .work_queue
                        .requeue_unstarted(lease)
                        .map_err(|error| {
                            super::work_scheduler_error("requeue an unstarted range", error)
                        })?;
                    break;
                };
                admission.active_segments.insert(
                    segment_index,
                    ActiveSegment {
                        offset,
                        length,
                        started_at,
                        task_id,
                    },
                );
                admission
                    .adaptive
                    .record_request_start(admission.executor.is_http2(admission.authority_key));
                admission.active_work.insert(segment_index, lease);
                super::executor::update_connection_snapshot(
                    admission.executor,
                    admission.connection_guard,
                );
                tracing::debug!(
                    segment_index,
                    task_id,
                    offset,
                    length,
                    "Submitted segment to HTTP connection pool"
                );
            }
            None => match admission.manager.segment_status(segment_index as usize) {
                Some(SegmentStatus::Done | SegmentStatus::Failed) => {
                    admission.work_queue.complete(lease).map_err(|error| {
                        super::work_scheduler_error("complete a terminal range", error)
                    })?;
                }
                _ => {
                    admission
                        .work_queue
                        .requeue_unstarted(lease)
                        .map_err(|error| {
                            super::work_scheduler_error("requeue a blocked range", error)
                        })?;
                    break;
                }
            },
        }
    }

    Ok(())
}

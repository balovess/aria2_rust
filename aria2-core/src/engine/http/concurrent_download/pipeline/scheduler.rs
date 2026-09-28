use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::constants;
use crate::engine::command::WRITE_CHANNEL_CAPACITY;
use crate::engine::http::adaptive_concurrency::HttpAdaptiveConcurrency;
use crate::engine::http::concurrent_download::slow_range::SlowRangeRecovery;
use crate::engine::http::request_executor::{HttpSegmentRequestExecutor, authority_key};
use crate::engine::http::segment_downloader::{
    SegmentProgress, SegmentProgressTracker, WriteChunk,
};
use crate::engine::retry_policy::RetryPolicy;
use crate::request::request_group::ActiveConnectionGuard;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::ConcurrentDownloader;
use crate::engine::mirror_coordinator::MirrorCoordinator;
use crate::request::request_group::DownloadOptions;

pub(super) struct RangeScheduler {
    pub(super) retry_policy: RetryPolicy,
    pub(super) adaptive: HashMap<String, HttpAdaptiveConcurrency>,
    pub(super) range_lengths: Vec<u64>,
    pub(super) executor: HttpSegmentRequestExecutor,
    pub(super) connection_guard: ActiveConnectionGuard,
    pub(super) write_tx: mpsc::Sender<WriteChunk>,
    pub(super) write_rx: mpsc::Receiver<WriteChunk>,
    pub(super) active: HashMap<u32, (usize, u64, Instant, u64)>,
    pub(super) progress_tracker: Arc<SegmentProgressTracker>,
    pub(super) segment_progress: HashMap<u32, Arc<SegmentProgress>>,
    pub(super) slow_range_recovery: SlowRangeRecovery,
    pub(super) segment_stall_timeout: Duration,
    pub(super) stall_check: tokio::time::Interval,
    pub(super) lifecycle_notify: Arc<tokio::sync::Notify>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn create(
    dl: &ConcurrentDownloader,
    uris: &[String],
    options: &DownloadOptions,
    max_retries_per_segment: u32,
    split: usize,
    max_conn: usize,
    session_limit: usize,
    piece_size: u64,
    coordinator: &MirrorCoordinator,
) -> RangeScheduler {
    let server_keys: Vec<String> = uris
        .iter()
        .map(|uri| authority_key(uri).unwrap_or_else(|| uri.clone()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let retry_wait = options.retry_wait;
    let retry_policy = RetryPolicy::new(max_retries_per_segment, retry_wait.saturating_mul(1000));
    let mut adaptive = HashMap::new();
    for key in &server_keys {
        adaptive.insert(
            key.clone(),
            HttpAdaptiveConcurrency::new(
                split,
                max_conn,
                session_limit,
                options.http2_streams_per_session(),
                retry_wait,
            ),
        );
    }
    let range_lengths = vec![piece_size; uris.len()];
    let executor = HttpSegmentRequestExecutor::new_with_clients(
        &dl.client,
        dl.range_clients.as_slice(),
        dl.request_policy.clone(),
        dl.cookie_helper.clone(),
        dl.auth_options.clone(),
        dl.netrc_path.clone(),
        split,
        &server_keys,
        max_conn,
    );
    if let Some((known_authority, version)) = &dl.initial_http_protocol {
        executor.set_protocol(known_authority, *version);
    }
    let connection_guard = ActiveConnectionGuard::new(Arc::clone(&dl.group));
    let (write_tx, write_rx) = mpsc::channel::<WriteChunk>(WRITE_CHANNEL_CAPACITY);
    let active: HashMap<u32, (usize, u64, Instant, u64)> = HashMap::new();
    let progress_tracker =
        SegmentProgressTracker::new(coordinator.completed_bytes(), Arc::clone(&dl.progress));
    let segment_progress: HashMap<u32, Arc<SegmentProgress>> = HashMap::new();
    let slow_range_recovery = SlowRangeRecovery::new();
    let segment_stall_timeout =
        Duration::from_secs(constants::HTTP_DEFAULT_SEGMENT_STALL_TIMEOUT_SECS);
    let stall_check_interval = Duration::from_secs(1);
    let mut stall_check = tokio::time::interval(stall_check_interval);
    stall_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Lifecycle changes wake the scheduler even when all segment requests are
    // blocked on slow network reads.
    let lifecycle_notify = dl.group.recover().lifecycle_notifier();

    RangeScheduler {
        retry_policy,
        adaptive,
        range_lengths,
        executor,
        connection_guard,
        write_tx,
        write_rx,
        active,
        progress_tracker,
        segment_progress,
        slow_range_recovery,
        segment_stall_timeout,
        stall_check,
        lifecycle_notify,
    }
}

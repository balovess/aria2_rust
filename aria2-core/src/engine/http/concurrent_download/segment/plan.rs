use crate::constants;
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::http::adaptive_concurrency::HttpAdaptiveConcurrency;
use crate::engine::http::request_executor::authority_key;
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::resume_helper::ResumeState;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::ConcurrentDownloader;
use super::super::effective_segment_count;
use super::super::fixed_piece_size::calculate_fixed_piece_size;

pub(super) struct SegmentPlan {
    pub(super) split: usize,
    pub(super) piece_length: u32,
    pub(super) max_conn: usize,
    pub(super) retry_policy: RetryPolicy,
    pub(super) authority_key: String,
    pub(super) adaptive: HttpAdaptiveConcurrency,
    pub(super) manager: ConcurrentSegmentManager,
}

pub(super) fn create(
    dl: &ConcurrentDownloader,
    uri: &str,
    total_length: u64,
    resume_state: &ResumeState,
    max_retries_per_segment: u32,
) -> Result<SegmentPlan> {
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
    let session_limit = (options
        .max_http2_sessions_per_server
        .unwrap_or(constants::DEFAULT_HTTP2_SESSIONS_PER_SERVER as u16)
        .clamp(1, max_conn as u16) as usize)
        .min(dl.range_clients.len().max(1));
    let adaptive = HttpAdaptiveConcurrency::new(
        split,
        max_conn,
        session_limit,
        options.http2_streams_per_session(),
        options.retry_wait,
    );

    tracing::info!(
        "Concurrent download started: split_budget={}, requested_split={}, min_split_size={}, max_conn={}, fixed_piece_size={} bytes, total={}",
        split,
        requested_split,
        min_split_size,
        max_conn,
        piece_size,
        total_length
    );

    let mut manager =
        ConcurrentSegmentManager::new(total_length, vec![uri.to_string()], Some(piece_size));
    // MirrorState selects segments; the request executor enforces the actual
    // authority-wide connection limit, including aliases for one server.
    manager.set_max_connections_per_mirror(split);
    manager.set_max_retries(max_retries_per_segment);

    Ok(SegmentPlan {
        split,
        piece_length,
        max_conn,
        retry_policy,
        authority_key,
        adaptive,
        manager,
    })
}

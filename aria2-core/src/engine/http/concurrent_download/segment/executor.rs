use tokio::sync::mpsc;

use crate::engine::command::WRITE_CHANNEL_CAPACITY;
use crate::engine::http::adaptive_concurrency::HttpAdaptiveConcurrency;
use crate::engine::http::request_executor::HttpSegmentRequestExecutor;
use crate::engine::http::segment_downloader::WriteChunk;
use crate::request::request_group::ActiveConnectionGuard;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::ConcurrentDownloader;

pub(super) struct SegmentRequests {
    pub(super) write_tx: mpsc::Sender<WriteChunk>,
    pub(super) write_rx: mpsc::Receiver<WriteChunk>,
    pub(super) executor: HttpSegmentRequestExecutor,
    pub(super) connection_guard: ActiveConnectionGuard,
    pub(super) lifecycle_notify: std::sync::Arc<tokio::sync::Notify>,
}

pub(super) fn create(
    dl: &ConcurrentDownloader,
    split: usize,
    max_conn: usize,
    authority_key: &str,
    adaptive: &HttpAdaptiveConcurrency,
) -> SegmentRequests {
    let (write_tx, write_rx) = mpsc::channel::<WriteChunk>(WRITE_CHANNEL_CAPACITY);
    let authorities = [authority_key.to_owned()];
    let executor = HttpSegmentRequestExecutor::new_with_clients(
        &dl.client,
        dl.range_clients.as_slice(),
        dl.request_policy.clone(),
        dl.cookie_helper.clone(),
        dl.auth_options.clone(),
        dl.netrc_path.clone(),
        split,
        &authorities,
        max_conn,
    );
    if let Some((known_authority, version)) = &dl.initial_http_protocol
        && known_authority == authority_key
    {
        executor.set_protocol(authority_key, *version);
        let is_http2 = *version == reqwest::Version::HTTP_2;
        executor.set_target(authority_key, adaptive.range_target(is_http2));
        if is_http2 {
            executor.set_active_h2_sessions(authority_key, adaptive.connection_target(is_http2));
        }
    }
    let connection_guard = ActiveConnectionGuard::new(std::sync::Arc::clone(&dl.group));
    let lifecycle_notify = dl.group.recover().lifecycle_notifier();

    SegmentRequests {
        write_tx,
        write_rx,
        executor,
        connection_guard,
        lifecycle_notify,
    }
}

pub(super) fn update_capacity(
    executor: &mut HttpSegmentRequestExecutor,
    adaptive: &mut HttpAdaptiveConcurrency,
    authority_key: &str,
    split: usize,
    download_complete: bool,
) {
    let is_http2 = executor.is_http2(authority_key);
    if executor.in_flight_for(authority_key) == 0 && !download_complete {
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
    executor.set_target(authority_key, adaptive.range_target(is_http2));
    if is_http2 {
        executor.set_active_h2_sessions(authority_key, adaptive.connection_target(is_http2));
    }
}

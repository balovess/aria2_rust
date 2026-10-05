use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::mpsc;

use crate::engine::http::cookie_helper::CookieHelper;
use crate::engine::http::segment_downloader::HttpSegmentDownloader;
use crate::http::{AuthResolveOptions, HttpRequestPolicy};

use super::admission::{ExecutorState, RunningTask};
use super::{HttpSegmentRequest, HttpSegmentRequestResult, MAX_SERVER_CONCURRENCY};
/// Dynamic executor for admitted HTTP Range requests.
///
/// `total_limit` is the per-download `split` budget. Each authority also has
/// an independent target controlled by `HttpAdaptiveConcurrency`. There is no
/// fixed worker count: each admitted request owns one Tokio task until its
/// HTTP response completes.
pub struct HttpSegmentRequestExecutor {
    pub(super) result_rx: mpsc::Receiver<HttpSegmentRequestResult>,
    pub(super) result_tx: mpsc::Sender<HttpSegmentRequestResult>,
    pub(super) clients: Vec<reqwest::Client>,
    pub(super) request_policy: HttpRequestPolicy,
    pub(super) cookie_helper: CookieHelper,
    pub(super) auth_options: AuthResolveOptions,
    pub(super) netrc_path: Option<String>,
    pub(super) state: Arc<ExecutorState>,
    pub(super) total_limit: usize,
    pub(super) tasks: Vec<RunningTask>,
    pub(super) next_task_id: u64,
}

impl HttpSegmentRequestExecutor {
    #[allow(clippy::too_many_arguments)]
    #[allow(dead_code)] // Preserve the single-client constructor for existing callers.
    pub fn new(
        client: &reqwest::Client,
        request_policy: HttpRequestPolicy,
        cookie_helper: CookieHelper,
        auth_options: AuthResolveOptions,
        netrc_path: Option<String>,
        total_limit: usize,
        authority_keys: &[String],
        server_hard_limit: usize,
    ) -> Self {
        Self::new_with_clients(
            client,
            std::slice::from_ref(client),
            request_policy,
            cookie_helper,
            auth_options,
            netrc_path,
            total_limit,
            authority_keys,
            server_hard_limit,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_clients(
        client: &reqwest::Client,
        range_clients: &[reqwest::Client],
        request_policy: HttpRequestPolicy,
        cookie_helper: CookieHelper,
        auth_options: AuthResolveOptions,
        netrc_path: Option<String>,
        total_limit: usize,
        authority_keys: &[String],
        server_hard_limit: usize,
    ) -> Self {
        let total_limit = total_limit.max(1);
        let (result_tx, result_rx) = mpsc::channel(total_limit);
        let session_limit = server_hard_limit
            .clamp(1, MAX_SERVER_CONCURRENCY)
            .min(range_clients.len().max(1));
        let clients = if range_clients.is_empty() {
            vec![client.clone()]
        } else {
            range_clients.iter().take(session_limit).cloned().collect()
        };
        let state = Arc::new(ExecutorState::new(
            authority_keys,
            total_limit,
            server_hard_limit,
            clients.len(),
        ));

        Self {
            result_rx,
            result_tx,
            clients,
            request_policy,
            cookie_helper,
            auth_options,
            netrc_path,
            state,
            total_limit,
            tasks: Vec::new(),
            next_task_id: 1,
        }
    }

    /// Admit and start a request if both total and authority targets have room.
    pub fn try_submit(&mut self, request: HttpSegmentRequest) -> Option<u64> {
        let authority = self.state.authority(&request.authority_key)?;
        let (mut lease, client_index) = self.state.try_acquire(&authority, self.total_limit)?;
        let session_pool_slots_busy = authority
            .client_in_flight
            .iter()
            .filter(|in_flight| in_flight.load(Ordering::Acquire) > 0)
            .count();
        let protocol_code = authority.protocol.load(Ordering::Acquire);
        let protocol = match protocol_code {
            1 => "http/1.1",
            2 => "http/2",
            _ => "unknown",
        };
        let physical_connection_target = if protocol_code == 2 {
            authority.active_h2_sessions.load(Ordering::Acquire)
        } else {
            authority.target.load(Ordering::Acquire)
        };
        tracing::debug!(
            authority = %request.authority_key,
            segment_index = request.segment_index,
            offset = request.offset,
            length = request.length,
            session_pool_slot = client_index,
            session_pool_slots_busy,
            physical_connection_target,
            active_ranges_for_server = authority.in_flight.load(Ordering::Acquire),
            range_target = authority.target.load(Ordering::Acquire),
            protocol,
            "Admitted HTTP Range request"
        );
        let client = self.clients[client_index].clone();
        let fallback_client = self.clients[0].clone();
        let authority_state = Arc::clone(&authority);
        let warmup = authority.client_warmup(client_index);
        let request_policy = self.request_policy.clone();
        let cookie_helper = self.cookie_helper.clone();
        let auth_options = self.auth_options.clone();
        let netrc_path = self.netrc_path.clone();
        let result_tx = self.result_tx.clone();
        let authority_key = request.authority_key.clone();
        let task_id = self.next_task_id;
        self.next_task_id = self.next_task_id.wrapping_add(1).max(1);

        let handle = tokio::spawn(async move {
            let mut used_client_index = client_index;
            let make_downloader = |client: &reqwest::Client| {
                HttpSegmentDownloader::new_with_policy(client, request_policy.clone())
                    .with_cookie_helper(cookie_helper.clone())
                    .with_auth_options(auth_options.clone(), netrc_path.clone())
            };
            let mut downloader = make_downloader(&client);
            // One successful one-byte Range response establishes this
            // client's connection before its other requests are dispatched.
            // Without this gate, cold concurrent requests can each open a new
            // HTTP/2 TCP session before the pool learns that one is reusable.
            let protocol = authority_state.protocol.load(Ordering::Acquire);
            let session_result = if protocol == 1 {
                Ok(())
            } else {
                warmup
                    .get_or_init(|| async {
                        match downloader
                            .download_range(
                                &request.url,
                                0,
                                1,
                                request.cookie_header.as_deref(),
                                &[],
                                None,
                                request.expected_entity_length,
                            )
                            .await
                        {
                            Ok(_) => Ok(()),
                            Err(error) => Err(error),
                        }
                    })
                    .await
                    .clone()
            };
            let mut session_capacity_error = None;
            if let Err(error) = session_result {
                if crate::engine::http::concurrent_download::is_capacity_limited_error(&error) {
                    tracing::warn!(
                        client_index,
                        %error,
                        "HTTP/2 session warmup hit server connection capacity"
                    );
                    session_capacity_error = Some(error);
                } else {
                    tracing::debug!(
                        client_index,
                        %error,
                        "HTTP range session warmup failed; retrying on the primary client"
                    );
                    if client_index != 0 {
                        let active_sessions = authority_state
                            .active_h2_sessions
                            .load(Ordering::Acquire)
                            .max(1);
                        let slot_limit = authority_state
                            .target
                            .load(Ordering::Acquire)
                            .div_ceil(active_sessions)
                            .max(1);
                        if lease.try_reassign_client_slot(0, slot_limit) {
                            used_client_index = 0;
                            downloader = make_downloader(&fallback_client);
                        }
                    }
                }
            }
            downloader.clear_last_peer_addr();
            downloader.clear_last_http_version();
            let transfer_started = std::time::Instant::now();
            let (result, range_size_rejected) = if let Some(error) = session_capacity_error {
                (Err(error), false)
            } else {
                match downloader
                    .download_range_streaming_with_progress(
                        &request.url,
                        request.offset,
                        request.length,
                        request.cookie_header.as_deref(),
                        &[],
                        Some(&request.progress),
                        &request.write_tx,
                        request.expected_entity_length,
                    )
                    .await
                {
                    Ok(bytes) => (Ok(bytes), false),
                    Err(failure) => (Err(failure.error), failure.explicit_size_rejection),
                }
            };
            let http_version = downloader.last_http_version();
            let downloaded_bytes = result.as_ref().map(|bytes| *bytes).unwrap_or(0);
            let elapsed = transfer_started.elapsed();
            let goodput_mib_per_sec = if elapsed.as_secs_f64() > 0.0 {
                downloaded_bytes as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0)
            } else {
                0.0
            };
            tracing::debug!(
                authority = %authority_key,
                task_id,
                segment_index = request.segment_index,
                offset = request.offset,
                requested_length = request.length,
                downloaded_bytes,
                elapsed_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
                goodput_mib_per_sec,
                session_pool_slot = used_client_index,
                protocol = ?http_version,
                success = result.is_ok(),
                "HTTP Range transfer finished"
            );
            if let Some(version) = http_version {
                tracing::debug!(
                    authority = %authority_key,
                    ?version,
                "HTTP Range negotiated protocol"
                );
                if version == reqwest::Version::HTTP_2 {
                    authority_state.protocol.store(2, Ordering::Release);
                } else {
                    authority_state.protocol.store(1, Ordering::Release);
                }
            }
            let request_result = HttpSegmentRequestResult {
                task_id,
                segment_index: request.segment_index,
                authority_key,
                result,
                range_size_limit: request.range_size_limit,
                range_size_rejected,
                peer_addr: downloader.last_peer_addr(),
                _lease: lease,
            };

            // If the scheduler has gone away, dropping the result also drops
            // the lease and releases both admission counters.
            if result_tx.send(request_result).await.is_err() {
                tracing::warn!(
                    task_id,
                    segment_index = request.segment_index,
                    "HTTP Range completion receiver was closed"
                );
            }
        });
        self.tasks.push(RunningTask {
            id: task_id,
            segment_index: request.segment_index,
            handle,
        });
        Some(task_id)
    }

    pub(crate) fn set_target(&self, authority_key: &str, target: usize) {
        if let Some(authority) = self.state.authority(authority_key) {
            authority
                .target
                .store(target.clamp(1, authority.hard_limit), Ordering::Release);
        }
    }

    pub(crate) fn set_protocol(&self, authority_key: &str, version: reqwest::Version) {
        if let Some(authority) = self.state.authority(authority_key) {
            let protocol = if version == reqwest::Version::HTTP_2 {
                2
            } else {
                1
            };
            authority.protocol.store(protocol, Ordering::Release);
        }
    }

    pub(crate) fn set_active_h2_sessions(&self, authority_key: &str, sessions: usize) {
        if let Some(authority) = self.state.authority(authority_key) {
            authority.active_h2_sessions.store(
                sessions.clamp(1, authority.client_in_flight.len()),
                Ordering::Release,
            );
        }
    }

    pub(crate) fn is_http2(&self, authority_key: &str) -> bool {
        self.state
            .authority(authority_key)
            .is_some_and(|authority| authority.protocol.load(Ordering::Acquire) == 2)
    }

    /// Includes completed results waiting to be consumed by the scheduler.
    pub fn in_flight(&self) -> usize {
        self.state.total_in_flight.load(Ordering::Acquire)
    }

    pub fn in_flight_for(&self, authority_key: &str) -> usize {
        self.state
            .authority(authority_key)
            .map(|authority| authority.in_flight.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    pub fn finished_tasks(&self) -> Vec<(u64, u32)> {
        self.tasks
            .iter()
            .filter(|task| task.handle.is_finished())
            .map(|task| (task.id, task.segment_index))
            .collect()
    }

    /// Receive one completion without awaiting task cleanup.
    ///
    /// This future is selected alongside write and progress events, so it must
    /// not await after consuming the channel message: cancellation at that
    /// point would drop the result and its admission lease before the scheduler
    /// can reconcile the Range. Call `reap_task` after this branch is selected.
    pub async fn next_result(&mut self) -> Option<HttpSegmentRequestResult> {
        self.result_rx.recv().await
    }

    /// Abort one stalled Range request and release its admission lease.
    pub async fn abort_segment(&mut self, segment_index: u32) -> bool {
        let Some(index) = self
            .tasks
            .iter()
            .position(|task| task.segment_index == segment_index)
        else {
            return false;
        };
        let task = self.tasks.swap_remove(index);
        task.handle.abort();
        let _ = task.handle.await;
        true
    }

    /// Reclaim the task handle associated with a completion event.
    ///
    /// A request sends its result immediately before returning, so awaiting
    /// this exact handle is short and deterministic. This keeps task storage
    /// bounded by requests that have not yet emitted a completion event.
    pub(crate) async fn reap_task(&mut self, task_id: u64) {
        let Some(index) = self.tasks.iter().position(|task| task.id == task_id) else {
            return;
        };
        let task = self.tasks.swap_remove(index);
        if let Err(error) = task.handle.await {
            tracing::warn!(
                task_id,
                segment_index = task.segment_index,
                %error,
                "HTTP Range task failed while being reaped"
            );
        }
    }

    /// Wait for all admitted requests after the scheduler has drained results.
    pub async fn shutdown(mut self) {
        while let Some(task) = self.tasks.pop() {
            let _ = task.handle.await;
        }
    }

    /// Abort all admitted requests. Used for cancellation and Range fallback.
    pub async fn cancel(mut self) {
        for task in &self.tasks {
            task.handle.abort();
        }
        while let Some(task) = self.tasks.pop() {
            let _ = task.handle.await;
        }
    }
}

impl Drop for HttpSegmentRequestExecutor {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.handle.abort();
        }
    }
}

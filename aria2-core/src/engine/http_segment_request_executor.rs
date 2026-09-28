//! Admission and execution for HTTP Range requests.
//!
//! This module owns per-download request admission and assigns admitted ranges
//! across independent reqwest pools. Each pool is warmed once per authority so
//! HTTP/2 can multiplex later range streams over a bounded set of TCP sessions.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use dashmap::DashMap;
use tokio::sync::{OnceCell, mpsc};

use crate::engine::download_cookie::CookieHelper;
use crate::engine::http_segment_downloader::{HttpSegmentDownloader, SegmentProgress, WriteChunk};
use crate::error::{Aria2Error, Result};
use crate::http::{AuthResolveOptions, HttpRequestPolicy};

const MAX_SERVER_CONCURRENCY: usize = 16;

/// One HTTP byte-range request admitted for execution.
pub struct HttpSegmentRequest {
    pub segment_index: u32,
    /// Authority bucket used for per-server request admission.
    pub authority_key: String,
    pub url: String,
    pub offset: u64,
    pub length: u64,
    pub cookie_header: Option<String>,
    pub(crate) progress: Arc<SegmentProgress>,
    pub write_tx: mpsc::Sender<WriteChunk>,
    pub expected_entity_length: u64,
}

/// Result returned after one Range request finishes.
pub struct HttpSegmentRequestResult {
    /// Internal identity used to reclaim the task handle when this completion
    /// event is consumed. The result channel is the completion signal, so the
    /// scheduler never needs to probe every handle for readiness.
    pub(crate) task_id: u64,
    pub segment_index: u32,
    pub authority_key: String,
    pub result: Result<u64>,
    pub peer_addr: Option<std::net::SocketAddr>,
    // The lease intentionally remains attached to the result. A completed
    // request still counts as in-flight until the scheduler consumes it.
    _lease: AdmissionLease,
}

struct AuthorityState {
    /// Per-download logical Range budget (`split`).
    hard_limit: usize,
    /// Per-download physical connection ceiling (`max-connection-per-server`).
    connection_limit: usize,
    target: AtomicUsize,
    in_flight: AtomicUsize,
    client_in_flight: Arc<Vec<AtomicUsize>>,
    http1_client_slot_limit: usize,
    next_client_index: AtomicUsize,
    protocol: AtomicU8,
    active_h2_sessions: AtomicUsize,
    client_warmups: DashMap<usize, Arc<OnceCell<std::result::Result<(), Aria2Error>>>>,
}

struct ExecutorState {
    authorities: DashMap<Box<str>, Arc<AuthorityState>>,
    total_in_flight: AtomicUsize,
}

struct AdmissionLease {
    state: Arc<ExecutorState>,
    authority: Arc<AuthorityState>,
    client_in_flight: Arc<Vec<AtomicUsize>>,
    client_index: usize,
}

impl Drop for AdmissionLease {
    fn drop(&mut self) {
        self.authority.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.state.total_in_flight.fetch_sub(1, Ordering::AcqRel);
        self.client_in_flight[self.client_index].fetch_sub(1, Ordering::AcqRel);
    }
}

impl AdmissionLease {
    fn try_reassign_client_slot(&mut self, client_index: usize, limit: usize) -> bool {
        if self.client_index == client_index {
            return true;
        }
        if !reserve_bounded(&self.client_in_flight[client_index], limit) {
            return false;
        }
        self.client_in_flight[self.client_index].fetch_sub(1, Ordering::AcqRel);
        self.client_index = client_index;
        true
    }
}

struct RunningTask {
    id: u64,
    segment_index: u32,
    handle: tokio::task::JoinHandle<()>,
}

/// Dynamic executor for admitted HTTP Range requests.
///
/// `total_limit` is the per-download `split` budget. Each authority also has
/// an independent target controlled by `HttpAdaptiveConcurrency`. There is no
/// fixed worker count: each admitted request owns one Tokio task until its
/// HTTP response completes.
pub struct HttpSegmentRequestExecutor {
    result_rx: mpsc::Receiver<HttpSegmentRequestResult>,
    result_tx: mpsc::Sender<HttpSegmentRequestResult>,
    clients: Vec<reqwest::Client>,
    request_policy: HttpRequestPolicy,
    cookie_helper: CookieHelper,
    auth_options: AuthResolveOptions,
    netrc_path: Option<String>,
    state: Arc<ExecutorState>,
    total_limit: usize,
    tasks: Vec<RunningTask>,
    next_task_id: u64,
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
                if crate::engine::concurrent_download::is_capacity_limited_error(&error) {
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
            let result = if let Some(error) = session_capacity_error {
                Err(error)
            } else {
                downloader
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

impl ExecutorState {
    fn new(
        authority_keys: &[String],
        total_limit: usize,
        server_hard_limit: usize,
        session_count: usize,
    ) -> Self {
        let hard_limit = total_limit.max(1);
        let connection_limit = server_hard_limit.clamp(1, MAX_SERVER_CONCURRENCY);
        let session_count = session_count.max(1);
        let http1_client_slot_limit = hard_limit.div_ceil(session_count).max(1);
        let authorities = DashMap::new();
        for key in authority_keys {
            let client_warmups = DashMap::new();
            let primary_session_ready = Arc::new(OnceCell::new());
            let _ = primary_session_ready.set(Ok(()));
            client_warmups.insert(0, primary_session_ready);
            authorities.insert(
                key.clone().into_boxed_str(),
                Arc::new(AuthorityState {
                    hard_limit,
                    connection_limit,
                    target: AtomicUsize::new(hard_limit),
                    in_flight: AtomicUsize::new(0),
                    client_in_flight: Arc::new(
                        (0..session_count).map(|_| AtomicUsize::new(0)).collect(),
                    ),
                    http1_client_slot_limit,
                    next_client_index: AtomicUsize::new(0),
                    protocol: AtomicU8::new(0),
                    active_h2_sessions: AtomicUsize::new(1),
                    client_warmups,
                }),
            );
        }
        Self {
            authorities,
            total_in_flight: AtomicUsize::new(0),
        }
    }

    fn authority(&self, authority_key: &str) -> Option<Arc<AuthorityState>> {
        self.authorities
            .get(authority_key)
            .map(|entry| Arc::clone(entry.value()))
    }

    fn try_acquire(
        self: &Arc<Self>,
        authority: &Arc<AuthorityState>,
        total_limit: usize,
    ) -> Option<(AdmissionLease, usize)> {
        if !reserve_total(&self.total_in_flight, total_limit) {
            return None;
        }
        if !reserve_authority(authority) {
            self.total_in_flight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        let Some(client_index) = authority.reserve_client_slot() else {
            authority.in_flight.fetch_sub(1, Ordering::AcqRel);
            self.total_in_flight.fetch_sub(1, Ordering::AcqRel);
            return None;
        };
        Some((
            AdmissionLease {
                state: Arc::clone(self),
                authority: Arc::clone(authority),
                client_in_flight: Arc::clone(&authority.client_in_flight),
                client_index,
            },
            client_index,
        ))
    }
}

impl AuthorityState {
    fn reserve_client_slot(&self) -> Option<usize> {
        let count = self.client_in_flight.len();
        let protocol = self.protocol.load(Ordering::Acquire);
        let session_count = if protocol == 2 {
            self.active_h2_sessions
                .load(Ordering::Acquire)
                .clamp(1, count)
        } else {
            count
        };
        let client_slot_limit = if protocol == 2 {
            self.target
                .load(Ordering::Acquire)
                .div_ceil(session_count)
                .max(1)
        } else {
            self.http1_client_slot_limit
        };
        let start = self.next_client_index.fetch_add(1, Ordering::Relaxed) % session_count;
        for offset in 0..session_count {
            let index = (start + offset) % session_count;
            if reserve_bounded(&self.client_in_flight[index], client_slot_limit) {
                return Some(index);
            }
        }
        None
    }

    fn client_warmup(
        &self,
        client_index: usize,
    ) -> Arc<OnceCell<std::result::Result<(), Aria2Error>>> {
        self.client_warmups
            .entry(client_index)
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone()
    }
}

fn reserve_total(total: &AtomicUsize, limit: usize) -> bool {
    let mut current = total.load(Ordering::Acquire);
    loop {
        if current >= limit {
            return false;
        }
        match total.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

fn reserve_authority(authority: &AuthorityState) -> bool {
    let protocol = authority.protocol.load(Ordering::Acquire);
    let hard_limit = if protocol == 2 {
        authority.hard_limit
    } else {
        authority.connection_limit
    };
    let target = authority.target.load(Ordering::Acquire).min(hard_limit);
    let mut current = authority.in_flight.load(Ordering::Acquire);
    loop {
        if current >= target {
            return false;
        }
        match authority.in_flight.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

fn reserve_bounded(value: &AtomicUsize, limit: usize) -> bool {
    let mut current = value.load(Ordering::Acquire);
    loop {
        if current >= limit {
            return false;
        }
        match value.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

/// Return the authority used for per-server request accounting.
pub fn authority_key(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    let port = url.port_or_known_default()?;
    Some(format!(
        "{}://{host}:{port}",
        url.scheme().to_ascii_lowercase()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_key_includes_scheme_and_ignores_path() {
        assert_eq!(
            authority_key("HTTP://Example.TEST/download/file").as_deref(),
            Some("http://example.test:80")
        );
        assert_eq!(
            authority_key("https://[::1]/file").as_deref(),
            Some("https://[::1]:443")
        );
        assert_ne!(
            authority_key("http://example.test/file"),
            authority_key("https://example.test/file")
        );
    }

    #[test]
    fn leases_keep_completed_requests_in_flight_until_consumed() {
        let state = Arc::new(ExecutorState::new(
            &["http://example.test:80".into()],
            4,
            4,
            1,
        ));
        let authority = state.authority("http://example.test:80").unwrap();
        let (lease, client_index) = state.try_acquire(&authority, 2).unwrap();
        assert_eq!(client_index, 0);
        assert_eq!(state.total_in_flight.load(Ordering::Acquire), 1);
        assert_eq!(authority.in_flight.load(Ordering::Acquire), 1);
        assert_eq!(authority.client_in_flight[0].load(Ordering::Acquire), 1);
        drop(lease);
        assert_eq!(state.total_in_flight.load(Ordering::Acquire), 0);
        assert_eq!(authority.in_flight.load(Ordering::Acquire), 0);
        assert_eq!(authority.client_in_flight[0].load(Ordering::Acquire), 0);
    }

    #[test]
    fn total_and_authority_limits_are_independent() {
        let state = Arc::new(ExecutorState::new(
            &["http://one.test:80".into(), "http://two.test:80".into()],
            2,
            2,
            1,
        ));
        let one = state.authority("http://one.test:80").unwrap();
        let two = state.authority("http://two.test:80").unwrap();
        let (first, _) = state.try_acquire(&one, 2).unwrap();
        let (second, _) = state.try_acquire(&one, 2).unwrap();
        assert!(state.try_acquire(&two, 2).is_none());
        drop(first);
        drop(second);
        assert!(state.try_acquire(&two, 2).is_some());
    }

    #[test]
    fn h2_prefers_streams_on_each_session_before_opening_another() {
        let state = Arc::new(ExecutorState::new(
            &["https://h2.test:443".into()],
            16,
            16,
            4,
        ));
        let authority = state.authority("https://h2.test:443").unwrap();
        authority.protocol.store(2, Ordering::Release);
        authority.active_h2_sessions.store(1, Ordering::Release);
        let stream_probe: Vec<_> = (0..4)
            .map(|_| state.try_acquire(&authority, 16).unwrap())
            .collect();
        assert!(stream_probe.iter().all(|(_, index)| *index == 0));
        drop(stream_probe);

        authority.active_h2_sessions.store(2, Ordering::Release);
        authority.target.store(4, Ordering::Release);
        let session_probe: Vec<_> = (0..4)
            .map(|_| state.try_acquire(&authority, 16).unwrap())
            .collect();
        let slots: Vec<_> = session_probe.iter().map(|(_, index)| *index).collect();
        assert_eq!(slots, vec![0, 1, 0, 1]);
    }

    #[test]
    fn h1_spreads_active_requests_across_session_pools() {
        let state = Arc::new(ExecutorState::new(&["http://h1.test:80".into()], 16, 16, 4));
        let authority = state.authority("http://h1.test:80").unwrap();
        authority.protocol.store(1, Ordering::Release);
        let reservations: Vec<_> = (0..4)
            .map(|_| state.try_acquire(&authority, 16).unwrap())
            .collect();
        let slots: Vec<_> = reservations.iter().map(|(_, index)| *index).collect();
        assert_eq!(slots, vec![0, 1, 2, 3]);
    }

    #[test]
    fn split_budget_is_distinct_from_connection_ceiling_for_http2() {
        let state = Arc::new(ExecutorState::new(
            &["https://single-session.test:443".into()],
            16,
            1,
            1,
        ));
        let authority = state.authority("https://single-session.test:443").unwrap();
        authority.protocol.store(2, Ordering::Release);
        authority.active_h2_sessions.store(1, Ordering::Release);

        let reservations: Vec<_> = (0..16)
            .map(|_| state.try_acquire(&authority, 16).unwrap())
            .collect();
        assert!(reservations.iter().all(|(_, index)| *index == 0));
        assert_eq!(authority.in_flight.load(Ordering::Acquire), 16);
        assert!(state.try_acquire(&authority, 16).is_none());
    }

    #[test]
    fn h2_stream_capacity_grows_on_the_active_session_before_another_is_added() {
        let state = Arc::new(ExecutorState::new(
            &["https://adaptive.test:443".into()],
            16,
            16,
            4,
        ));
        let authority = state.authority("https://adaptive.test:443").unwrap();
        authority.protocol.store(2, Ordering::Release);
        authority.active_h2_sessions.store(1, Ordering::Release);
        authority.target.store(8, Ordering::Release);

        let reservations: Vec<_> = (0..8)
            .map(|_| state.try_acquire(&authority, 16).unwrap())
            .collect();

        assert!(reservations.iter().all(|(_, index)| *index == 0));
        assert_eq!(authority.in_flight.load(Ordering::Acquire), 8);
    }

    #[tokio::test]
    async fn completion_event_reclaims_only_its_task() {
        crate::http::client_pool::ensure_rustls_provider();
        let (result_tx, result_rx) = mpsc::channel(1);
        let mut executor = HttpSegmentRequestExecutor {
            result_rx,
            result_tx,
            clients: vec![reqwest::Client::new()],
            request_policy: HttpRequestPolicy::default(),
            cookie_helper: CookieHelper::new(
                Arc::new(crate::http::cookie::CookieStorage::new()),
                None,
            ),
            auth_options: AuthResolveOptions::default(),
            netrc_path: None,
            state: Arc::new(ExecutorState::new(&[], 1, 1, 1)),
            total_limit: 1,
            tasks: vec![
                RunningTask {
                    id: 1,
                    segment_index: 1,
                    handle: tokio::spawn(async {}),
                },
                RunningTask {
                    id: 2,
                    segment_index: 2,
                    handle: tokio::spawn(async {}),
                },
            ],
            next_task_id: 3,
        };

        executor.reap_task(2).await;
        assert_eq!(executor.tasks.len(), 1);
        assert_eq!(executor.tasks[0].id, 1);

        executor.cancel().await;
    }

    #[tokio::test]
    async fn selecting_next_result_does_not_drop_a_received_completion() {
        crate::http::client_pool::ensure_rustls_provider();
        let authority_key = "https://cancel-safe.test:443".to_owned();
        let state = Arc::new(ExecutorState::new(
            std::slice::from_ref(&authority_key),
            1,
            1,
            1,
        ));
        let authority = state.authority(&authority_key).unwrap();
        let (lease, _) = state.try_acquire(&authority, 1).unwrap();
        let (result_tx, result_rx) = mpsc::channel(1);
        let executor_result_tx = result_tx.clone();
        let (sent_tx, sent_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            result_tx
                .send(HttpSegmentRequestResult {
                    task_id: 1,
                    segment_index: 0,
                    authority_key,
                    result: Ok(1),
                    peer_addr: None,
                    _lease: lease,
                })
                .await
                .unwrap();
            let _ = sent_tx.send(());
            let _ = release_rx.await;
        });
        let mut executor = HttpSegmentRequestExecutor {
            result_rx,
            result_tx: executor_result_tx,
            clients: vec![reqwest::Client::new()],
            request_policy: HttpRequestPolicy::default(),
            cookie_helper: CookieHelper::new(
                Arc::new(crate::http::cookie::CookieStorage::new()),
                None,
            ),
            auth_options: AuthResolveOptions::default(),
            netrc_path: None,
            state: Arc::clone(&state),
            total_limit: 1,
            tasks: vec![RunningTask {
                id: 1,
                segment_index: 0,
                handle: task,
            }],
            next_task_id: 2,
        };

        let result = tokio::select! {
            biased;
            result = executor.next_result() => Some(result.unwrap()),
            _ = sent_rx => None,
        };
        assert!(
            result.is_some(),
            "the result must win selection once it has been received"
        );

        let result = result.unwrap();
        let _ = release_tx.send(());
        executor.reap_task(result.task_id).await;
        assert_eq!(state.total_in_flight.load(Ordering::Acquire), 1);
        drop(result);
        assert_eq!(state.total_in_flight.load(Ordering::Acquire), 0);
    }
}

#[cfg(test)]
#[path = "http_segment_request_executor_h2_tests.rs"]
mod h2_tests;

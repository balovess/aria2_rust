//! Admission and execution for HTTP Range requests.
//!
//! This module deliberately does not model download tasks or transport
//! connections. The shared `reqwest::Client` owns transport connection reuse;
//! this module owns the per-download request budget and per-authority adaptive
//! budget.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;
use tokio::sync::mpsc;

use crate::engine::download_cookie::CookieHelper;
use crate::engine::http_segment_downloader::{HttpSegmentDownloader, SegmentProgress, WriteChunk};
use crate::error::Result;
use crate::http::{AuthResolveOptions, HttpRequestPolicy};

const MAX_SERVER_CONCURRENCY: usize = 16;

/// One HTTP byte-range request admitted for execution.
pub struct HttpSegmentRequest {
    pub mirror_index: usize,
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
    task_id: u64,
    pub mirror_index: usize,
    pub segment_index: u32,
    pub authority_key: String,
    pub result: Result<u64>,
    pub peer_addr: Option<std::net::SocketAddr>,
    // The lease intentionally remains attached to the result. A completed
    // request still counts as in-flight until the scheduler consumes it.
    _lease: AdmissionLease,
}

struct AuthorityState {
    hard_limit: usize,
    target: AtomicUsize,
    in_flight: AtomicUsize,
}

struct ExecutorState {
    authorities: DashMap<String, Arc<AuthorityState>>,
    total_in_flight: AtomicUsize,
}

struct AdmissionLease {
    state: Arc<ExecutorState>,
    authority: Arc<AuthorityState>,
}

impl Drop for AdmissionLease {
    fn drop(&mut self) {
        self.authority.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.state.total_in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

struct RunningTask {
    id: u64,
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
    client: reqwest::Client,
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
        let total_limit = total_limit.max(1);
        let (result_tx, result_rx) = mpsc::channel(total_limit);
        let state = Arc::new(ExecutorState::new(authority_keys, server_hard_limit));

        Self {
            result_rx,
            result_tx,
            client: client.clone(),
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
    pub fn try_submit(&mut self, request: HttpSegmentRequest) -> bool {
        let Some(authority) = self.state.authority(&request.authority_key) else {
            return false;
        };
        let Some(lease) = self.state.try_acquire(&authority, self.total_limit) else {
            return false;
        };

        let client = self.client.clone();
        let request_policy = self.request_policy.clone();
        let cookie_helper = self.cookie_helper.clone();
        let auth_options = self.auth_options.clone();
        let netrc_path = self.netrc_path.clone();
        let result_tx = self.result_tx.clone();
        let authority_key = request.authority_key.clone();
        let task_id = self.next_task_id;
        self.next_task_id = self.next_task_id.wrapping_add(1).max(1);

        let handle = tokio::spawn(async move {
            let downloader = HttpSegmentDownloader::new_with_policy(&client, request_policy)
                .with_cookie_helper(cookie_helper)
                .with_auth_options(auth_options, netrc_path);
            downloader.clear_last_peer_addr();
            let result = downloader
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
                .await;
            let request_result = HttpSegmentRequestResult {
                task_id,
                mirror_index: request.mirror_index,
                segment_index: request.segment_index,
                authority_key,
                result,
                peer_addr: downloader.last_peer_addr(),
                _lease: lease,
            };

            // If the scheduler has gone away, dropping the result also drops
            // the lease and releases both admission counters.
            let _ = result_tx.send(request_result).await;
        });
        self.tasks.push(RunningTask {
            id: task_id,
            handle,
        });
        true
    }

    pub fn set_target(&self, authority_key: &str, target: usize) {
        if let Some(authority) = self.state.authority(authority_key) {
            authority
                .target
                .store(target.clamp(1, authority.hard_limit), Ordering::Release);
        }
    }

    pub fn target_for(&self, authority_key: &str) -> Option<usize> {
        self.state
            .authority(authority_key)
            .map(|authority| authority.target.load(Ordering::Acquire))
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

    pub async fn next_result(&mut self) -> Option<HttpSegmentRequestResult> {
        let result = self.result_rx.recv().await?;
        self.reap_task(result.task_id).await;
        Some(result)
    }

    /// Reclaim the task handle associated with a completion event.
    ///
    /// A request sends its result immediately before returning, so awaiting
    /// this exact handle is short and deterministic. This keeps task storage
    /// bounded by requests that have not yet emitted a completion event.
    async fn reap_task(&mut self, task_id: u64) {
        let Some(index) = self.tasks.iter().position(|task| task.id == task_id) else {
            return;
        };
        let task = self.tasks.swap_remove(index);
        let _ = task.handle.await;
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
    fn new(authority_keys: &[String], server_hard_limit: usize) -> Self {
        let hard_limit = server_hard_limit.clamp(1, MAX_SERVER_CONCURRENCY);
        let authorities = DashMap::new();
        for key in authority_keys {
            authorities.insert(
                key.clone(),
                Arc::new(AuthorityState {
                    hard_limit,
                    target: AtomicUsize::new(hard_limit),
                    in_flight: AtomicUsize::new(0),
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
    ) -> Option<AdmissionLease> {
        if !reserve_total(&self.total_in_flight, total_limit) {
            return None;
        }
        if !reserve_authority(authority) {
            self.total_in_flight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(AdmissionLease {
            state: Arc::clone(self),
            authority: Arc::clone(authority),
        })
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
    let target = authority.target.load(Ordering::Acquire);
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
        let state = Arc::new(ExecutorState::new(&["http://example.test:80".into()], 4));
        let authority = state.authority("http://example.test:80").unwrap();
        let lease = state.try_acquire(&authority, 2).unwrap();
        assert_eq!(state.total_in_flight.load(Ordering::Acquire), 1);
        assert_eq!(authority.in_flight.load(Ordering::Acquire), 1);
        drop(lease);
        assert_eq!(state.total_in_flight.load(Ordering::Acquire), 0);
        assert_eq!(authority.in_flight.load(Ordering::Acquire), 0);
    }

    #[test]
    fn total_and_authority_limits_are_independent() {
        let state = Arc::new(ExecutorState::new(
            &["http://one.test:80".into(), "http://two.test:80".into()],
            2,
        ));
        let one = state.authority("http://one.test:80").unwrap();
        let two = state.authority("http://two.test:80").unwrap();
        let first = state.try_acquire(&one, 2).unwrap();
        let second = state.try_acquire(&one, 2).unwrap();
        assert!(state.try_acquire(&two, 2).is_none());
        drop(first);
        drop(second);
        assert!(state.try_acquire(&two, 2).is_some());
    }

    #[tokio::test]
    async fn completion_event_reclaims_only_its_task() {
        let (result_tx, result_rx) = mpsc::channel(1);
        let mut executor = HttpSegmentRequestExecutor {
            result_rx,
            result_tx,
            client: reqwest::Client::new(),
            request_policy: HttpRequestPolicy::default(),
            cookie_helper: CookieHelper::new(
                Arc::new(crate::http::cookie_storage::CookieStorage::new()),
                None,
            ),
            auth_options: AuthResolveOptions::default(),
            netrc_path: None,
            state: Arc::new(ExecutorState::new(&[], 1)),
            total_limit: 1,
            tasks: vec![
                RunningTask {
                    id: 1,
                    handle: tokio::spawn(async {}),
                },
                RunningTask {
                    id: 2,
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
}

//! Bounded, long-lived workers for HTTP range downloads.
//!
//! The pool owns the worker lifetime and admission control.  Callers only
//! submit range jobs and consume completed results; they do not create a new
//! request future for every segment.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;
use tokio::sync::{Mutex, mpsc};

use crate::engine::command::ProgressUpdate;
use crate::engine::download_cookie::CookieHelper;
use crate::engine::http_segment_downloader::{HttpSegmentDownloader, WriteChunk};
use crate::error::Result;
use crate::http::{AuthResolveOptions, HttpRequestPolicy};

const DEFAULT_QUEUE_CAPACITY: usize = 16;
const MAX_CONNECTIONS_PER_SERVER: usize = 16;
const MAX_WORKERS: usize = 16;

/// A single HTTP byte-range download submitted to the pool.
pub struct HttpSegmentJob {
    pub mirror_index: usize,
    pub segment_index: u32,
    /// Normalized HTTP authority used for per-server admission control.
    pub server_key: String,
    pub url: String,
    pub offset: u64,
    pub length: u64,
    pub cookie_header: Option<String>,
    pub progress_tx: mpsc::UnboundedSender<ProgressUpdate>,
    pub write_tx: mpsc::UnboundedSender<WriteChunk>,
    pub expected_entity_length: u64,
}

/// Result returned after a worker has finished one range request.
pub struct HttpSegmentResult {
    pub mirror_index: usize,
    pub segment_index: u32,
    pub server_key: String,
    pub result: Result<u64>,
    pub peer_addr: Option<std::net::SocketAddr>,
}

struct PoolState {
    servers: DashMap<String, Arc<ServerState>>,
    total_in_flight: AtomicUsize,
}

struct ServerState {
    hard_limit: usize,
    target: AtomicUsize,
    in_flight: AtomicUsize,
}

/// A bounded pool of long-lived HTTP range workers.
///
/// The pool has a fixed worker count for its lifetime.  `target` controls how
/// many jobs may be admitted at once, so adaptive throttling does not create
/// or destroy workers and cannot overshoot the current target.
pub struct HttpConnectionPool {
    task_tx: Option<mpsc::Sender<HttpSegmentJob>>,
    result_rx: mpsc::Receiver<HttpSegmentResult>,
    state: Arc<PoolState>,
    workers: Vec<tokio::task::JoinHandle<()>>,
}

impl HttpConnectionPool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: &reqwest::Client,
        request_policy: HttpRequestPolicy,
        cookie_helper: CookieHelper,
        auth_options: AuthResolveOptions,
        netrc_path: Option<String>,
        max_workers: usize,
        server_keys: &[String],
        hard_limit: usize,
    ) -> Self {
        let worker_count = max_workers.clamp(1, MAX_WORKERS);
        let queue_capacity = worker_count.max(DEFAULT_QUEUE_CAPACITY);
        let (task_tx, task_rx) = mpsc::channel(queue_capacity);
        let (result_tx, result_rx) = mpsc::channel(queue_capacity);
        let state = Arc::new(PoolState::new(server_keys, hard_limit));
        let shared_rx = Arc::new(Mutex::new(task_rx));

        let workers = (0..worker_count)
            .map(|_| {
                let task_rx = Arc::clone(&shared_rx);
                let result_tx = result_tx.clone();
                let state = Arc::clone(&state);
                let downloader =
                    HttpSegmentDownloader::new_with_policy(client, request_policy.clone())
                        .with_cookie_helper(cookie_helper.clone())
                        .with_auth_options(auth_options.clone(), netrc_path.clone());
                tokio::spawn(worker_loop(task_rx, result_tx, state, downloader))
            })
            .collect();

        Self {
            task_tx: Some(task_tx),
            result_rx,
            state,
            workers,
        }
    }

    /// Admit one job if its server target has room.
    pub fn try_submit(&self, job: HttpSegmentJob) -> bool {
        let Some(task_tx) = &self.task_tx else {
            return false;
        };

        let Some(server) = self.state.server(&job.server_key) else {
            return false;
        };
        if !reserve_slot(&server, &self.state.total_in_flight) {
            return false;
        }

        if task_tx.try_send(job).is_err() {
            release_slot(&server, &self.state.total_in_flight);
            return false;
        }
        true
    }

    pub fn set_target(&self, server_key: &str, target: usize) {
        if let Some(server) = self.state.server(server_key) {
            server
                .target
                .store(target.clamp(1, server.hard_limit), Ordering::Release);
        }
    }

    pub fn target_for(&self, server_key: &str) -> Option<usize> {
        self.state
            .server(server_key)
            .map(|server| server.target.load(Ordering::Acquire))
    }

    /// Includes results waiting to be consumed by the caller.  This prevents
    /// the scheduler from mistaking an unread result for an idle pool.
    pub fn in_flight(&self) -> usize {
        self.state.total_in_flight.load(Ordering::Acquire)
    }

    pub fn in_flight_for(&self, server_key: &str) -> usize {
        self.state
            .server(server_key)
            .map(|server| server.in_flight.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    pub async fn next_result(&mut self) -> Option<HttpSegmentResult> {
        let result = self.result_rx.recv().await;
        if let Some(result) = &result {
            self.state.release(&result.server_key);
        }
        result
    }

    /// Gracefully stop workers after the task queue has drained.
    pub async fn shutdown(mut self) {
        self.task_tx.take();
        while let Some(worker) = self.workers.pop() {
            let _ = worker.await;
        }
    }

    /// Immediately cancel workers and release all pool resources.
    pub async fn cancel(mut self) {
        self.task_tx.take();
        for worker in &self.workers {
            worker.abort();
        }
        while let Some(worker) = self.workers.pop() {
            let _ = worker.await;
        }
    }
}

impl Drop for HttpConnectionPool {
    fn drop(&mut self) {
        for worker in &self.workers {
            worker.abort();
        }
    }
}

impl PoolState {
    fn new(server_keys: &[String], hard_limit: usize) -> Self {
        let hard_limit = hard_limit.clamp(1, MAX_CONNECTIONS_PER_SERVER);
        let servers = DashMap::new();
        for key in server_keys {
            servers.insert(
                key.clone(),
                Arc::new(ServerState {
                    hard_limit,
                    target: AtomicUsize::new(hard_limit),
                    in_flight: AtomicUsize::new(0),
                }),
            );
        }
        Self {
            servers,
            total_in_flight: AtomicUsize::new(0),
        }
    }

    fn server(&self, server_key: &str) -> Option<Arc<ServerState>> {
        self.servers
            .get(server_key)
            .map(|entry| Arc::clone(entry.value()))
    }

    fn release(&self, server_key: &str) {
        if let Some(server) = self.server(server_key) {
            release_slot(&server, &self.total_in_flight);
        }
    }
}

/// Return the authority used for per-server connection accounting.
///
/// The key intentionally excludes the URL path, so mirrors serving different
/// paths on the same host and port share one limit.  A DNS name is used before
/// connecting because admission must happen before the request resolves; this
/// also keeps all requests to one configured authority in the same bucket.
pub fn server_key(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    let port = url.port_or_known_default()?;
    Some(format!("{host}:{port}"))
}

fn reserve_slot(server: &ServerState, total_in_flight: &AtomicUsize) -> bool {
    let target = server.target.load(Ordering::Acquire);
    let mut current = server.in_flight.load(Ordering::Acquire);
    loop {
        if current >= target {
            return false;
        }
        match server.in_flight.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                total_in_flight.fetch_add(1, Ordering::AcqRel);
                return true;
            }
            Err(observed) => current = observed,
        }
    }
}

fn release_slot(server: &ServerState, total_in_flight: &AtomicUsize) {
    server.in_flight.fetch_sub(1, Ordering::AcqRel);
    total_in_flight.fetch_sub(1, Ordering::AcqRel);
}

async fn worker_loop(
    task_rx: Arc<Mutex<mpsc::Receiver<HttpSegmentJob>>>,
    result_tx: mpsc::Sender<HttpSegmentResult>,
    state: Arc<PoolState>,
    downloader: HttpSegmentDownloader,
) {
    loop {
        let Some(job) = ({
            let mut task_rx = task_rx.lock().await;
            task_rx.recv().await
        }) else {
            return;
        };

        downloader.clear_last_peer_addr();
        let result = downloader
            .download_range_streaming(
                &job.url,
                job.offset,
                job.length,
                job.cookie_header.as_deref(),
                &[],
                Some(&job.progress_tx),
                &job.write_tx,
                job.expected_entity_length,
            )
            .await;
        let worker_result = HttpSegmentResult {
            mirror_index: job.mirror_index,
            segment_index: job.segment_index,
            server_key: job.server_key.clone(),
            result,
            peer_addr: downloader.last_peer_addr(),
        };

        if result_tx.send(worker_result).await.is_err() {
            state.release(&job.server_key);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_is_bounded_by_target() {
        let server = ServerState {
            hard_limit: 2,
            target: AtomicUsize::new(2),
            in_flight: AtomicUsize::new(0),
        };
        let total = AtomicUsize::new(0);
        assert!(reserve_slot(&server, &total));
        assert!(reserve_slot(&server, &total));
        assert!(!reserve_slot(&server, &total));
        release_slot(&server, &total);
        assert!(reserve_slot(&server, &total));
    }

    #[tokio::test]
    async fn target_cannot_exceed_per_server_limit() {
        crate::http::client_pool::ensure_rustls_provider();
        let client = reqwest::Client::new();
        let pool = HttpConnectionPool::new(
            &client,
            HttpRequestPolicy::default(),
            CookieHelper::new(
                std::sync::Arc::new(crate::http::cookie::CookieStorage::new()),
                None,
            ),
            AuthResolveOptions::default(),
            None,
            32,
            &["example.test:80".to_string()],
            32,
        );
        assert_eq!(pool.target_for("example.test:80"), Some(16));
        pool.cancel().await;
    }

    #[test]
    fn servers_have_independent_targets_and_slots() {
        let state = PoolState::new(&["one.test:80".to_string(), "two.test:80".to_string()], 4);
        let one = state.server("one.test:80").unwrap();
        let two = state.server("two.test:80").unwrap();

        assert!(reserve_slot(&one, &state.total_in_flight));
        assert!(reserve_slot(&one, &state.total_in_flight));
        assert!(reserve_slot(&two, &state.total_in_flight));
        assert_eq!(one.in_flight.load(Ordering::Acquire), 2);
        assert_eq!(two.in_flight.load(Ordering::Acquire), 1);

        one.target.store(1, Ordering::Release);
        assert!(!reserve_slot(&one, &state.total_in_flight));
        assert!(reserve_slot(&two, &state.total_in_flight));
    }

    #[test]
    fn server_key_ignores_path_and_normalizes_host() {
        assert_eq!(
            server_key("HTTP://Example.TEST/download/file").as_deref(),
            Some("example.test:80")
        );
        assert_eq!(
            server_key("https://[::1]/file").as_deref(),
            Some("[::1]:443")
        );
    }
}

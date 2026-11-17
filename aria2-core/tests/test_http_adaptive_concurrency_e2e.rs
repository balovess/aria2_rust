//! End-to-end coverage for the unified HTTP segmented download pipeline.

mod e2e_helpers;

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aria2_core::engine::command::Command;
use aria2_core::engine::http::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use aria2_core::util::rwlock_ext::RwLockRecover;
use bytes::Bytes;
use e2e_helpers::mock_http_server::{
    Incoming, MockHttpServer, Request, Response, StatusCode, empty_body, full_body,
};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;

#[derive(Clone)]
struct ServerStats {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    capacity_hits: Arc<AtomicUsize>,
    changed: Arc<tokio::sync::Notify>,
    release_successes: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Default for ServerStats {
    fn default() -> Self {
        let (release_successes, _) = tokio::sync::watch::channel(false);
        Self {
            active: Arc::default(),
            peak: Arc::default(),
            capacity_hits: Arc::default(),
            changed: Arc::default(),
            release_successes: Arc::new(release_successes),
        }
    }
}

struct ActiveRequestGuard {
    active: Arc<AtomicUsize>,
    changed: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Copy)]
enum SuccessGate {
    Open,
    Manual,
    CapacityRejected,
    ActivePeak(usize),
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
        self.changed.notify_waiters();
    }
}

fn update_peak(peak: &AtomicUsize, current: usize) {
    let mut observed = peak.load(Ordering::Acquire);
    while current > observed {
        match peak.compare_exchange_weak(observed, current, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(next) => observed = next,
        }
    }
}

async fn wait_for_stats(stats: &ServerStats, condition: impl Fn(&ServerStats) -> bool) {
    loop {
        let changed = stats.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if condition(stats) {
            return;
        }
        changed.await;
    }
}

fn parse_range(request: &Request<Incoming>, total: usize) -> Option<(usize, usize)> {
    let value = request.headers().get("Range")?.to_str().ok()?;
    let range = value.strip_prefix("bytes=")?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<usize>().ok()?;
    let end = if end.is_empty() {
        total.checked_sub(1)?
    } else {
        end.parse::<usize>().ok()?.min(total.saturating_sub(1))
    };
    (start <= end && start < total).then_some((start, end))
}

fn register_range_server(
    server: &MockHttpServer,
    path: &str,
    data: Arc<Vec<u8>>,
    stats: ServerStats,
    capacity: usize,
    success_gate: SuccessGate,
) {
    let path = path.to_string();
    server.on_get(&path, move |request| {
        let Some((start, end)) = parse_range(request, data.len()) else {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes=*/{}", data.len()))
                .body(empty_body())
                .unwrap();
        };

        let current = stats.active.fetch_add(1, Ordering::AcqRel) + 1;
        stats.changed.notify_waiters();
        update_peak(&stats.peak, current);
        if current > capacity {
            stats.active.fetch_sub(1, Ordering::AcqRel);
            stats.capacity_hits.fetch_add(1, Ordering::Relaxed);
            stats.changed.notify_waiters();
            if matches!(success_gate, SuccessGate::CapacityRejected) {
                stats.release_successes.send_replace(true);
            }
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header("Content-Length", 0)
                .body(empty_body())
                .unwrap();
        }
        if matches!(success_gate, SuccessGate::ActivePeak(peak) if current >= peak) {
            stats.release_successes.send_replace(true);
        }

        let guard = ActiveRequestGuard {
            active: Arc::clone(&stats.active),
            changed: Arc::clone(&stats.changed),
        };
        let body = Bytes::copy_from_slice(&data[start..=end]);
        let mut release_successes = stats.release_successes.subscribe();
        let stream = futures::stream::once(async move {
            if !matches!(success_gate, SuccessGate::Open) {
                while !*release_successes.borrow_and_update() {
                    release_successes
                        .changed()
                        .await
                        .expect("range response release sender remains alive");
                }
            }
            let frame = Frame::data(body);
            drop(guard);
            Ok::<_, Infallible>(frame)
        });

        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("Accept-Ranges", "bytes")
            .header(
                "Content-Range",
                format!("bytes={start}-{end}/{}", data.len()),
            )
            .header("Content-Length", end - start + 1)
            .body(StreamBody::new(stream).boxed())
            .unwrap()
    });
}

fn options(split: u64, max_connection_per_server: u16) -> DownloadOptions {
    DownloadOptions {
        split: Some(split),
        max_connection_per_server: Some(max_connection_per_server),
        http_adaptive: aria2_core::request::request_group::HttpAdaptiveOptions {
            enabled: true,
            start_interval_ms: 0,
            backoff_base_secs: 1,
            backoff_max_secs: 1,
            capacity_start_interval_ms: 1,
            capacity_start_interval_max_ms: 1,
            ..Default::default()
        },
        file_allocation: Some("none".into()),
        max_retries: 5,
        retry_wait: 0,
        ..DownloadOptions::default()
    }
}

fn data() -> Arc<Vec<u8>> {
    Arc::new((0..(8 * 1024 * 1024)).map(|index| index as u8).collect())
}

fn set_test_min_split_size(command: &mut DownloadCommand) {
    command.group_mut().set_option_snapshot(HashMap::from([(
        "min-split-size".to_string(),
        serde_json::json!("1M"),
    )]));
}

async fn execute_download(
    url: &str,
    output_dir: &std::path::Path,
    output_name: &str,
    mut options: DownloadOptions,
    total_length: u64,
) -> Result<(), aria2_core::error::Aria2Error> {
    options.dir = Some(output_dir.to_string_lossy().into_owned());
    options.out = Some(output_name.to_string());
    let mut command = DownloadCommand::new(
        GroupId::new(rand::random::<u64>()),
        url,
        &options,
        options.dir.as_deref(),
        options.out.as_deref(),
    )?;
    set_test_min_split_size(&mut command);
    command.group_mut().set_total_length(total_length);
    command.execute().await
}

async fn execute_multi_mirror_download(
    urls: &[String],
    output_dir: &std::path::Path,
    output_name: &str,
    mut options: DownloadOptions,
    total_length: u64,
) -> Result<(), aria2_core::error::Aria2Error> {
    options.dir = Some(output_dir.to_string_lossy().into_owned());
    options.out = Some(output_name.to_string());
    let group = std::sync::Arc::new(std::sync::RwLock::new(
        aria2_core::request::request_group::RequestGroup::new(
            GroupId::new(rand::random::<u64>()),
            urls.to_vec(),
            options.clone(),
        ),
    ));
    group.recover().set_total_length(total_length);
    let first_url = urls.first().expect("multi-mirror download needs one URL");
    let mut command = DownloadCommand::new_with_group(
        group,
        first_url,
        &options,
        options.dir.as_deref(),
        options.out.as_deref(),
    )?;
    set_test_min_split_size(&mut command);
    command.execute().await
}

#[tokio::test]
async fn split_budget_limits_total_http_range_requests() {
    let server = MockHttpServer::start().await.unwrap();
    let body = data();
    let stats = ServerStats::default();
    register_range_server(
        &server,
        "/split-budget",
        Arc::clone(&body),
        stats.clone(),
        16,
        SuccessGate::Open,
    );

    let output_dir = tempfile::tempdir().unwrap();
    execute_download(
        &format!("{}/split-budget", server.base_url()),
        output_dir.path(),
        "split-budget.bin",
        options(8, 16),
        body.len() as u64,
    )
    .await
    .unwrap();

    assert!(stats.peak.load(Ordering::Acquire) <= 8);
    assert_eq!(stats.capacity_hits.load(Ordering::Acquire), 0);
    assert_eq!(
        std::fs::read(output_dir.path().join("split-budget.bin")).unwrap(),
        *body
    );
    server.shutdown().await;
}

#[tokio::test]
async fn adaptive_concurrency_reduces_after_429_and_finishes() {
    let server = MockHttpServer::start().await.unwrap();
    let body = data();
    let stats = ServerStats::default();
    register_range_server(
        &server,
        "/adaptive",
        Arc::clone(&body),
        stats.clone(),
        2,
        SuccessGate::CapacityRejected,
    );

    let output_dir = tempfile::tempdir().unwrap();
    execute_download(
        &format!("{}/adaptive", server.base_url()),
        output_dir.path(),
        "adaptive.bin",
        options(8, 4),
        body.len() as u64,
    )
    .await
    .unwrap();

    assert!(stats.capacity_hits.load(Ordering::Acquire) > 0);
    assert!(stats.peak.load(Ordering::Acquire) <= 4);
    assert_eq!(
        std::fs::read(output_dir.path().join("adaptive.bin")).unwrap(),
        *body
    );
    server.shutdown().await;
}

#[tokio::test]
async fn adaptive_cooldown_is_interruptible_when_paused() {
    let server = MockHttpServer::start().await.unwrap();
    let body = data();
    let stats = ServerStats::default();
    register_range_server(
        &server,
        "/adaptive-pause",
        Arc::clone(&body),
        stats.clone(),
        1,
        SuccessGate::CapacityRejected,
    );

    let output_dir = tempfile::tempdir().unwrap();
    let url = format!("{}/adaptive-pause", server.base_url());
    let mut options = options(8, 4);
    options.retry_wait = 5;
    options.dir = Some(output_dir.path().to_string_lossy().into_owned());
    options.out = Some("adaptive-pause.bin".to_string());
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(9001),
        vec![url.clone()],
        options.clone(),
    )));
    group.recover().set_total_length(body.len() as u64);
    let mut command = DownloadCommand::new_with_group(
        Arc::clone(&group),
        &url,
        &options,
        options.dir.as_deref(),
        options.out.as_deref(),
    )
    .unwrap();
    set_test_min_split_size(&mut command);

    let command_task = tokio::spawn(async move { command.execute().await });
    tokio::task::yield_now().await;
    tokio::time::timeout(
        Duration::from_secs(30),
        wait_for_stats(&stats, |stats| {
            stats.capacity_hits.load(Ordering::Acquire) > 0
                && stats.active.load(Ordering::Acquire) == 0
        }),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "adaptive cooldown test did not observe a drained 429 round: hits={}, active={}",
            stats.capacity_hits.load(Ordering::Acquire),
            stats.active.load(Ordering::Acquire)
        )
    });

    group.recover_mut().pause().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), command_task)
        .await
        .expect("pause must interrupt adaptive HTTP cooldown")
        .expect("adaptive cooldown pause task panicked");
    assert!(
        matches!(
            &result,
            Err(aria2_core::error::Aria2Error::DownloadFailed(message))
                if message == "Download paused"
        ),
        "unexpected adaptive cooldown pause result: {result:?}"
    );
    assert!(group.read().unwrap().status().is_paused());
    server.shutdown().await;
}

#[tokio::test]
async fn adaptive_cooldown_is_interruptible_when_removed() {
    let server = MockHttpServer::start().await.unwrap();
    let body = data();
    let stats = ServerStats::default();
    register_range_server(
        &server,
        "/adaptive-remove",
        Arc::clone(&body),
        stats.clone(),
        1,
        SuccessGate::CapacityRejected,
    );

    let output_dir = tempfile::tempdir().unwrap();
    let url = format!("{}/adaptive-remove", server.base_url());
    let mut options = options(8, 4);
    options.retry_wait = 5;
    options.dir = Some(output_dir.path().to_string_lossy().into_owned());
    options.out = Some("adaptive-remove.bin".to_string());
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(9002),
        vec![url.clone()],
        options.clone(),
    )));
    group.recover().set_total_length(body.len() as u64);
    let mut command = DownloadCommand::new_with_group(
        Arc::clone(&group),
        &url,
        &options,
        options.dir.as_deref(),
        options.out.as_deref(),
    )
    .unwrap();
    set_test_min_split_size(&mut command);

    let command_task = tokio::spawn(async move { command.execute().await });
    tokio::task::yield_now().await;
    tokio::time::timeout(
        Duration::from_secs(30),
        wait_for_stats(&stats, |stats| {
            stats.capacity_hits.load(Ordering::Acquire) > 0
                && stats.active.load(Ordering::Acquire) == 0
        }),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "adaptive cooldown test did not observe a drained 429 round: hits={}, active={}",
            stats.capacity_hits.load(Ordering::Acquire),
            stats.active.load(Ordering::Acquire)
        )
    });

    group.recover_mut().mark_removed();
    let result = tokio::time::timeout(Duration::from_secs(1), command_task)
        .await
        .expect("remove must interrupt adaptive HTTP cooldown")
        .expect("adaptive cooldown remove task panicked");
    assert!(
        matches!(
            &result,
            Err(aria2_core::error::Aria2Error::DownloadFailed(message))
                if message == "Download cancelled by user"
        ),
        "unexpected adaptive cooldown remove result: {result:?}"
    );
    assert!(group.read().unwrap().is_removed());
    server.shutdown().await;
}

#[tokio::test]
async fn mirrors_have_independent_adaptive_limits() {
    let limited_server = MockHttpServer::start().await.unwrap();
    let healthy_server = MockHttpServer::start().await.unwrap();
    let body = data();
    let limited_stats = ServerStats::default();
    let healthy_stats = ServerStats::default();

    register_range_server(
        &limited_server,
        "/multi-mirror",
        Arc::clone(&body),
        limited_stats.clone(),
        1,
        SuccessGate::CapacityRejected,
    );
    register_range_server(
        &healthy_server,
        "/multi-mirror",
        Arc::clone(&body),
        healthy_stats.clone(),
        16,
        SuccessGate::ActivePeak(2),
    );

    let output_dir = tempfile::tempdir().unwrap();
    execute_multi_mirror_download(
        &[
            format!("{}/multi-mirror", limited_server.base_url()),
            format!("{}/multi-mirror", healthy_server.base_url()),
        ],
        output_dir.path(),
        "multi-mirror.bin",
        options(8, 4),
        body.len() as u64,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read(output_dir.path().join("multi-mirror.bin")).unwrap(),
        *body
    );
    assert!(limited_stats.capacity_hits.load(Ordering::Acquire) > 0);
    assert_eq!(healthy_stats.capacity_hits.load(Ordering::Acquire), 0);
    assert!(healthy_stats.peak.load(Ordering::Acquire) > 1);
    assert!(limited_stats.peak.load(Ordering::Acquire) <= 4);
    assert!(healthy_stats.peak.load(Ordering::Acquire) <= 4);

    limited_server.shutdown().await;
    healthy_server.shutdown().await;
}

#[tokio::test]
async fn mirrors_on_one_authority_share_the_server_limit() {
    let server = MockHttpServer::start().await.unwrap();
    let body = data();
    let stats = ServerStats::default();

    register_range_server(
        &server,
        "/same-authority-a",
        Arc::clone(&body),
        stats.clone(),
        2,
        SuccessGate::CapacityRejected,
    );
    register_range_server(
        &server,
        "/same-authority-b",
        Arc::clone(&body),
        stats.clone(),
        2,
        SuccessGate::CapacityRejected,
    );

    let output_dir = tempfile::tempdir().unwrap();
    execute_multi_mirror_download(
        &[
            format!("{}/same-authority-a", server.base_url()),
            format!("{}/same-authority-b", server.base_url()),
        ],
        output_dir.path(),
        "same-authority.bin",
        options(8, 4),
        body.len() as u64,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read(output_dir.path().join("same-authority.bin")).unwrap(),
        *body
    );
    assert!(stats.capacity_hits.load(Ordering::Acquire) > 0);
    assert!(stats.peak.load(Ordering::Acquire) <= 4);

    server.shutdown().await;
}

#[tokio::test]
async fn cancellation_releases_http_requests() {
    let server = MockHttpServer::start().await.unwrap();
    let body = data();
    let stats = ServerStats::default();
    register_range_server(
        &server,
        "/cancel",
        Arc::clone(&body),
        stats.clone(),
        16,
        SuccessGate::Manual,
    );

    let output_dir = tempfile::tempdir().unwrap();
    let mut options = options(8, 4);
    options.dir = Some(output_dir.path().to_string_lossy().into_owned());
    options.out = Some("cancel.bin".into());
    let mut command = DownloadCommand::new(
        GroupId::new(rand::random::<u64>()),
        &format!("{}/cancel", server.base_url()),
        &options,
        options.dir.as_deref(),
        options.out.as_deref(),
    )
    .unwrap();
    set_test_min_split_size(&mut command);
    command.group_mut().set_total_length(body.len() as u64);
    let group = command.request_group().unwrap();
    let task = tokio::spawn(async move { command.execute().await });
    tokio::task::yield_now().await;

    tokio::time::timeout(
        Duration::from_secs(30),
        wait_for_stats(&stats, |stats| stats.active.load(Ordering::Acquire) > 0),
    )
    .await
    .unwrap();

    group.recover_mut().remove().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());

    tokio::time::timeout(
        Duration::from_secs(30),
        wait_for_stats(&stats, |stats| stats.active.load(Ordering::Acquire) == 0),
    )
    .await
    .expect("cancelled HTTP response body must release its active request");
    assert_eq!(stats.active.load(Ordering::Acquire), 0);
    server.shutdown().await;
}

/// The first `count` GET requests receive 429.
#[derive(Clone, Copy)]
struct RequestLimit(usize);

/// Serves `data` (200 without Range, 206 with one) and rejects a fixed number
/// of requests with 429. Records response statuses for deterministic checks.
fn register_rate_limited_server(
    server: &MockHttpServer,
    path: &str,
    data: Arc<Vec<u8>>,
    limit: RequestLimit,
    retry_after: Option<&'static str>,
) -> Arc<std::sync::Mutex<Vec<u16>>> {
    let log = Arc::new(std::sync::Mutex::new(Vec::<u16>::new()));
    let handler_log = Arc::clone(&log);
    server.on_get(path, move |request| {
        let mut log = handler_log.lock().unwrap();
        let limited = log.len() < limit.0;
        if limited {
            log.push(429);
            let page = "<html><body>429 Too Many Requests</body></html>";
            let mut response = Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header("Content-Type", "text/html")
                .header("Content-Length", page.len());
            if let Some(retry_after) = retry_after {
                response = response.header("Retry-After", retry_after);
            }
            return response.body(full_body(page)).unwrap();
        }

        let Some((start, end)) = parse_range(request, data.len()) else {
            log.push(200);
            return Response::builder()
                .status(StatusCode::OK)
                .header("Accept-Ranges", "bytes")
                .header("Content-Length", data.len())
                .body(full_body(data.as_ref().clone()))
                .unwrap();
        };
        log.push(206);
        let body = Bytes::copy_from_slice(&data[start..=end]);
        let stream = futures::stream::once(async move { Ok::<_, Infallible>(Frame::data(body)) });
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("Accept-Ranges", "bytes")
            .header(
                "Content-Range",
                format!("bytes {start}-{end}/{}", data.len()),
            )
            .header("Content-Length", end - start + 1)
            .body(StreamBody::new(stream).boxed())
            .unwrap()
    });
    log
}

fn rejections(log: &std::sync::Mutex<Vec<u16>>) -> usize {
    log.lock()
        .unwrap()
        .iter()
        .filter(|status| **status == 429)
        .count()
}

#[tokio::test]
async fn rate_limited_ranges_back_off_without_spending_retries() {
    let server = MockHttpServer::start().await.unwrap();
    let body = data();
    // A fixed set of capacity refusals exercises retries without depending on
    // request timing or server response delays.
    let log = register_rate_limited_server(
        &server,
        "/rate-limited",
        Arc::clone(&body),
        RequestLimit(4),
        None,
    );

    let output_dir = tempfile::tempdir().unwrap();
    let mut options = options(8, 4);
    options.max_retries = 2;
    execute_download(
        &format!("{}/rate-limited", server.base_url()),
        output_dir.path(),
        "rate-limited.bin",
        options,
        body.len() as u64,
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "a rate limit must slow the download, not fail it: {error}; requests: {:?}",
            log.lock().unwrap()
        )
    });

    assert!(
        rejections(&log) > 2,
        "the server must have refused more requests than the retry budget"
    );
    assert_eq!(
        std::fs::read(output_dir.path().join("rate-limited.bin")).unwrap(),
        *body
    );
    server.shutdown().await;
}

#[tokio::test]
async fn disabled_adaptive_mode_reconnects_after_capacity_rejection() {
    let server = MockHttpServer::start().await.unwrap();
    let body = data();
    let log = register_rate_limited_server(
        &server,
        "/disabled-reconnect",
        Arc::clone(&body),
        RequestLimit(4),
        None,
    );

    let output_dir = tempfile::tempdir().unwrap();
    let mut options = options(8, 4);
    options.http_adaptive.enabled = false;
    options.retry_wait = 0;
    tokio::time::timeout(
        Duration::from_secs(3),
        execute_download(
            &format!("{}/disabled-reconnect", server.base_url()),
            output_dir.path(),
            "disabled-reconnect.bin",
            options,
            body.len() as u64,
        ),
    )
    .await
    .expect("disabled adaptive mode must reconnect instead of waiting for adaptive cooldown")
    .expect("disabled adaptive mode must finish after a temporary rate limit");

    assert!(rejections(&log) >= 4);
    assert_eq!(
        std::fs::read(output_dir.path().join("disabled-reconnect.bin")).unwrap(),
        *body
    );
    server.shutdown().await;
}

#[tokio::test]
async fn retry_after_pauses_new_ranges() {
    let server = MockHttpServer::start().await.unwrap();
    let body = data();
    // Two requests are refused with Retry-After before the server recovers.
    let log = register_rate_limited_server(
        &server,
        "/retry-after",
        Arc::clone(&body),
        RequestLimit(2),
        Some("1"),
    );

    let output_dir = tempfile::tempdir().unwrap();
    execute_download(
        &format!("{}/retry-after", server.base_url()),
        output_dir.path(),
        "retry-after.bin",
        options(8, 1),
        body.len() as u64,
    )
    .await
    .expect("a server asking for a pause must not fail the download");

    let requests = log.lock().unwrap().clone();
    assert!(requests.contains(&429));
    assert_eq!(
        std::fs::read(output_dir.path().join("retry-after.bin")).unwrap(),
        *body
    );
    server.shutdown().await;
}

#[tokio::test]
async fn rate_limited_range_probe_backs_off_without_spending_retries() {
    let server = MockHttpServer::start().await.unwrap();
    let body = data();
    // The first two requests, the range probe and its retry, are refused.
    let log = register_rate_limited_server(
        &server,
        "/probe-rate-limited",
        Arc::clone(&body),
        RequestLimit(2),
        None,
    );

    let output_dir = tempfile::tempdir().unwrap();
    let mut options = options(8, 4);
    options.max_retries = 1;
    options.http_adaptive.backoff_base_secs = 1;
    options.http_adaptive.backoff_max_secs = 1;
    execute_download(
        &format!("{}/probe-rate-limited", server.base_url()),
        output_dir.path(),
        "probe-rate-limited.bin",
        options,
        body.len() as u64,
    )
    .await
    .expect("a rate-limited range probe must be waited out");

    assert_eq!(rejections(&log), 2);
    assert_eq!(
        std::fs::read(output_dir.path().join("probe-rate-limited.bin")).unwrap(),
        *body
    );
    server.shutdown().await;
}

#[tokio::test]
async fn rate_limited_first_response_does_not_size_the_file() {
    let server = MockHttpServer::start().await.unwrap();
    let body = data();
    // The first GET, which names and sizes an unknown download, gets a
    // 429 page; its Content-Length is the page's, not the file's.
    let log = register_rate_limited_server(
        &server,
        "/first-rate-limited.bin",
        Arc::clone(&body),
        RequestLimit(1),
        None,
    );

    let output_dir = tempfile::tempdir().unwrap();
    let mut options = options(8, 4);
    options.dir = Some(output_dir.path().to_string_lossy().into_owned());
    let url = format!("{}/first-rate-limited.bin", server.base_url());
    let mut command = DownloadCommand::new(
        GroupId::new(rand::random::<u64>()),
        &url,
        &options,
        options.dir.as_deref(),
        None,
    )
    .unwrap();
    set_test_min_split_size(&mut command);
    let result = command.execute().await;

    assert_eq!(rejections(&log), 1);
    let output = output_dir.path().join("first-rate-limited.bin");
    let written = std::fs::read(&output).unwrap_or_default();
    assert!(
        result.is_err() || written == *body,
        "a download that succeeds must hold the whole file, not {} bytes",
        written.len()
    );
    assert_ne!(
        command.group().total_length(),
        "<html><body>429 Too Many Requests</body></html>".len() as u64,
        "the 429 page's length was taken as the file's"
    );
    server.shutdown().await;
}

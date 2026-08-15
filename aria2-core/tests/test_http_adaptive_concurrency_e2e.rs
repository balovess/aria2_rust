//! End-to-end coverage for the unified HTTP segmented download pipeline.

mod e2e_helpers;

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::util::rwlock_ext::RwLockRecover;
use bytes::Bytes;
use e2e_helpers::mock_http_server::{
    Incoming, MockHttpServer, Request, Response, StatusCode, empty_body,
};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;

#[derive(Clone, Default)]
struct ServerStats {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    capacity_hits: Arc<AtomicUsize>,
}

struct ActiveRequestGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
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
    delay: Duration,
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
        update_peak(&stats.peak, current);
        if current > capacity {
            stats.active.fetch_sub(1, Ordering::AcqRel);
            stats.capacity_hits.fetch_add(1, Ordering::Relaxed);
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header("Content-Length", 0)
                .body(empty_body())
                .unwrap();
        }

        let guard = ActiveRequestGuard {
            active: Arc::clone(&stats.active),
        };
        let body = Bytes::copy_from_slice(&data[start..=end]);
        let stream = futures::stream::once(async move {
            tokio::time::sleep(delay).await;
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

fn options(split: u16, max_connection_per_server: u16) -> DownloadOptions {
    DownloadOptions {
        split: Some(split),
        max_connection_per_server: Some(max_connection_per_server),
        file_allocation: Some("none".into()),
        max_retries: 5,
        retry_wait: 0,
        ..DownloadOptions::default()
    }
}

fn data() -> Arc<Vec<u8>> {
    Arc::new((0..(2 * 1024 * 1024)).map(|index| index as u8).collect())
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
        Duration::from_millis(30),
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
        Duration::from_millis(30),
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
        Duration::from_millis(30),
    );
    register_range_server(
        &healthy_server,
        "/multi-mirror",
        Arc::clone(&body),
        healthy_stats.clone(),
        16,
        Duration::from_millis(30),
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
        Duration::from_millis(30),
    );
    register_range_server(
        &server,
        "/same-authority-b",
        Arc::clone(&body),
        stats.clone(),
        2,
        Duration::from_millis(30),
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
        Duration::from_secs(5),
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
    command.group_mut().set_total_length(body.len() as u64);
    let group = command.request_group().unwrap();
    let task = tokio::spawn(async move { command.execute().await });

    tokio::time::timeout(Duration::from_secs(2), async {
        while stats.active.load(Ordering::Acquire) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    group.recover_mut().remove().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(stats.active.load(Ordering::Acquire), 0);
    server.shutdown().await;
}

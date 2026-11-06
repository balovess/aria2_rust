//! End-to-end recovery coverage for HTTP adaptive concurrency.

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
    Incoming, MockHttpServer, Request, Response, StatusCode, empty_body,
};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;

#[derive(Clone, Default)]
struct RecoveryStats {
    active: Arc<AtomicUsize>,
    limited_peak: Arc<AtomicUsize>,
    recovered_peak: Arc<AtomicUsize>,
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

#[derive(Clone, Default)]
struct JitterStats {
    active: Arc<AtomicUsize>,
    accepted_peak: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    successes: Arc<AtomicUsize>,
    capacity_rejections: Arc<AtomicUsize>,
    proxy_errors: Arc<AtomicUsize>,
    capacity_rejections_after_final_recovery: Arc<AtomicUsize>,
}

const FINAL_RECOVERY_SUCCESSES: usize = 14;

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

fn register_recovering_range_server(
    server: &MockHttpServer,
    path: &str,
    data: Arc<Vec<u8>>,
    stats: RecoveryStats,
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

        // Three rounds of 4, 3, then 2 requests observe the temporary limit
        // before it recovers, forcing the controller through 4 -> 3 -> 2 -> 1.
        let limited = stats.capacity_hits.load(Ordering::Acquire) < 6;
        let capacity = if limited { 1 } else { 4 };
        let current = stats.active.fetch_add(1, Ordering::AcqRel) + 1;
        if current > capacity {
            stats.active.fetch_sub(1, Ordering::AcqRel);
            stats.capacity_hits.fetch_add(1, Ordering::Relaxed);
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header("Content-Length", 0)
                .header("Retry-After", "1")
                .body(empty_body())
                .unwrap();
        }
        if limited {
            update_peak(&stats.limited_peak, current);
        } else {
            update_peak(&stats.recovered_peak, current);
        }

        let guard = ActiveRequestGuard {
            active: Arc::clone(&stats.active),
        };
        let body = Bytes::copy_from_slice(&data[start..=end]);
        let stream = futures::stream::once(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
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

fn register_jitter_range_server(
    server: &MockHttpServer,
    path: &str,
    data: Arc<Vec<u8>>,
    stats: JitterStats,
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

        let request_index = stats.requests.fetch_add(1, Ordering::AcqRel);
        let successes = stats.successes.load(Ordering::Acquire);
        let capacity = if successes < 6 {
            1
        } else if successes < 10 {
            4
        } else if successes < FINAL_RECOVERY_SUCCESSES {
            2
        } else {
            4
        };
        let current = stats.active.fetch_add(1, Ordering::AcqRel) + 1;
        if current > capacity {
            stats.active.fetch_sub(1, Ordering::AcqRel);
            stats.capacity_rejections.fetch_add(1, Ordering::Relaxed);
            if successes >= FINAL_RECOVERY_SUCCESSES {
                stats
                    .capacity_rejections_after_final_recovery
                    .fetch_add(1, Ordering::Relaxed);
            }
            let status = if request_index.is_multiple_of(2) {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::TOO_MANY_REQUESTS
            };
            return Response::builder()
                .status(status)
                .header("Content-Length", 0)
                .body(empty_body())
                .unwrap();
        }
        if successes > 0 && matches!(request_index, 8 | 20) {
            stats.active.fetch_sub(1, Ordering::AcqRel);
            stats.proxy_errors.fetch_add(1, Ordering::Relaxed);
            return Response::builder()
                .status(StatusCode::GATEWAY_TIMEOUT)
                .header("Content-Length", 0)
                .body(empty_body())
                .unwrap();
        }
        update_peak(&stats.accepted_peak, current);

        let guard = ActiveRequestGuard {
            active: Arc::clone(&stats.active),
        };
        let body = Bytes::copy_from_slice(&data[start..=end]);
        let successes = Arc::clone(&stats.successes);
        let stream = futures::stream::once(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            successes.fetch_add(1, Ordering::AcqRel);
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
        file_allocation: Some("none".into()),
        max_retries: 12,
        retry_wait: 0,
        ..DownloadOptions::default()
    }
}

fn data() -> Arc<Vec<u8>> {
    Arc::new((0..(8 * 1024 * 1024)).map(|index| index as u8).collect())
}

fn large_data() -> Arc<Vec<u8>> {
    Arc::new((0..(32 * 1024 * 1024)).map(|index| index as u8).collect())
}

fn set_test_min_split_size(command: &mut DownloadCommand, size: &str) {
    command.group_mut().set_option_snapshot(HashMap::from([(
        "min-split-size".to_string(),
        serde_json::json!(size),
    )]));
}

async fn execute_multi_mirror_download(
    urls: &[String],
    output_dir: &std::path::Path,
    output_name: &str,
    mut options: DownloadOptions,
    total_length: u64,
    min_split_size: &str,
) -> Result<(), aria2_core::error::Aria2Error> {
    options.dir = Some(output_dir.to_string_lossy().into_owned());
    options.out = Some(output_name.to_string());
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(rand::random::<u64>()),
        urls.to_vec(),
        options.clone(),
    )));
    group.recover().set_total_length(total_length);
    let first_url = urls.first().expect("multi-mirror download needs one URL");
    let mut command = DownloadCommand::new_with_group(
        group,
        first_url,
        &options,
        options.dir.as_deref(),
        options.out.as_deref(),
    )?;
    set_test_min_split_size(&mut command, min_split_size);
    command.execute().await
}

#[tokio::test]
async fn multi_mirror_target_recovers_after_temporary_capacity_limit() {
    let server = MockHttpServer::start().await.unwrap();
    let body = data();
    let stats = RecoveryStats::default();
    register_recovering_range_server(&server, "/recovery-a", Arc::clone(&body), stats.clone());
    register_recovering_range_server(&server, "/recovery-b", Arc::clone(&body), stats.clone());

    let output_dir = tempfile::tempdir().unwrap();
    execute_multi_mirror_download(
        &[
            format!("{}/recovery-a", server.base_url()),
            format!("{}/recovery-b", server.base_url()),
        ],
        output_dir.path(),
        "adaptive-recovery.bin",
        options(8, 4),
        body.len() as u64,
        "1M",
    )
    .await
    .unwrap();

    assert!(stats.capacity_hits.load(Ordering::Acquire) > 0);
    assert_eq!(
        stats.limited_peak.load(Ordering::Acquire),
        1,
        "the temporary server limit must reduce accepted concurrency to one"
    );
    assert!(
        stats.recovered_peak.load(Ordering::Acquire) > 1,
        "adaptive concurrency must probe upward after recovery"
    );
    assert_eq!(
        std::fs::read(output_dir.path().join("adaptive-recovery.bin")).unwrap(),
        *body
    );
    server.shutdown().await;
}

async fn run_adaptive_soak(iteration: usize) {
    let server = MockHttpServer::start().await.unwrap();
    let body = large_data();
    let stats = JitterStats::default();
    let path_a = format!("/soak-{iteration}-a");
    let path_b = format!("/soak-{iteration}-b");
    register_jitter_range_server(&server, &path_a, Arc::clone(&body), stats.clone());
    register_jitter_range_server(&server, &path_b, Arc::clone(&body), stats.clone());

    let output_dir = tempfile::tempdir().unwrap();
    let output_name = format!("adaptive-soak-{iteration}.bin");
    execute_multi_mirror_download(
        &[
            format!("{}{path_a}", server.base_url()),
            format!("{}{path_b}", server.base_url()),
        ],
        output_dir.path(),
        &output_name,
        options(16, 4),
        body.len() as u64,
        "2M",
    )
    .await
    .unwrap();

    assert!(
        stats.capacity_rejections.load(Ordering::Acquire) > 0,
        "the soak must exercise capacity downshifts"
    );
    assert!(
        stats.proxy_errors.load(Ordering::Acquire) > 0,
        "the soak must exercise non-capacity request failures"
    );
    assert!(
        stats.accepted_peak.load(Ordering::Acquire) > 1,
        "the soak must recover from capacity downshifts"
    );
    assert_eq!(
        stats
            .capacity_rejections_after_final_recovery
            .load(Ordering::Acquire),
        0,
        "a fully recovered server must not receive self-inflicted capacity rejections"
    );
    assert_eq!(
        std::fs::read(output_dir.path().join(output_name)).unwrap(),
        *body
    );
    server.shutdown().await;
}

#[tokio::test]
async fn adaptive_soak_survives_capacity_changes_and_proxy_jitter() {
    run_adaptive_soak(0).await;
}

#[tokio::test]
#[ignore = "extended soak; run explicitly with --ignored"]
async fn adaptive_extended_soak_repeats_capacity_and_proxy_jitter() {
    tokio::time::timeout(Duration::from_secs(55), async {
        for iteration in 0..8 {
            run_adaptive_soak(iteration).await;
        }
    })
    .await
    .expect("extended adaptive soak exceeded 55 seconds");
}

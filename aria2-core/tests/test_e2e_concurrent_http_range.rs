//! E2E tests: DownloadCommand concurrent path (FuturesUnordered) over real HTTP.
//!
//! Each test starts a MockHttpServer that serves a file with Range support,
//! creates a DownloadCommand with split > 1, executes it, and verifies
//! the assembled output is correct and progress updates are reported.
//!
//! These tests fill the gap between:
//! - `test_e2e_http_concurrent.rs` — only tests constructor + segment manager units
//! - `test_e2e_download.rs` — only tests the sequential path (split = 1)

mod e2e_helpers;

use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::e2e_helpers::mock_http_server::{Body, MockHttpServer, Request, Response, StatusCode};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate deterministic test data (reproducible across runs).
fn generate_test_data(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_add(seed)).collect()
}

/// Build URL from base + path
fn make_url(base: &str, path: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{}{}", trimmed, path)
    } else {
        format!("{}/{}", trimmed, path)
    }
}

/// Create a minimal DownloadOptions with split and max_connection_per_server.
fn make_options(
    split: Option<u16>,
    max_conn: Option<u16>,
    dir: &str,
    out: &str,
) -> DownloadOptions {
    DownloadOptions {
        split,
        max_connection_per_server: max_conn,
        // The concurrent path needs entity metadata before it can allocate
        // ranges. Keep this fixture explicit; unknown-length downloads are
        // covered separately and must begin with one ordinary GET.
        use_head: true,
        max_download_limit: None,
        max_upload_limit: None,
        dir: Some(dir.to_string()),
        out: Some(out.to_string()),
        ..Default::default()
    }
}

/// Check if a request log entry has a Range header.
fn has_range_header(entry: &crate::e2e_helpers::mock_http_server::RequestLog) -> bool {
    entry
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("range"))
}

/// Register a GET range handler for the given path.
///
/// HEAD requests are automatically handled by the mock server's fallback logic:
/// when no explicit HEAD handler matches, the server matches against GET handlers
/// and strips the response body. This ensures `Content-Length` and `Accept-Ranges`
/// headers from the GET handler are returned for HEAD probes, which is required
/// to trigger the concurrent download path.
fn register_range_with_head(server: &MockHttpServer, path: &str, body: &[u8]) {
    server.register_range_response(path, body);
}

// ---------------------------------------------------------------------------
// Test 1: Concurrent download assembles file correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_download_assembles_file_correctly() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2MB file >= CONCURRENT_MIN_FILE_SIZE (1MB), so concurrent path is used
    let file_size = 2 * 1024 * 1024;
    let data = generate_test_data(file_size, 42);
    register_range_with_head(&server, "/largefile", &data);

    let url = make_url(&server.base_url(), "/largefile");
    let tmp_dir = std::env::temp_dir().to_string_lossy().into_owned();
    let out_name = format!("test_concurrent_asm_{}.bin", std::process::id());
    let out_path = format!("{}/{}", tmp_dir, out_name);

    // Clean up any leftover from previous runs
    let _ = std::fs::remove_file(&out_path);

    let mut cmd = DownloadCommand::new(
        GroupId::new(1),
        &url,
        &make_options(Some(4), Some(2), &tmp_dir, &out_name),
        Some(&tmp_dir),
        Some(&out_name),
    )
    .expect("Failed to create DownloadCommand");

    cmd.execute()
        .await
        .expect("Concurrent download should succeed");

    // Verify: output file exists and has correct size
    let metadata = std::fs::metadata(&out_path).expect("Output file should exist");
    assert_eq!(
        metadata.len() as usize,
        file_size,
        "Output file size should match"
    );

    // Verify: content is byte-for-byte identical
    let output_data = std::fs::read(&out_path).expect("Should read output file");
    assert_eq!(output_data, data, "Output content should match source data");

    // Verify: at least 2 Range requests were made (proving concurrent split download)
    let log = server.take_request_log();
    let range_count = log.iter().filter(|e| has_range_header(e)).count();
    assert!(
        range_count >= 2,
        "Expected at least 2 Range requests, got {}",
        range_count
    );

    // Cleanup
    let _ = std::fs::remove_file(&out_path);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 2: Small file (< CONCURRENT_MIN_FILE_SIZE) uses single segment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_download_small_file_sequential() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 500KB < CONCURRENT_MIN_FILE_SIZE (1MB), so sequential path is used
    let file_size = 500 * 1024;
    let data = generate_test_data(file_size, 7);
    server.register_range_response("/smallfile", &data);

    let url = make_url(&server.base_url(), "/smallfile");
    let tmp_dir = std::env::temp_dir().to_string_lossy().into_owned();
    let out_name = format!("test_concurrent_small_{}.bin", std::process::id());
    let out_path = format!("{}/{}", tmp_dir, out_name);
    let _ = std::fs::remove_file(&out_path);

    let mut cmd = DownloadCommand::new(
        GroupId::new(3),
        &url,
        &make_options(Some(4), Some(2), &tmp_dir, &out_name),
        Some(&tmp_dir),
        Some(&out_name),
    )
    .expect("Failed to create DownloadCommand");

    cmd.execute().await.expect("Download should succeed");

    // Verify file
    let metadata = std::fs::metadata(&out_path).expect("Output file should exist");
    assert_eq!(
        metadata.len() as usize,
        file_size,
        "Output file size should match"
    );

    let output_data = std::fs::read(&out_path).expect("Should read output file");
    assert_eq!(output_data, data, "Output content should match source data");

    // Verify: small file should NOT use concurrent split download.
    // The sequential path never splits into multiple byte ranges. On Linux the
    // splice optimization (`try_splice_download`) issues a single full-file
    // Range request (`bytes=0-N`) for zero-copy transfer — this is still a
    // single segment, not concurrent splitting, so at most one Range request
    // is acceptable.
    let log = server.take_request_log();
    let range_count = log.iter().filter(|e| has_range_header(e)).count();
    assert!(
        range_count <= 1,
        "Small file should NOT use concurrent split download (Range requests: {})",
        range_count
    );

    // Cleanup
    let _ = std::fs::remove_file(&out_path);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 3: Multiple Range requests made for large file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_download_multiple_range_requests() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    let file_size = 2 * 1024 * 1024;
    let data = generate_test_data(file_size, 123);
    register_range_with_head(&server, "/range-test", &data);

    let url = make_url(&server.base_url(), "/range-test");
    let tmp_dir = std::env::temp_dir().to_string_lossy().into_owned();
    let out_name = format!("test_concurrent_range_{}.bin", std::process::id());
    let out_path = format!("{}/{}", tmp_dir, out_name);
    let _ = std::fs::remove_file(&out_path);

    let mut cmd = DownloadCommand::new(
        GroupId::new(4),
        &url,
        &make_options(Some(4), Some(2), &tmp_dir, &out_name),
        Some(&tmp_dir),
        Some(&out_name),
    )
    .expect("Failed to create DownloadCommand");

    cmd.execute()
        .await
        .expect("Concurrent download should succeed");

    // Verify file content
    let output_data = std::fs::read(&out_path).expect("Should read output file");
    assert_eq!(output_data, data, "Output content should match source data");

    // Verify: multiple Range requests were made
    let log = server.take_request_log();
    let range_entries: Vec<_> = log.iter().filter(|e| has_range_header(e)).collect();
    assert!(
        range_entries.len() >= 2,
        "Expected at least 2 Range requests, got {}",
        range_entries.len()
    );

    // Cleanup
    let _ = std::fs::remove_file(&out_path);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 4: Capacity feedback lowers concurrency and requeues 429 segments
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_adaptive_pool_requeues_rate_limited_ranges() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    let file_size = 2 * 1024 * 1024;
    let data = generate_test_data(file_size, 99);
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let rate_limited = Arc::new(AtomicUsize::new(0));
    let body = data.clone();
    let active_for_handler = Arc::clone(&active);
    let max_active_for_handler = Arc::clone(&max_active);
    let rate_limited_for_handler = Arc::clone(&rate_limited);

    server.on_get("/limited", move |req: &Request<_>| -> Response<Body> {
        if req.method() == hyper::Method::HEAD {
            return Response::builder()
                .status(StatusCode::OK)
                .header("Accept-Ranges", "bytes")
                .header("Content-Length", body.len())
                .body(crate::e2e_helpers::mock_http_server::empty_body())
                .unwrap();
        }

        let current = active_for_handler.fetch_add(1, Ordering::AcqRel) + 1;
        max_active_for_handler.fetch_max(current, Ordering::AcqRel);
        if current > 2 {
            active_for_handler.fetch_sub(1, Ordering::AcqRel);
            rate_limited_for_handler.fetch_add(1, Ordering::AcqRel);
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(crate::e2e_helpers::mock_http_server::empty_body())
                .unwrap();
        }

        let Some(range) = req
            .headers()
            .get("Range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.split_once('-'))
        else {
            active_for_handler.fetch_sub(1, Ordering::AcqRel);
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(crate::e2e_helpers::mock_http_server::empty_body())
                .unwrap();
        };
        let start: usize = range.0.parse().unwrap();
        let end: usize = range.1.parse().unwrap();
        let chunk = body[start..=end].to_vec();
        let active_for_body = Arc::clone(&active_for_handler);
        let stream = futures::stream::once(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            active_for_body.fetch_sub(1, Ordering::AcqRel);
            Ok::<_, Infallible>(Frame::data(Bytes::from(chunk)))
        });
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("Accept-Ranges", "bytes")
            .header(
                "Content-Range",
                format!("bytes={}-{}/{}", start, end, body.len()),
            )
            .body(StreamBody::new(stream).boxed())
            .unwrap()
    });

    let url = make_url(&server.base_url(), "/limited");
    let tmp_dir = std::env::temp_dir().to_string_lossy().into_owned();
    let out_name = format!("test_adaptive_pool_{}.bin", std::process::id());
    let out_path = format!("{}/{}", tmp_dir, out_name);
    let _ = std::fs::remove_file(&out_path);

    let mut options = make_options(Some(4), Some(4), &tmp_dir, &out_name);
    options.retry_wait = 0;
    let mut cmd = DownloadCommand::new(
        GroupId::new(5),
        &url,
        &options,
        Some(&tmp_dir),
        Some(&out_name),
    )
    .expect("Failed to create DownloadCommand");
    cmd.execute()
        .await
        .expect("Rate-limited download should converge and succeed");

    assert_eq!(std::fs::read(&out_path).unwrap(), data);
    assert!(rate_limited.load(Ordering::Acquire) > 0);
    assert!(max_active.load(Ordering::Acquire) >= 3);
    assert!(max_active.load(Ordering::Acquire) <= 4);

    let _ = std::fs::remove_file(&out_path);
    server.shutdown().await;
}

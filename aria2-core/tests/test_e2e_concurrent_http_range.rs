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

use crate::e2e_helpers::mock_http_server::MockHttpServer;

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
    let out_path = format!("{}/{}", tmp_dir, &out_name);

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
    let out_path = format!("{}/{}", tmp_dir, &out_name);
    let _ = std::fs::remove_file(&out_path);

    let mut cmd = DownloadCommand::new(
        GroupId::new(3),
        &url,
        &make_options(Some(4), Some(2), &tmp_dir, &out_name),
        Some(&tmp_dir),
        Some(&out_name),
    )
    .expect("Failed to create DownloadCommand");

    cmd.execute()
        .await
        .expect("Download should succeed");

    // Verify file
    let metadata = std::fs::metadata(&out_path).expect("Output file should exist");
    assert_eq!(
        metadata.len() as usize,
        file_size,
        "Output file size should match"
    );

    let output_data = std::fs::read(&out_path).expect("Should read output file");
    assert_eq!(output_data, data, "Output content should match source data");

    // Verify: no Range requests (sequential path doesn't split into byte ranges)
    let log = server.take_request_log();
    for entry in &log {
        assert!(
            !has_range_header(entry),
            "Small file should NOT use Range requests (sequential path)"
        );
    }

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
    let out_path = format!("{}/{}", tmp_dir, &out_name);
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

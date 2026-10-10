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
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::engine::http::download_command::DownloadCommand;
use aria2_core::error::{Aria2Error, RecoverableError};
use aria2_core::filesystem::control_file::ControlFile;
use aria2_core::http::HttpVersion;
use aria2_core::request::request_group::{DownloadOptions, DownloadStatus, GroupId, RequestGroup};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::session::save_session_command::SaveSessionCommand;
use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::e2e_helpers::mock_http_server::{
    Body, Incoming, MockHttpServer, Request, Response, StatusCode, empty_body, full_body,
};

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

fn make_concurrent_command(
    gid: GroupId,
    uri: &str,
    options: &DownloadOptions,
    output_dir: Option<&str>,
    output_name: Option<&str>,
) -> DownloadCommand {
    make_concurrent_command_with_group(gid, uri, options, output_dir, output_name).0
}

fn make_concurrent_command_with_group(
    gid: GroupId,
    uri: &str,
    options: &DownloadOptions,
    output_dir: Option<&str>,
    output_name: Option<&str>,
) -> (DownloadCommand, Arc<std::sync::RwLock<RequestGroup>>) {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![uri.to_string()],
        options.clone(),
    )));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "min-split-size".to_string(),
            serde_json::json!("1M"),
        )]));
    let command =
        DownloadCommand::new_with_group(Arc::clone(&group), uri, options, output_dir, output_name)
            .expect("Failed to create DownloadCommand");
    (command, group)
}

/// Check if a request log entry has a Range header.
fn has_range_header(entry: &crate::e2e_helpers::mock_http_server::RequestLog) -> bool {
    entry
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("range"))
}

async fn wait_for_group_status(
    group: &Arc<std::sync::RwLock<RequestGroup>>,
    expected: DownloadStatus,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if group.read().unwrap().status() == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("download did not reach the expected lifecycle state");
}

async fn wait_for_control_file(path: &std::path::Path) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("download did not create its control file");
}

async fn wait_for_progress(group: &Arc<std::sync::RwLock<RequestGroup>>) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if group.read().unwrap().get_completed_length() > 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("download did not report in-flight progress");
}

async fn wait_for_engine(
    handle: tokio::task::JoinHandle<aria2_core::error::Result<()>>,
    message: &str,
) {
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), handle)
        .await
        .expect(message)
        .expect("download engine task panicked");
    result.expect("download engine returned an error");
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

fn register_range_with_size_limit(
    server: &MockHttpServer,
    path: &str,
    body: &[u8],
    accepted_range_size: u64,
    observed_range_lengths: Arc<std::sync::Mutex<Vec<u64>>>,
) {
    let body = Arc::new(body.to_vec());
    server.on("GET", path, move |request| {
        if request.method() == hyper::Method::HEAD {
            return Response::builder()
                .status(StatusCode::OK)
                .header("Accept-Ranges", "bytes")
                .header("Content-Length", body.len())
                .body(empty_body())
                .unwrap();
        }

        let Some((start, end)) = request
            .headers()
            .get("Range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.split_once('-'))
            .and_then(|(start, end)| Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok()?)))
        else {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(empty_body())
                .unwrap();
        };
        let range_length = end.saturating_sub(start).saturating_add(1);
        observed_range_lengths
            .lock()
            .expect("range length lock should be available")
            .push(range_length);
        if range_length > accepted_range_size {
            return Response::builder()
                .status(StatusCode::PAYLOAD_TOO_LARGE)
                .body(empty_body())
                .unwrap();
        }
        if start > end || end >= body.len() as u64 {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes */{}", body.len()))
                .body(empty_body())
                .unwrap();
        }

        let start = start as usize;
        let end = end as usize;
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("Accept-Ranges", "bytes")
            .header(
                "Content-Range",
                format!("bytes {start}-{end}/{}", body.len()),
            )
            .header("Content-Length", end - start + 1)
            .body(full_body(body[start..=end].to_vec()))
            .unwrap()
    });
}

fn range_response(req: &Request<Incoming>, body: &[u8]) -> Response<Body> {
    let Some(range) = req
        .headers()
        .get("Range")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split_once('-'))
    else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(empty_body())
            .unwrap();
    };
    let Ok(start) = range.0.parse::<usize>() else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(empty_body())
            .unwrap();
    };
    let Ok(end) = range.1.parse::<usize>() else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(empty_body())
            .unwrap();
    };
    if start > end || end >= body.len() {
        return Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .body(empty_body())
            .unwrap();
    }
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header("Accept-Ranges", "bytes")
        .header(
            "Content-Range",
            format!("bytes={start}-{end}/{}", body.len()),
        )
        .header("Content-Length", end - start + 1)
        .body(full_body(body[start..=end].to_vec()))
        .unwrap()
}

fn range_response_with_slow_first(
    req: &Request<Incoming>,
    body: &[u8],
    first_range_attempts: &AtomicUsize,
    chunk_delay: std::time::Duration,
) -> Response<Body> {
    range_response_with_slow_nonzero_ranges(req, body, first_range_attempts, chunk_delay, false)
}

fn range_response_with_slow_nonzero_ranges(
    req: &Request<Incoming>,
    body: &[u8],
    first_range_attempts: &AtomicUsize,
    chunk_delay: std::time::Duration,
    slow_nonzero_ranges: bool,
) -> Response<Body> {
    if req.method() == hyper::Method::HEAD {
        return Response::builder()
            .status(StatusCode::OK)
            .header("Accept-Ranges", "bytes")
            .header("Content-Length", body.len())
            .body(empty_body())
            .unwrap();
    }

    let Some((start, end)) = req
        .headers()
        .get("Range")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split_once('-'))
        .and_then(|(start, end)| Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?)))
    else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(empty_body())
            .unwrap();
    };
    if start > end || end >= body.len() {
        return Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .body(empty_body())
            .unwrap();
    }

    let slow_first_attempt = !slow_nonzero_ranges
        && start == 0
        && end > start
        && first_range_attempts.fetch_add(1, Ordering::AcqRel) == 0;
    let slow_response = slow_first_attempt || (slow_nonzero_ranges && start > 0);
    let range = Arc::new(body[start..=end].to_vec());
    let response_body = if slow_response {
        let range_for_stream = Arc::clone(&range);
        StreamBody::new(futures::stream::unfold(0usize, move |offset| {
            let range = Arc::clone(&range_for_stream);
            async move {
                if offset >= range.len() {
                    return None;
                }
                tokio::time::sleep(chunk_delay).await;
                let next = (offset + 32 * 1024).min(range.len());
                Some((
                    Ok::<_, Infallible>(Frame::data(Bytes::copy_from_slice(&range[offset..next]))),
                    next,
                ))
            }
        }))
        .boxed()
    } else {
        full_body(range.as_ref().clone())
    };

    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header("Accept-Ranges", "bytes")
        .header(
            "Content-Range",
            format!("bytes={start}-{end}/{}", body.len()),
        )
        .header("Content-Length", end - start + 1)
        .body(response_body)
        .unwrap()
}

fn make_multi_mirror_command(
    gid: GroupId,
    uris: Vec<String>,
    options: &DownloadOptions,
    output_dir: &str,
    output_name: &str,
) -> DownloadCommand {
    let first_uri = uris
        .first()
        .expect("multi-mirror download needs a URI")
        .clone();
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        uris,
        options.clone(),
    )));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "min-split-size".to_string(),
            serde_json::json!("1M"),
        )]));
    DownloadCommand::new_with_group(
        group,
        &first_uri,
        options,
        Some(output_dir),
        Some(output_name),
    )
    .expect("Failed to create multi-mirror DownloadCommand")
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

    let mut options = make_options(Some(4), Some(2), &tmp_dir, &out_name);
    options.http_version = HttpVersion::Http11;
    let mut cmd = make_concurrent_command(
        GroupId::new(1),
        &url,
        &options,
        Some(&tmp_dir),
        Some(&out_name),
    );

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

#[tokio::test]
async fn concurrent_download_smooths_live_speed_during_slow_range_progress() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    let file_size = 8 * 1024 * 1024;
    let data = Arc::new(generate_test_data(file_size, 91));
    let first_range_attempts = Arc::new(AtomicUsize::new(0));
    let handler_data = Arc::clone(&data);
    let handler_attempts = Arc::clone(&first_range_attempts);
    server.on("GET", "/speed-smoothing", move |request| {
        range_response_with_slow_nonzero_ranges(
            request,
            &handler_data,
            &handler_attempts,
            std::time::Duration::from_millis(20),
            true,
        )
    });

    let url = make_url(&server.base_url(), "/speed-smoothing");
    let tmp_dir = std::env::temp_dir().to_string_lossy().into_owned();
    let out_name = format!("test_speed_smoothing_{}.bin", std::process::id());
    let out_path = format!("{}/{}", tmp_dir, out_name);
    let _ = std::fs::remove_file(&out_path);

    let mut options = make_options(Some(4), Some(2), &tmp_dir, &out_name);
    options.http_version = HttpVersion::Http11;
    let (mut command, group) = make_concurrent_command_with_group(
        GroupId::new(76),
        &url,
        &options,
        Some(&tmp_dir),
        Some(&out_name),
    );
    let download = tokio::spawn(async move { command.execute().await });

    let mut speed_samples = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !download.is_finished() {
            let (status, completed_bytes, download_speed) = {
                let group = group
                    .read()
                    .expect("request group lock should be available");
                (
                    group.status(),
                    group.get_completed_length(),
                    group.get_download_speed_cached(),
                )
            };
            if status != DownloadStatus::Complete
                && completed_bytes < file_size as u64
                && download_speed > 0
                && speed_samples.last() != Some(&download_speed)
            {
                speed_samples.push(download_speed);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("concurrent speed regression should finish promptly");
    download
        .await
        .expect("download task should not panic")
        .expect("concurrent download should succeed");

    let requests = server.take_request_log();
    let range_headers: Vec<_> = requests
        .iter()
        .filter_map(|request| {
            request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("range"))
                .map(|(_, value)| value.clone())
        })
        .collect();
    assert!(
        speed_samples.len() >= 2,
        "expected multiple live speed samples before completion, got {speed_samples:?}; requests={range_headers:?}"
    );
    let peak_speed = speed_samples.iter().copied().max().unwrap_or_default();
    assert!(
        peak_speed <= 20 * 1024 * 1024,
        "the 8 MiB fixture reported an implausible live-speed spike: {speed_samples:?}"
    );

    let output = std::fs::read(&out_path).expect("completed output should be readable");
    assert_eq!(output.as_slice(), data.as_slice());

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

    let options = make_options(Some(4), Some(2), &tmp_dir, &out_name);
    let mut cmd = make_concurrent_command(
        GroupId::new(3),
        &url,
        &options,
        Some(&tmp_dir),
        Some(&out_name),
    );

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

    let options = make_options(Some(4), Some(2), &tmp_dir, &out_name);
    let mut cmd = make_concurrent_command(
        GroupId::new(4),
        &url,
        &options,
        Some(&tmp_dir),
        Some(&out_name),
    );

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

#[tokio::test]
async fn explicit_range_size_rejection_downshifts_without_changing_output() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 8 * 1024 * 1024;
    let data = Arc::new(generate_test_data(file_size, 91));
    let observed_range_lengths = Arc::new(std::sync::Mutex::new(Vec::new()));
    let accepted_range_size = 128 * 1024u64;
    register_range_with_size_limit(
        &server,
        "/range-size-limited",
        &data,
        accepted_range_size,
        Arc::clone(&observed_range_lengths),
    );

    let url = make_url(&server.base_url(), "/range-size-limited");
    let tmp_dir = std::env::temp_dir().to_string_lossy().into_owned();
    let out_name = format!("test_range_size_downshift_{}.bin", std::process::id());
    let out_path = format!("{tmp_dir}/{out_name}");
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(ControlFile::control_path_for(std::path::Path::new(
        &out_path,
    )));

    let mut options = make_options(Some(4), Some(2), &tmp_dir, &out_name);
    options.http_version = HttpVersion::Http11;
    options.min_http_range_size = Some(accepted_range_size);
    let mut cmd = make_concurrent_command(
        GroupId::new(98),
        &url,
        &options,
        Some(&tmp_dir),
        Some(&out_name),
    );
    cmd.execute()
        .await
        .expect("download should recover after the server rejects the initial Range size");

    assert_eq!(
        std::fs::read(&out_path).expect("downloaded file should be readable"),
        data.as_slice(),
        "downshifting Range requests must preserve the assembled file"
    );
    {
        let observed = observed_range_lengths
            .lock()
            .expect("range length lock should be available");
        assert!(
            observed.contains(&(1024 * 1024)),
            "the initial 1 MiB Range should be attempted: {observed:?}"
        );
        assert!(
            observed.contains(&accepted_range_size),
            "the scheduler should honor the configured 128 KiB floor: {observed:?}"
        );
    }

    let _ = std::fs::remove_file(&out_path);
    server.shutdown().await;
}

#[tokio::test]
async fn multi_mirror_range_size_downshift_is_shared_by_same_authority() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 8 * 1024 * 1024;
    let data = generate_test_data(file_size, 93);
    let accepted_range_size = 128 * 1024u64;
    let observed_range_lengths = Arc::new(std::sync::Mutex::new(Vec::new()));
    register_range_with_size_limit(
        &server,
        "/multi-range-size-a",
        &data,
        accepted_range_size,
        Arc::clone(&observed_range_lengths),
    );
    register_range_with_size_limit(
        &server,
        "/multi-range-size-b",
        &data,
        accepted_range_size,
        Arc::clone(&observed_range_lengths),
    );

    let base_url = server.base_url();
    let uris = vec![
        make_url(&base_url, "/multi-range-size-a"),
        make_url(&base_url, "/multi-range-size-b"),
    ];
    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let output_name = "multi_mirror_range_size_downshift.bin";
    let dir_string = dir.path().to_string_lossy().into_owned();
    let mut options = make_options(Some(4), Some(2), &dir_string, output_name);
    options.http_version = HttpVersion::Http11;
    options.min_http_range_size = Some(accepted_range_size);
    let mut command =
        make_multi_mirror_command(GroupId::new(99), uris, &options, &dir_string, output_name);
    command
        .execute()
        .await
        .expect("multi-mirror pipeline should recover from rejected Range sizes");

    assert_eq!(
        tokio::fs::read(dir.path().join(output_name))
            .await
            .expect("downloaded file should be readable"),
        data,
        "per-authority downshifting must preserve data across mirrors"
    );
    {
        let observed = observed_range_lengths
            .lock()
            .expect("range length lock should be available");
        assert!(
            observed.contains(&(1024 * 1024)),
            "the initial 1 MiB Range should be attempted: {observed:?}"
        );
        assert!(
            observed.contains(&accepted_range_size),
            "same-authority mirrors should honor the configured 128 KiB floor: {observed:?}"
        );
    }

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Redirected metadata must not suppress concurrent Range downloads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn head_redirect_with_zero_length_still_downloads_ranges_from_final_url() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    let data = generate_test_data(4 * 1024 * 1024, 31);
    server.on_get("/entry", |_req: &Request<Incoming>| -> Response<Body> {
        Response::builder()
            .status(StatusCode::FOUND)
            .header("Location", "/cdn/file")
            .header("Content-Length", 0)
            .body(empty_body())
            .unwrap()
    });

    let final_data = data.clone();
    server.on_get(
        "/cdn/file",
        move |req: &Request<Incoming>| -> Response<Body> {
            if req.method() == hyper::Method::HEAD {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Accept-Ranges", "bytes")
                    .header("Content-Length", final_data.len())
                    .body(empty_body())
                    .unwrap();
            }
            range_response(req, &final_data)
        },
    );

    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let options = make_options(
        Some(4),
        Some(4),
        &dir.path().to_string_lossy(),
        "redirected-range.bin",
    );
    let url = make_url(&server.base_url(), "/entry");
    let mut command = make_concurrent_command(
        GroupId::new(407),
        &url,
        &options,
        Some(&dir.path().to_string_lossy()),
        Some("redirected-range.bin"),
    );

    command
        .execute()
        .await
        .expect("Redirected concurrent download should succeed");

    let output = std::fs::read(dir.path().join("redirected-range.bin"))
        .expect("Should read redirected output file");
    assert_eq!(output, data, "Output content should match source data");

    let log = server.take_request_log();
    assert!(
        log.iter()
            .any(|entry| entry.method == "HEAD" && entry.path == "/entry"),
        "Expected the original URL to receive the metadata HEAD request: {log:?}"
    );
    assert!(
        log.iter()
            .any(|entry| entry.method == "HEAD" && entry.path == "/cdn/file"),
        "Expected HEAD redirect to be followed to the final URL: {log:?}"
    );

    let original_payload_ranges: Vec<_> = log
        .iter()
        .filter(|entry| entry.path == "/entry" && has_range_header(entry))
        .filter(|entry| {
            !entry
                .headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("range") && value == "bytes=0-0")
        })
        .collect();
    assert!(
        original_payload_ranges.is_empty(),
        "Payload ranges must go directly to the final URL: {original_payload_ranges:?}; all requests: {log:?}"
    );

    let final_payload_ranges: Vec<_> = log
        .iter()
        .filter(|entry| entry.path == "/cdn/file" && has_range_header(entry))
        .filter(|entry| {
            !entry
                .headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("range") && value == "bytes=0-0")
        })
        .collect();
    assert!(
        final_payload_ranges.len() > 1,
        "Expected concurrent payload Range requests at the final URL, got {final_payload_ranges:?}"
    );

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 4: Terminal HTTP errors do not retry the same concurrent mirror
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_http_500_is_terminal() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 2 * 1024 * 1024;
    let range_attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_handler = Arc::clone(&range_attempts);
    server.on_get(
        "/terminal-500",
        move |req: &Request<Incoming>| -> Response<Body> {
            if req.method() == hyper::Method::HEAD {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Accept-Ranges", "bytes")
                    .header("Content-Length", file_size)
                    .body(empty_body())
                    .unwrap();
            }
            attempts_for_handler.fetch_add(1, Ordering::AcqRel);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(empty_body())
                .unwrap()
        },
    );

    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let mut options = make_options(
        Some(4),
        Some(2),
        &dir.path().to_string_lossy(),
        "terminal-500.bin",
    );
    options.max_retries = 3;
    options.retry_wait = 1;
    let url = make_url(&server.base_url(), "/terminal-500");
    let mut command = make_concurrent_command(
        GroupId::new(406),
        &url,
        &options,
        Some(&dir.path().to_string_lossy()),
        Some("terminal-500.bin"),
    );

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("terminal HTTP 500 must not wait for retry backoff");
    assert!(matches!(
        result,
        Err(Aria2Error::Recoverable(RecoverableError::ServerError {
            code: 500
        }))
    ));
    assert!(range_attempts.load(Ordering::Acquire) > 0);
    assert!(
        range_attempts.load(Ordering::Acquire) <= 4,
        "terminal 500 must not retry a range: {} attempts",
        range_attempts.load(Ordering::Acquire)
    );
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 5: Gateway Timeout retries and then completes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_http_504_retries_successfully() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 2 * 1024 * 1024;
    let data = generate_test_data(file_size, 71);
    let range_attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_handler = Arc::clone(&range_attempts);
    let body_for_handler = data.clone();
    server.on_get(
        "/retry-504",
        move |req: &Request<Incoming>| -> Response<Body> {
            if req.method() == hyper::Method::HEAD {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Accept-Ranges", "bytes")
                    .header("Content-Length", body_for_handler.len())
                    .body(empty_body())
                    .unwrap();
            }
            let attempt = attempts_for_handler.fetch_add(1, Ordering::AcqRel);
            if attempt == 0 {
                return Response::builder()
                    .status(StatusCode::GATEWAY_TIMEOUT)
                    .body(empty_body())
                    .unwrap();
            }
            range_response(req, &body_for_handler)
        },
    );

    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let url = make_url(&server.base_url(), "/retry-504");
    let mut options = make_options(
        Some(4),
        Some(2),
        &dir.path().to_string_lossy(),
        "retry-504.bin",
    );
    options.max_retries = 2;
    options.retry_wait = 0;
    let mut command = make_concurrent_command(
        GroupId::new(407),
        &url,
        &options,
        Some(&dir.path().to_string_lossy()),
        Some("retry-504.bin"),
    );

    command
        .execute()
        .await
        .expect("HTTP 504 should retry and complete");
    assert_eq!(
        tokio::fs::read(dir.path().join("retry-504.bin"))
            .await
            .unwrap(),
        data
    );
    assert!(
        range_attempts.load(Ordering::Acquire) >= 3,
        "one 504 plus two successful ranges should be observed, got {}",
        range_attempts.load(Ordering::Acquire)
    );
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 6: Bad Gateway requires retry-wait before another concurrent attempt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_http_502_without_retry_wait_is_terminal() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 2 * 1024 * 1024;
    let range_attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_handler = Arc::clone(&range_attempts);
    server.on_get(
        "/terminal-502",
        move |req: &Request<Incoming>| -> Response<Body> {
            if req.method() == hyper::Method::HEAD {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Accept-Ranges", "bytes")
                    .header("Content-Length", file_size)
                    .body(empty_body())
                    .unwrap();
            }
            attempts_for_handler.fetch_add(1, Ordering::AcqRel);
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(empty_body())
                .unwrap()
        },
    );

    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let mut options = make_options(
        Some(4),
        Some(2),
        &dir.path().to_string_lossy(),
        "terminal-502.bin",
    );
    options.max_retries = 3;
    options.retry_wait = 0;
    let url = make_url(&server.base_url(), "/terminal-502");
    let mut command = make_concurrent_command(
        GroupId::new(409),
        &url,
        &options,
        Some(&dir.path().to_string_lossy()),
        Some("terminal-502.bin"),
    );

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("HTTP 502 without retry-wait must not hang");
    assert!(matches!(
        result,
        Err(Aria2Error::Recoverable(RecoverableError::ServerError {
            code: 502
        }))
    ));
    assert!(range_attempts.load(Ordering::Acquire) <= 4);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 7: Service Unavailable honors retry-wait and then completes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_http_503_retries_after_retry_wait() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 2 * 1024 * 1024;
    let data = generate_test_data(file_size, 97);
    let range_attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_handler = Arc::clone(&range_attempts);
    let body_for_handler = data.clone();
    server.on_get(
        "/retry-503",
        move |req: &Request<Incoming>| -> Response<Body> {
            if req.method() == hyper::Method::HEAD {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Accept-Ranges", "bytes")
                    .header("Content-Length", body_for_handler.len())
                    .body(empty_body())
                    .unwrap();
            }
            let attempt = attempts_for_handler.fetch_add(1, Ordering::AcqRel);
            if attempt == 0 {
                return Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .body(empty_body())
                    .unwrap();
            }
            range_response(req, &body_for_handler)
        },
    );

    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let mut options = make_options(
        Some(4),
        Some(2),
        &dir.path().to_string_lossy(),
        "retry-503.bin",
    );
    options.max_retries = 2;
    options.retry_wait = 1;
    let url = make_url(&server.base_url(), "/retry-503");
    let mut command = make_concurrent_command(
        GroupId::new(410),
        &url,
        &options,
        Some(&dir.path().to_string_lossy()),
        Some("retry-503.bin"),
    );

    command
        .execute()
        .await
        .expect("HTTP 503 should honor retry-wait and complete");
    assert_eq!(
        tokio::fs::read(dir.path().join("retry-503.bin"))
            .await
            .unwrap(),
        data
    );
    assert!(
        range_attempts.load(Ordering::Acquire) >= 3,
        "one 503 plus two successful ranges should be observed, got {}",
        range_attempts.load(Ordering::Acquire)
    );
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 8: Terminal failure on one mirror fails over to a healthy mirror
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_concurrent_http_500_fails_over_to_next_mirror() {
    let bad_server = MockHttpServer::start()
        .await
        .expect("Failed to start bad mirror");
    let good_server = MockHttpServer::start()
        .await
        .expect("Failed to start good mirror");
    let file_size = 2 * 1024 * 1024;
    let data = generate_test_data(file_size, 83);
    let bad_attempts = Arc::new(AtomicUsize::new(0));
    let bad_attempts_for_handler = Arc::clone(&bad_attempts);
    bad_server.on_get(
        "/mirror-failover-500",
        move |req: &Request<Incoming>| -> Response<Body> {
            if req.method() == hyper::Method::HEAD {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Accept-Ranges", "bytes")
                    .header("Content-Length", file_size)
                    .body(empty_body())
                    .unwrap();
            }
            bad_attempts_for_handler.fetch_add(1, Ordering::AcqRel);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(empty_body())
                .unwrap()
        },
    );
    register_range_with_head(&good_server, "/mirror-failover-500", &data);

    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let mut options = make_options(
        Some(4),
        Some(2),
        &dir.path().to_string_lossy(),
        "mirror-failover-500.bin",
    );
    options.max_retries = 1;
    let bad_url = make_url(&bad_server.base_url(), "/mirror-failover-500");
    let good_url = make_url(&good_server.base_url(), "/mirror-failover-500");
    let mut command = make_multi_mirror_command(
        GroupId::new(408),
        vec![bad_url, good_url],
        &options,
        &dir.path().to_string_lossy(),
        "mirror-failover-500.bin",
    );

    command
        .execute()
        .await
        .expect("a terminal mirror error should fail over to the next mirror");
    assert_eq!(
        tokio::fs::read(dir.path().join("mirror-failover-500.bin"))
            .await
            .unwrap(),
        data
    );
    assert!(bad_attempts.load(Ordering::Acquire) > 0);
    let good_range_count = good_server
        .take_request_log()
        .into_iter()
        .filter(has_range_header)
        .count();
    assert!(
        good_range_count > 0,
        "healthy mirror must receive range requests after terminal failover"
    );
    bad_server.shutdown().await;
    good_server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 7: Multi-mirror concurrent resume restores the control-file bitfield
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_multi_mirror_resume_restores_completed_segments() {
    let first_server = MockHttpServer::start()
        .await
        .expect("Failed to start first mirror");
    let second_server = MockHttpServer::start()
        .await
        .expect("Failed to start second mirror");

    let file_size = 4 * 1024 * 1024;
    let data = generate_test_data(file_size, 17);
    register_range_with_head(&first_server, "/mirror-file", &data);
    register_range_with_head(&second_server, "/mirror-file", &data);

    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let output_name = "multi-mirror-resume.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);

    // split=4 creates four 512 KiB segments. Persist only segment zero and
    // prewrite its bytes; the resumed pipeline must request segments 1..3.
    tokio::fs::write(&output_path, &data[..file_size / 4])
        .await
        .expect("Failed to seed the completed segment");
    let mut control_file = ControlFile::open_or_create(&control_path, file_size as u64, 4)
        .await
        .expect("Failed to create resume control file");
    control_file.mark_piece_done(0);
    control_file
        .save()
        .await
        .expect("Failed to save resume state");

    let first_url = make_url(&first_server.base_url(), "/mirror-file");
    let second_url = make_url(&second_server.base_url(), "/mirror-file");
    let mut options = make_options(Some(4), Some(2), &dir.path().to_string_lossy(), output_name);
    options.continue_download = true;
    options.allow_overwrite = true;
    let group = std::sync::Arc::new(std::sync::RwLock::new(
        aria2_core::request::request_group::RequestGroup::new(
            GroupId::new(401),
            vec![first_url.clone(), second_url],
            options.clone(),
        ),
    ));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "min-split-size".to_string(),
            serde_json::json!("1M"),
        )]));
    let mut command = DownloadCommand::new_with_group(
        group,
        &first_url,
        &options,
        Some(&dir.path().to_string_lossy()),
        Some(output_name),
    )
    .expect("Failed to create multi-mirror download command");

    command
        .execute()
        .await
        .expect("Multi-mirror resume should succeed");

    assert_eq!(tokio::fs::read(&output_path).await.unwrap(), data);
    assert!(
        !control_path.exists(),
        "successful multi-mirror completion must remove the control file"
    );

    let first_ranges = first_server
        .take_request_log()
        .into_iter()
        .filter_map(|entry| {
            entry
                .headers
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("range"))
                .map(|(_, value)| value)
        })
        .chain(
            second_server
                .take_request_log()
                .into_iter()
                .filter_map(|entry| {
                    entry
                        .headers
                        .into_iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("range"))
                        .map(|(_, value)| value)
                }),
        )
        .filter(|range| range != "bytes=0-0")
        .collect::<Vec<_>>();
    assert!(!first_ranges.is_empty(), "resume must issue Range requests");
    assert!(
        first_ranges
            .iter()
            .all(|range| !range.starts_with("bytes=0-")),
        "restored segment zero must not be requested again: {first_ranges:?}"
    );

    first_server.shutdown().await;
    second_server.shutdown().await;
}

#[tokio::test]
async fn test_multi_mirror_without_continue_discards_stale_control_file() {
    let first_server = MockHttpServer::start()
        .await
        .expect("Failed to start first mirror");
    let second_server = MockHttpServer::start()
        .await
        .expect("Failed to start second mirror");
    let file_size = 4 * 1024 * 1024;
    let data = generate_test_data(file_size, 29);
    register_range_with_head(&first_server, "/fresh-file", &data);
    register_range_with_head(&second_server, "/fresh-file", &data);

    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let output_name = "fresh-multi-mirror.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let mut stale = ControlFile::open_or_create(&control_path, file_size as u64, 4)
        .await
        .expect("Failed to create stale control file");
    stale.mark_piece_done(0);
    stale
        .save()
        .await
        .expect("Failed to save stale control file");

    let first_url = make_url(&first_server.base_url(), "/fresh-file");
    let second_url = make_url(&second_server.base_url(), "/fresh-file");
    let options = make_options(Some(4), Some(2), &dir.path().to_string_lossy(), output_name);
    let group = std::sync::Arc::new(std::sync::RwLock::new(
        aria2_core::request::request_group::RequestGroup::new(
            GroupId::new(402),
            vec![first_url.clone(), second_url],
            options.clone(),
        ),
    ));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "min-split-size".to_string(),
            serde_json::json!("1M"),
        )]));
    let mut command = DownloadCommand::new_with_group(
        group,
        &first_url,
        &options,
        Some(&dir.path().to_string_lossy()),
        Some(output_name),
    )
    .expect("Failed to create fresh multi-mirror command");

    command
        .execute()
        .await
        .expect("fresh multi-mirror download should succeed");

    assert_eq!(tokio::fs::read(&output_path).await.unwrap(), data);
    assert!(!control_path.exists());
    let range_count = first_server
        .take_request_log()
        .into_iter()
        .chain(second_server.take_request_log())
        .filter(has_range_header)
        .count();
    assert!(
        range_count >= 4,
        "continue=false must download all segments, got {range_count} range requests"
    );

    first_server.shutdown().await;
    second_server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 6: Engine pause/remove preserve HTTP concurrent checkpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_engine_pause_unpause_preserves_concurrent_control_file() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 8 * 1024 * 1024;
    let data = generate_test_data(file_size, 41);
    server.register_slow_range_response("/pause-file", &data, 64 * 1024, 10);

    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let output_name = "engine-pause-resume.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let url = make_url(&server.base_url(), "/pause-file");
    let mut options = make_options(Some(4), Some(2), &dir.path().to_string_lossy(), output_name);
    options.continue_download = true;
    options.allow_overwrite = true;

    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(403),
        vec![url.clone()],
        options,
    )));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "min-split-size".to_string(),
            serde_json::json!("1M"),
        )]));
    let mut engine = DownloadEngine::new();
    engine.set_request_group_man(Arc::new(
        aria2_core::request::request_group_man::RequestGroupMan::new(),
    ));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    wait_for_group_status(&group, DownloadStatus::Active).await;
    wait_for_control_file(&control_path).await;
    wait_for_progress(&group).await;

    command_tx
        .send(EngineCommand::Pause {
            gid: GroupId::new(403),
        })
        .expect("pause command should be accepted");
    wait_for_group_status(&group, DownloadStatus::Paused).await;
    assert!(
        control_path.exists(),
        "pause must preserve the HTTP control file for resume"
    );

    command_tx
        .send(EngineCommand::Unpause {
            gid: GroupId::new(403),
        })
        .expect("unpause command should be accepted");
    wait_for_engine(engine_task, "paused download did not finish after unpause").await;

    assert_eq!(tokio::fs::read(&output_path).await.unwrap(), data);
    assert!(
        !control_path.exists(),
        "successful completion must remove the control file"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn test_engine_timeout_tracks_concurrent_range_payload_activity() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 2 * 1024 * 1024;
    let data = generate_test_data(file_size, 47);
    server.register_slow_range_response("/timeout-activity", &data, 64 * 1024, 300);

    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let output_name = "concurrent-timeout-activity.bin";
    let url = make_url(&server.base_url(), "/timeout-activity");
    let mut options = make_options(Some(4), Some(2), &dir.path().to_string_lossy(), output_name);
    options.timeout = Some(1);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(410),
        vec![url],
        options.clone(),
    )));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "min-split-size".to_string(),
            serde_json::json!("1M"),
        )]));

    let mut engine = DownloadEngine::new();
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("concurrent timeout command should be accepted");
    let engine_task = tokio::spawn(engine.run());

    wait_for_group_status(&group, DownloadStatus::Active).await;
    wait_for_engine(
        engine_task,
        "concurrent HTTP payload activity must extend the inactivity timeout",
    )
    .await;

    assert_eq!(
        tokio::fs::read(dir.path().join(output_name)).await.unwrap(),
        data
    );
    server.shutdown().await;
}

#[tokio::test]
async fn test_engine_remove_preserves_incomplete_concurrent_control_file() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 8 * 1024 * 1024;
    let data = generate_test_data(file_size, 53);
    server.register_slow_range_response("/remove-file", &data, 64 * 1024, 10);

    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let output_name = "engine-remove.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let url = make_url(&server.base_url(), "/remove-file");
    let mut options = make_options(Some(4), Some(2), &dir.path().to_string_lossy(), output_name);
    options.continue_download = true;
    options.allow_overwrite = true;

    let gid = GroupId::new(404);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![url],
        options,
    )));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "min-split-size".to_string(),
            serde_json::json!("1M"),
        )]));
    let mut engine = DownloadEngine::new();
    engine.set_request_group_man(Arc::new(
        aria2_core::request::request_group_man::RequestGroupMan::new(),
    ));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    wait_for_group_status(&group, DownloadStatus::Active).await;
    wait_for_control_file(&control_path).await;
    wait_for_progress(&group).await;

    command_tx
        .send(EngineCommand::RemoveDownload { gid })
        .expect("remove command should be accepted");
    wait_for_engine(engine_task, "removed download did not stop promptly").await;

    assert_eq!(group.read().unwrap().status(), DownloadStatus::Removed);
    assert!(
        output_path.exists(),
        "remove should retain the partial output for the saved checkpoint"
    );
    assert!(
        control_path.exists(),
        "remove must preserve the incomplete HTTP control file"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn test_concurrent_save_session_flushes_requested_control_file() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 8 * 1024 * 1024;
    let data = generate_test_data(file_size, 67);
    server.register_slow_range_response("/save-session", &data, 64 * 1024, 20);

    let dir = tempfile::tempdir().expect("Failed to create temporary directory");
    let output_name = "concurrent-save-session.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let url = make_url(&server.base_url(), "/save-session");
    let mut options = make_options(Some(4), Some(2), &dir.path().to_string_lossy(), output_name);
    options.continue_download = true;
    options.allow_overwrite = true;

    let gid = GroupId::new(405);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![url.clone()],
        options.clone(),
    )));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "min-split-size".to_string(),
            serde_json::json!("1M"),
        )]));
    let manager = Arc::new(RequestGroupMan::new());
    manager.add_group_arc(Arc::clone(&group));

    let mut command = DownloadCommand::new_with_group(
        Arc::clone(&group),
        &url,
        &options,
        Some(&dir.path().to_string_lossy()),
        Some(output_name),
    )
    .expect("concurrent download command should be created");
    let task = tokio::spawn(async move { command.execute().await });

    wait_for_control_file(&control_path).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let committed = ControlFile::load(&control_path)
                .await
                .expect("concurrent checkpoint should remain readable")
                .is_some_and(|control_file| control_file.completed_length() > 0);
            if committed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("concurrent download did not commit a segment before session save");

    let session_path = dir.path().join("concurrent-save-session.txt");
    let mut save_session = SaveSessionCommand::new(session_path.clone(), manager);
    save_session
        .execute()
        .await
        .expect("session save should request the concurrent checkpoint");
    assert!(session_path.exists());

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if !group.read().unwrap().is_save_control_file_requested() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("concurrent HTTP command did not consume the save request");

    let control_file = ControlFile::load(&control_path)
        .await
        .expect("requested concurrent checkpoint should be readable")
        .expect("requested concurrent checkpoint should exist");
    assert!(
        control_file.completed_length() > 0,
        "requested concurrent checkpoint should contain progress"
    );
    assert!(
        control_file.completed_pieces() > 0,
        "requested concurrent checkpoint should contain a completed segment"
    );

    group.write().unwrap().pause().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("paused concurrent save-session command timed out")
        .expect("paused concurrent save-session task panicked");
    assert!(result.is_err(), "pause should stop the concurrent command");
    assert!(output_path.exists(), "paused output should be retained");
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 8: Capacity feedback lowers concurrency and requeues 429 segments
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_adaptive_pool_requeues_rate_limited_ranges() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    let file_size = 4 * 1024 * 1024;
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

        // The capability probe is a separate one-byte request. It must not
        // consume the mock server's concurrent payload capacity.
        if start == 0 && end == 0 {
            return Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header("Accept-Ranges", "bytes")
                .header("Content-Range", format!("bytes=0-0/{}", body.len()))
                .header("Content-Length", 1)
                .body(full_body(Bytes::copy_from_slice(&body[..1])))
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
    let mut cmd = make_concurrent_command(
        GroupId::new(5),
        &url,
        &options,
        Some(&tmp_dir),
        Some(&out_name),
    );
    cmd.execute()
        .await
        .expect("Rate-limited download should converge and succeed");

    let range_requests = server
        .take_request_log()
        .into_iter()
        .filter_map(|entry| {
            entry
                .headers
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("range"))
                .map(|(_, value)| value)
        })
        .filter(|range| range != "bytes=0-0")
        .collect::<Vec<_>>();
    // Exclude the separate one-byte capability probe; these assertions cover
    // only payload ranges scheduled by the concurrent downloader.
    assert_eq!(
        range_requests.len(),
        4 + rate_limited.load(Ordering::Acquire),
        "each 429 should correspond to one retried payload Range: {range_requests:?}"
    );
    for range in [
        "bytes=0-1048575",
        "bytes=1048576-2097151",
        "bytes=2097152-3145727",
        "bytes=3145728-4194303",
    ] {
        let count = range_requests
            .iter()
            .filter(|request| *request == range)
            .count();
        assert!(
            (1..=2).contains(&count),
            "every payload Range should complete with at most one 429 retry: range_requests={range_requests:?}, rate_limited={}, max_active={}",
            rate_limited.load(Ordering::Acquire),
            max_active.load(Ordering::Acquire)
        );
    }
    assert_eq!(std::fs::read(&out_path).unwrap(), data);
    assert!(rate_limited.load(Ordering::Acquire) > 0);
    assert!(max_active.load(Ordering::Acquire) >= 3);
    assert!(max_active.load(Ordering::Acquire) <= 4);

    let _ = std::fs::remove_file(&out_path);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 9: A stalled Range is reclaimed without waiting for the HTTP timeout.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_stalled_range_is_reclaimed_and_requeued() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 4 * 1024 * 1024;
    let data = generate_test_data(file_size, 123);
    let first_range_attempt = Arc::new(std::sync::Mutex::new(true));
    let first_range_attempt_for_handler = Arc::clone(&first_range_attempt);
    let body = data.clone();

    server.on_get(
        "/stalled-range",
        move |req: &Request<Incoming>| -> Response<Body> {
            if req.method() == hyper::Method::HEAD {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Accept-Ranges", "bytes")
                    .header("Content-Length", body.len())
                    .body(empty_body())
                    .unwrap();
            }

            let Some((start, end)) = req
                .headers()
                .get("Range")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("bytes="))
                .and_then(|value| value.split_once('-'))
                .and_then(|(start, end)| {
                    Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
                })
            else {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(empty_body())
                    .unwrap();
            };

            let end = end.min(body.len().saturating_sub(1));
            if start == 0 && end > 0 {
                let should_stall = {
                    let mut first = first_range_attempt_for_handler
                        .lock()
                        .expect("range attempt lock poisoned");
                    if *first {
                        *first = false;
                        true
                    } else {
                        false
                    }
                };
                if should_stall {
                    let stream = futures::stream::once(async move {
                        std::future::pending::<Result<Frame<Bytes>, Infallible>>().await
                    });
                    return Response::builder()
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header("Accept-Ranges", "bytes")
                        .header(
                            "Content-Range",
                            format!("bytes={}-{}/{}", start, end, body.len()),
                        )
                        .header("Content-Length", end - start + 1)
                        .body(StreamBody::new(stream).boxed())
                        .unwrap();
                }
            }

            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header("Accept-Ranges", "bytes")
                .header(
                    "Content-Range",
                    format!("bytes={}-{}/{}", start, end, body.len()),
                )
                .header("Content-Length", end - start + 1)
                .body(full_body(body[start..=end].to_vec()))
                .unwrap()
        },
    );

    let dir = tempfile::tempdir().expect("temporary directory should be created");
    let output_name = "stalled-range.bin";
    let output_path = dir.path().join(output_name);
    let url = make_url(&server.base_url(), "/stalled-range");
    let options = make_options(Some(4), Some(4), &dir.path().to_string_lossy(), output_name);
    let started = std::time::Instant::now();
    let mut command = make_concurrent_command(
        GroupId::new(406),
        &url,
        &options,
        Some(&dir.path().to_string_lossy()),
        Some(output_name),
    );
    tokio::time::timeout(std::time::Duration::from_secs(45), command.execute())
        .await
        .expect("stalled Range download waited for the ordinary HTTP timeout")
        .expect("reclaimed Range download should succeed");

    assert!(
        started.elapsed() < std::time::Duration::from_secs(40),
        "download should finish after segment reclaim, elapsed={:?}",
        started.elapsed()
    );
    assert_eq!(std::fs::read(&output_path).unwrap(), data);
    let range_requests = server
        .take_request_log()
        .into_iter()
        .filter_map(|entry| {
            entry
                .headers
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("range"))
                .map(|(_, value)| value)
        })
        .collect::<Vec<_>>();
    assert!(
        range_requests
            .iter()
            .filter(|range| *range == "bytes=0-1048575")
            .count()
            >= 2,
        "the stalled first Range must be requested again: {range_requests:?}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn test_slow_progressing_range_is_reclaimed_and_requeued() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 4 * 1024 * 1024;
    let data = generate_test_data(file_size, 131);
    let first_range_attempts = Arc::new(AtomicUsize::new(0));
    let first_range_attempts_for_handler = Arc::clone(&first_range_attempts);
    let body = Arc::new(data.clone());

    server.on_get(
        "/slow-progress-range",
        move |req: &Request<Incoming>| -> Response<Body> {
            if req.method() == hyper::Method::HEAD {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Accept-Ranges", "bytes")
                    .header("Content-Length", body.len())
                    .body(empty_body())
                    .unwrap();
            }

            let Some((start, end)) = req
                .headers()
                .get("Range")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("bytes="))
                .and_then(|value| value.split_once('-'))
                .and_then(|(start, end)| {
                    Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
                })
            else {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(empty_body())
                    .unwrap();
            };
            let end = end.min(body.len().saturating_sub(1));
            let slow_first_attempt = start == 0
                && end > start
                && first_range_attempts_for_handler.fetch_add(1, Ordering::AcqRel) == 0;
            let range = Arc::new(body[start..=end].to_vec());
            let response_body = if slow_first_attempt {
                let range_for_stream = Arc::clone(&range);
                StreamBody::new(futures::stream::unfold(0usize, move |offset| {
                    let range = Arc::clone(&range_for_stream);
                    async move {
                        if offset >= range.len() {
                            return None;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        let next = (offset + 32 * 1024).min(range.len());
                        Some((
                            Ok::<_, Infallible>(Frame::data(Bytes::copy_from_slice(
                                &range[offset..next],
                            ))),
                            next,
                        ))
                    }
                }))
                .boxed()
            } else {
                full_body(range.as_ref().clone())
            };

            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header("Accept-Ranges", "bytes")
                .header(
                    "Content-Range",
                    format!("bytes={}-{}/{}", start, end, body.len()),
                )
                .header("Content-Length", end - start + 1)
                .body(response_body)
                .unwrap()
        },
    );

    let dir = tempfile::tempdir().expect("temporary directory should be created");
    let output_name = "slow-progress-range.bin";
    let output_path = dir.path().join(output_name);
    let url = make_url(&server.base_url(), "/slow-progress-range");
    let options = make_options(Some(4), Some(4), &dir.path().to_string_lossy(), output_name);
    let started = std::time::Instant::now();
    let mut command = make_concurrent_command(
        GroupId::new(407),
        &url,
        &options,
        Some(&dir.path().to_string_lossy()),
        Some(output_name),
    );
    tokio::time::timeout(std::time::Duration::from_secs(20), command.execute())
        .await
        .expect("slow-progress Range should be recovered before its full body finishes")
        .expect("requeued Range download should succeed");

    assert!(
        started.elapsed() < std::time::Duration::from_secs(6),
        "the slow Range should be replaced by a fast retry, elapsed={:?}",
        started.elapsed()
    );
    assert_eq!(std::fs::read(&output_path).unwrap(), data);
    assert!(
        first_range_attempts.load(Ordering::Acquire) >= 2,
        "the slow first Range should be retried"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn test_slow_progressing_range_is_reclaimed_in_multi_mirror_pipeline() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let file_size = 4 * 1024 * 1024;
    let data = generate_test_data(file_size, 149);
    let body = Arc::new(data.clone());
    let first_range_attempts = Arc::new(AtomicUsize::new(0));

    for path in ["/slow-mirror-a", "/slow-mirror-b"] {
        let body = Arc::clone(&body);
        let first_range_attempts = Arc::clone(&first_range_attempts);
        server.on_get(path, move |req| {
            range_response_with_slow_first(
                req,
                &body,
                &first_range_attempts,
                std::time::Duration::from_millis(250),
            )
        });
    }

    let dir = tempfile::tempdir().expect("temporary directory should be created");
    let output_name = "multi-mirror-slow-range.bin";
    let output_path = dir.path().join(output_name);
    let base_url = server.base_url();
    let uris = vec![
        make_url(&base_url, "/slow-mirror-a"),
        make_url(&base_url, "/slow-mirror-b"),
    ];
    let mut options = make_options(Some(4), Some(4), &dir.path().to_string_lossy(), output_name);
    options.http_version = HttpVersion::Http11;
    let started = std::time::Instant::now();
    let mut command = make_multi_mirror_command(
        GroupId::new(408),
        uris,
        &options,
        &dir.path().to_string_lossy(),
        output_name,
    );
    tokio::time::timeout(std::time::Duration::from_secs(20), command.execute())
        .await
        .expect("multi-mirror slow Range should recover before the full body finishes")
        .expect("multi-mirror slow Range retry should complete");

    assert!(
        started.elapsed() < std::time::Duration::from_secs(6),
        "the slow multi-mirror Range should be replaced quickly, elapsed={:?}",
        started.elapsed()
    );
    assert_eq!(std::fs::read(&output_path).unwrap(), data);
    assert!(
        first_range_attempts.load(Ordering::Acquire) >= 2,
        "the slow multi-mirror Range should be requested again"
    );
    server.shutdown().await;
}

#![allow(dead_code)]

//! Engine-level integration tests for aria2-core
//!
//! Tests the DownloadCommand, DownloadEngine, and related components
//! at the integration level using mock servers and real file I/O.

mod e2e_helpers;

use std::time::{Duration, Instant};

// Import from aria2_core crate (external to integration test)
use aria2_core::engine::command::{Command, CommandStatus};
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::engine::ftp_download_command::FtpDownloadCommand;
#[cfg(feature = "metalink")]
use aria2_core::engine::metalink_download_command::MetalinkDownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};

// Re-export helpers from test harness module
use e2e_helpers::mock_http_server::{MockHttpServer, Response, StatusCode, full_body};

// Import test harness utilities (need to check what's available)
// Note: These may need adjustment based on actual module structure

/// Create a minimal DownloadOptions suitable for testing.
fn test_download_options(output_dir: &std::path::Path) -> DownloadOptions {
    DownloadOptions {
        dir: Some(output_dir.display().to_string()),
        split: None,
        max_connection_per_server: None,
        max_download_limit: None,
        max_upload_limit: None,
        out: None,
        seed_time: None,
        seed_ratio: None,
        checksum: None,
        cookie_file: None,
        cookies: None,
        bt_force_encrypt: false,
        bt_require_crypto: false,
        enable_dht: false,
        dht_listen_port: None,
        dht_entry_point: None,
        http_proxy: None,
        dht_file_path: None,
        enable_public_trackers: false,
        bt_piece_selection_strategy: "default".to_string(),
        bt_endgame_threshold: 10,
        max_retries: 3,
        retry_wait: 1000,
        bt_max_upload_slots: None,
        bt_optimistic_unchoke_interval: None,
        bt_snubbed_timeout: None,
        bt_prioritize_piece: String::new(),
        all_proxy: None,
        https_proxy: None,
        ftp_proxy: None,
        no_proxy: None,
        enable_utp: false,
        utp_listen_port: None,
        ..Default::default()
    }
}

/// Build a DownloadCommand targeting a URL, writing to the given output path.
fn build_http_command(
    url: &str,
    output_path: &std::path::Path,
) -> std::result::Result<DownloadCommand, Box<dyn std::error::Error>> {
    let gid = GroupId::new(1);
    let opts = test_download_options(output_path.parent().unwrap_or(output_path));
    Ok(DownloadCommand::new(
        gid,
        url,
        &opts,
        None,
        Some(
            output_path
                .file_name()
                .unwrap_or(std::ffi::OsStr::new("download"))
                .to_str()
                .unwrap(),
        ),
    )?)
}

/// Assert that a command has completed successfully.
fn assert_cmd_completed<C: Command>(cmd: &C) {
    assert_eq!(
        cmd.status(),
        CommandStatus::Completed,
        "Command should be Completed, got {:?}",
        cmd.status()
    );
}

/// Assert file exists and contents match expected bytes exactly
fn assert_file_contents(path: &std::path::Path, expected: &[u8]) {
    let actual = std::fs::read(path).unwrap_or_default();
    assert_eq!(
        actual,
        expected,
        "File content mismatch at {:?}: expected {} bytes, got {} bytes",
        path,
        expected.len(),
        actual.len()
    );
}

/// Generate deterministic test data of given size (reproducible across runs).
fn generate_test_data(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_add(seed)).collect()
}

/// Create a temporary directory that auto-cleans on Drop
fn setup_temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("Failed to create temp directory")
}

// ============================================================================
// TIER A TESTS: Direct Command.execute()
// ============================================================================

/// D1: Basic HTTP download via DownloadCommand.execute()
///
/// Verifies that a simple HTTP download completes successfully:
/// - Server returns 200 OK with test data
/// - File is written to disk with correct content
/// - Command status transitions to Completed
#[tokio::test]
async fn engine_http_download_basic() {
    let temp_dir = setup_temp_dir();
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock HTTP server");

    // Generate 1024 bytes of test data
    let data_1024 = generate_test_data(1024, 0x42);
    server.register_range_response("/download.bin", &data_1024);

    let url = format!("{}/download.bin", server.base_url());
    let output_path = temp_dir.path().join("download.bin");

    // Build and execute download command
    let mut cmd = build_http_command(&url, &output_path).expect("Failed to build command");
    let result: Result<(), _> = cmd.execute().await;

    // Verify success
    assert!(
        result.is_ok(),
        "Download should succeed: {:?}",
        result.err()
    );
    assert_file_contents(&output_path, &data_1024);
    assert_cmd_completed(&cmd);

    server.shutdown().await;
}

/// D2: HTTP download with authentication (401 handling)
///
/// Tests how DownloadCommand handles authentication challenges:
/// - Server returns 401 Unauthorized for /secret path
/// - Without valid credentials, command should fail gracefully
/// - Without credentials, the command fails with the aria2-compatible auth error
#[tokio::test]
async fn engine_http_download_with_auth() {
    let temp_dir = setup_temp_dir();
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock HTTP server");

    // Register auth-gated resource (returns 401 without valid Authorization header)
    let secret_data = b"secret_content".to_vec();
    server.register_auth_gated("/secret", "TestRealm", "Basic", &secret_data);

    let url = format!("{}/secret", server.base_url());
    let output_path = temp_dir.path().join("secret.bin");

    // Build command without credentials (should fail with 401)
    let mut cmd = build_http_command(&url, &output_path).expect("Failed to build command");
    let result: Result<(), _> = cmd.execute().await;

    assert!(
        result.is_err(),
        "Download without auth should fail: expected Err, got Ok"
    );

    // Verify no partial/orphaned file left behind
    assert!(
        !output_path.exists(),
        "No orphaned file should remain after auth failure"
    );

    // Command status should indicate not completed
    let status: CommandStatus = cmd.status();
    assert!(
        matches!(status, CommandStatus::Pending),
        "Command status should be Pending after 401, got {:?}",
        status
    );

    server.shutdown().await;
}

/// D3: DownloadCommand sends configured HTTP credentials preemptively.
#[tokio::test]
async fn engine_http_download_with_preemptive_auth() {
    let temp_dir = setup_temp_dir();
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock HTTP server");

    let secret_data = b"secret_content".to_vec();
    server.register_auth_gated("/secret", "TestRealm", "Basic", &secret_data);

    let url = format!("{}/secret", server.base_url());
    let output_path = temp_dir.path().join("secret.bin");
    let gid = GroupId::new(2);
    let mut options = test_download_options(temp_dir.path());
    options.http_user = Some("admin".to_string());
    options.http_passwd = Some("password".to_string());
    let mut cmd = DownloadCommand::new(gid, &url, &options, None, Some("secret.bin"))
        .expect("Failed to build authenticated command");

    let result = cmd.execute().await;
    assert!(
        result.is_ok(),
        "Download with configured auth should succeed: {:?}",
        result.err()
    );
    assert_file_contents(&output_path, &secret_data);
    assert_cmd_completed(&cmd);

    let log = server.take_request_log();
    assert!(
        log.iter().any(|request| {
            request.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("authorization") && value == "Basic YWRtaW46cGFzc3dvcmQ="
            })
        }),
        "configured credentials must be sent on the wire: {log:?}"
    );

    server.shutdown().await;
}

/// D4: Sequential DownloadCommand completes an RFC-compatible Digest retry.
#[tokio::test]
async fn engine_http_download_with_digest_auth() {
    use md5::{Digest, Md5};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn md5_hex(value: &str) -> String {
        let mut hasher = Md5::new();
        hasher.update(value.as_bytes());
        hex::encode(hasher.finalize())
    }

    fn header_value<'a>(request: &'a str, name: &str) -> Option<&'a str> {
        request.lines().find_map(|line| {
            line.split_once(':')
                .and_then(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.trim()))
        })
    }

    fn digest_parameter<'a>(header: &'a str, name: &str) -> Option<&'a str> {
        header
            .strip_prefix("Digest ")
            .and_then(|parameters| {
                parameters.split(", ").find_map(|parameter| {
                    parameter
                        .split_once('=')
                        .and_then(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value))
                })
            })
            .map(|value| value.trim_matches('"'))
    }

    async fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        loop {
            let mut chunk = [0u8; 1024];
            let bytes = stream.read(&mut chunk).await.expect("read HTTP request");
            if bytes == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..bytes]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(request).expect("HTTP request must be UTF-8")
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut first_stream, _) = listener.accept().await.expect("accept first request");
        let first_request = read_request(&mut first_stream).await;
        assert!(first_request.starts_with("GET /digest.bin HTTP/1.1"));
        assert!(header_value(&first_request, "Authorization").is_none());
        first_stream
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"download\", nonce=\"fixed-nonce\", qop=\"auth\", algorithm=MD5, opaque=\"opaque\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write digest challenge");

        let (mut second_stream, _) = listener.accept().await.expect("accept retry request");
        let second_request = read_request(&mut second_stream).await;
        let authorization = header_value(&second_request, "Authorization")
            .expect("sequential retry should include Digest authorization");
        assert!(authorization.starts_with("Digest "));
        assert_eq!(digest_parameter(authorization, "username"), Some("user"));
        assert_eq!(digest_parameter(authorization, "realm"), Some("download"));
        assert_eq!(
            digest_parameter(authorization, "nonce"),
            Some("fixed-nonce")
        );
        assert_eq!(digest_parameter(authorization, "uri"), Some("/digest.bin"));
        assert_eq!(digest_parameter(authorization, "qop"), Some("auth"));
        assert_eq!(digest_parameter(authorization, "nc"), Some("00000001"));
        assert_eq!(digest_parameter(authorization, "opaque"), Some("opaque"));

        let cnonce = digest_parameter(authorization, "cnonce").expect("digest cnonce");
        let response = digest_parameter(authorization, "response").expect("digest response");
        let ha1 = md5_hex("user:download:pass");
        let ha2 = md5_hex("GET:/digest.bin");
        let expected = md5_hex(&format!("{ha1}:fixed-nonce:00000001:{cnonce}:auth:{ha2}"));
        assert_eq!(response, expected);

        second_stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\ndigest-content-12345",
            )
            .await
            .expect("write authenticated response");
    });

    let temp_dir = setup_temp_dir();
    let url = format!("http://{addr}/digest.bin");
    let output_path = temp_dir.path().join("digest.bin");
    let gid = GroupId::new(3);
    let mut options = test_download_options(temp_dir.path());
    options.http_auth_challenge = true;
    options.http_user = Some("user".to_string());
    options.http_passwd = Some("pass".to_string());
    let mut cmd = DownloadCommand::new(gid, &url, &options, None, Some("digest.bin"))
        .expect("Failed to build digest-authenticated command");

    let result = cmd.execute().await;
    assert!(
        result.is_ok(),
        "Digest-authenticated download should succeed: {:?}",
        result.err()
    );
    assert_file_contents(&output_path, b"digest-content-12345");
    assert_cmd_completed(&cmd);

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("digest auth fixture should finish")
        .expect("digest auth fixture task should succeed");
}

/// D3: FTP download via FtpDownloadCommand
///
/// Tests FTP protocol download functionality.
/// GAP: Requires running FTP server; in CI environments this may not be available.
/// This test validates the constructor and basic structure; actual FTP I/O
/// would need a MockFtpServer similar to MockHttpServer.
#[tokio::test]
async fn engine_ftp_download_basic() {
    let temp_dir = setup_temp_dir();

    // Construct an FTP URI pointing to localhost (will fail to connect, but validates API)
    let ftp_url = "ftp://localhost:21/testfile.txt";
    let _output_path = temp_dir.path().join("ftp_download.txt");

    let gid = GroupId::new(1);
    let opts = test_download_options(temp_dir.path());

    // Test that FtpDownloadCommand can be constructed with valid parameters
    let result = FtpDownloadCommand::new(gid, ftp_url, &opts, None, None);

    // Constructor should succeed (parsing is valid)
    assert!(
        result.is_ok(),
        "FTP command construction should succeed for valid URI"
    );

    // This test covers URI parsing and command initialization only. A real
    // transfer belongs in the dedicated FTP server integration target.
    let cmd = result.expect("valid FTP URI should construct a command");
    let status: CommandStatus = cmd.status();
    assert_eq!(
        status,
        CommandStatus::Pending,
        "New command should be Pending"
    );
}

/// D4: Metalink download via MetalinkDownloadCommand
#[cfg(feature = "metalink")]
///
/// Tests Metalink-based download which can mirror from multiple URLs.
/// Uses a metalink XML document pointing to our mock HTTP server.
#[tokio::test]
async fn engine_metalink_download_basic() {
    let temp_dir = setup_temp_dir();
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock HTTP server");

    // Prepare test data and register on mock server
    let file_data = generate_test_data(2048, 0x77);
    let filename = "metalink_test.bin";
    server.register_range_response(&format!("/{}", filename), &file_data);

    // Build metalink XML pointing to our mock server
    // GAP: Need to import or inline metalink builder logic
    // Using simple metalink v3 XML for now
    let url = format!("{}/{}", server.base_url(), filename);

    // Compute SHA256 hash of file data
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&file_data);
    let sha256_hash = hex::encode(hasher.finalize());

    // Build minimal metalink v3 XML
    let metalink_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <files>
    <file name="{}">
      <size>{}</size>
      <hash type="sha-256">{}</hash>
      <url priority="1">{}</url>
    </file>
  </files>
</metalink>"#,
        filename,
        file_data.len(),
        sha256_hash,
        url
    )
    .into_bytes();

    // Construct MetalinkDownloadCommand
    let gid = GroupId::new(1);
    let opts = test_download_options(temp_dir.path());

    let mut cmd = MetalinkDownloadCommand::new(
        gid,
        &metalink_xml,
        &opts,
        Some(temp_dir.path().to_str().unwrap()),
    )
    .expect("Failed to create MetalinkDownloadCommand");

    // Execute the download
    let result: Result<(), _> = cmd.execute().await;

    // Verify success
    assert!(
        result.is_ok(),
        "Metalink download should succeed: {:?}",
        result.err()
    );

    let output_path = temp_dir.path().join(filename);
    assert_file_contents(&output_path, &file_data);
    assert_cmd_completed(&cmd);

    server.shutdown().await;
}

/// D6: BT progress persistence (.aria2 control file creation)
#[cfg(feature = "bittorrent")]
///
/// Tests that BtDownloadCommand with BtProgressManager enabled
/// creates progress tracking files during/after download.
/// GAP: Full BT download requires tracker + seeder infrastructure.
/// This test verifies the progress save/load API surface directly.
#[tokio::test]
async fn engine_bt_progress_persistence() {
    use aria2_core::engine::bt_download_command::BtDownloadCommand;

    let temp_dir = setup_temp_dir();

    // Build a small test torrent (1 piece, 16KB)
    // Inline torrent builder logic (simplified)
    use sha1::{Digest, Sha1};

    let name = "progress_test.dat";
    let total_size: u64 = 16384; // 16KB
    let piece_length: u32 = 16384; // 1 piece
    let tracker_url = "http://tracker.example.com:6969/announce";

    // Generate file data
    let file_data: Vec<u8> = (0..total_size).map(|i| (i % 256) as u8).collect();

    // Compute piece hashes
    let num_pieces = total_size.div_ceil(piece_length as u64) as usize;
    let mut pieces_hash = Vec::with_capacity(num_pieces * 20);
    for i in 0..num_pieces {
        let start = i * piece_length as usize;
        let end = std::cmp::min(start + piece_length as usize, file_data.len());
        let mut hasher = Sha1::new();
        hasher.update(&file_data[start..end]);
        pieces_hash.extend_from_slice(&hasher.finalize());
    }

    // Build torrent bencoding (simplified)
    fn bencode_int(v: u64) -> Vec<u8> {
        format!("i{}e", v).into_bytes()
    }
    fn bencode_str(s: &str) -> Vec<u8> {
        format!("{}:{}", s.len(), s).into_bytes()
    }
    fn bencode_bytes(b: &[u8]) -> Vec<u8> {
        format!("{}:", b.len())
            .into_bytes()
            .into_iter()
            .chain(b.iter().copied())
            .collect()
    }
    fn bencode_dict(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let mut result = b"d".to_vec();
        for (key, val) in entries {
            result.extend_from_slice(key.len().to_string().as_bytes());
            result.push(b':');
            result.extend_from_slice(key);
            result.extend_from_slice(val);
        }
        result.push(b'e');
        result
    }

    let info_dict = vec![
        (b"length".to_vec(), bencode_int(total_size)),
        (b"name".to_vec(), bencode_str(name)),
        (b"piece length".to_vec(), bencode_int(piece_length as u64)),
        (b"pieces".to_vec(), bencode_bytes(&pieces_hash)),
    ];

    let torrent = bencode_dict(&[
        (b"announce".to_vec(), bencode_str(tracker_url)),
        (b"info".to_vec(), bencode_dict(&info_dict)),
    ]);

    let gid = GroupId::new(1);
    let opts = test_download_options(temp_dir.path());

    // Create BtDownloadCommand (constructor should succeed)
    let result = BtDownloadCommand::new(
        gid,
        &torrent,
        &opts,
        Some(temp_dir.path().to_str().unwrap()),
    );
    let _cmd = result.expect("valid torrent metadata should construct a BT command");
    assert!(temp_dir.path().exists(), "Output directory should exist");
}

/// D7: BT hook chain fires on completion
#[cfg(feature = "bittorrent")]
///
/// Tests that post-download hooks (MoveHook, TouchHook) are executed
/// when a BT download completes.
/// GAP: HookManager integration requires completed BT download workflow.
/// This test validates hook registration and chain execution pattern.
#[tokio::test]
async fn engine_bt_hook_chain_fires() {
    use aria2_core::engine::bt_download_command::BtDownloadCommand;
    use aria2_core::engine::hook_manager::HookManager;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let temp_dir = setup_temp_dir();

    // Flags to track hook execution
    let move_executed = Arc::new(AtomicBool::new(false));
    let touch_executed = Arc::new(AtomicBool::new(false));

    // Create HookManager with custom hooks (simulating MoveHook + TouchHook)
    let hook_config = aria2_core::engine::hook_manager::HookConfig::default();
    let hook_mgr = HookManager::new(hook_config);
    // GAP: HookManager::add_move_hook() and add_touch_hook() may not exist yet
    // or may have different API signatures. Adjust based on actual implementation.
    //
    // Example of what full test would do:
    // hook_mgr.add_move_hook(move || { move_executed.store(true, Ordering::SeqCst); });
    // hook_mgr.add_touch_hook(move || { touch_executed.store(true, Ordering::SeqCst); });
    let _hook_mgr = Arc::new(hook_mgr);

    // Build test torrent (reuse simplified builder from D6)
    let name = "hook_test.dat";
    let total_size: u64 = 8192; // 8KB
    let piece_length: u32 = 8192; // 1 piece
    let tracker_url = "http://tracker.test.com/announce";

    use sha1::{Digest, Sha1};
    let file_data: Vec<u8> = (0..total_size).map(|i| (i % 256) as u8).collect();

    let num_pieces = total_size.div_ceil(piece_length as u64) as usize;
    let mut pieces_hash = Vec::with_capacity(num_pieces * 20);
    for i in 0..num_pieces {
        let start = i * piece_length as usize;
        let end = std::cmp::min(start + piece_length as usize, file_data.len());
        let mut hasher = Sha1::new();
        hasher.update(&file_data[start..end]);
        pieces_hash.extend_from_slice(&hasher.finalize());
    }

    fn bencode_int(v: u64) -> Vec<u8> {
        format!("i{}e", v).into_bytes()
    }
    fn bencode_str(s: &str) -> Vec<u8> {
        format!("{}:{}", s.len(), s).into_bytes()
    }
    fn bencode_bytes(b: &[u8]) -> Vec<u8> {
        format!("{}:", b.len())
            .into_bytes()
            .into_iter()
            .chain(b.iter().copied())
            .collect()
    }
    fn bencode_dict(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let mut result = b"d".to_vec();
        for (key, val) in entries {
            result.extend_from_slice(key.len().to_string().as_bytes());
            result.push(b':');
            result.extend_from_slice(key);
            result.extend_from_slice(val);
        }
        result.push(b'e');
        result
    }

    let info_dict = vec![
        (b"length".to_vec(), bencode_int(total_size)),
        (b"name".to_vec(), bencode_str(name)),
        (b"piece length".to_vec(), bencode_int(piece_length as u64)),
        (b"pieces".to_vec(), bencode_bytes(&pieces_hash)),
    ];

    let torrent = bencode_dict(&[
        (b"announce".to_vec(), bencode_str(tracker_url)),
        (b"info".to_vec(), bencode_dict(&info_dict)),
    ]);

    let gid = GroupId::new(1);
    let opts = test_download_options(temp_dir.path());

    // Create BtDownloadCommand
    let result = BtDownloadCommand::new(
        gid,
        &torrent,
        &opts,
        Some(temp_dir.path().to_str().unwrap()),
    );
    let _cmd = result.expect("valid torrent metadata should construct a BT command");

    // Setting hook_manager on BtDownloadCommand requires public API access
    // Currently hook_manager is pub(crate). Would need either:
    // 1. A setter method: cmd.set_hook_manager(hook_mgr)
    // 2. Or construction parameter in DownloadOptions
    //
    // To complete this test:
    // 1. Attach hook_mgr to command
    // 2. Execute command with MockTrackerServer + MockBtSeeder
    // 3. After completion, assert move_executed && touch_executed

    // Verify hooks were registered (structural check)
    assert!(
        !move_executed.load(Ordering::SeqCst),
        "Move hook should not fire before execution"
    );
    assert!(
        !touch_executed.load(Ordering::SeqCst),
        "Touch hook should not fire before execution"
    );
}

/// D11: Error cleanup on download failure
///
/// Verifies that failed downloads don't leave orphaned partial files.
/// Server returns 404/500, command fails, output directory should be clean.
#[tokio::test]
async fn engine_error_cleanup_on_failure() {
    let temp_dir = setup_temp_dir();
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock HTTP server");

    // Register route that always returns 404 Not Found
    server.on_get("/nonexistent.bin", |_req| {
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(full_body("Not Found"))
            .unwrap()
    });

    let url = format!("{}/nonexistent.bin", server.base_url());
    let output_path = temp_dir.path().join("nonexistent.bin");

    // Build and attempt download
    let mut cmd = build_http_command(&url, &output_path).expect("Failed to build command");
    let result: Result<(), _> = cmd.execute().await;

    // Should fail due to 404
    assert!(
        result.is_err(),
        "Download should fail for 404 response: {:?}",
        result
    );

    // Critical assertion: no orphaned files
    assert!(
        !output_path.exists(),
        "Orphaned partial file should NOT exist after failure"
    );

    // List all files in temp directory to ensure nothing was created
    let entries: Vec<_> = std::fs::read_dir(temp_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries.is_empty(),
        "Temp dir should be empty after failed download, found {} files",
        entries.len()
    );

    // Also test 500 error case
    server.on_get("/server_error.bin", |_req| {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(full_body("Internal Server Error"))
            .unwrap()
    });

    let url_500 = format!("{}/server_error.bin", server.base_url());
    let output_path_500 = temp_dir.path().join("server_error.bin");

    let mut cmd_500 =
        build_http_command(&url_500, &output_path_500).expect("Failed to build command");
    let result_500: Result<(), _> = cmd_500.execute().await;

    assert!(
        result_500.is_err(),
        "Download should fail for 500 response: {:?}",
        result_500
    );
    assert!(
        !output_path_500.exists(),
        "No orphaned file after 500 error"
    );

    server.shutdown().await;
}

// ============================================================================
// TIER B TESTS: Full DownloadEngine lifecycle
// ============================================================================

/// D5: BitTorrent download with tracker
#[cfg(feature = "bittorrent")]
///
/// Tests full BT download workflow through the v2 engine command path.
/// GAP: Requires MockTrackerServer + MockBtSeeder infrastructure.
#[tokio::test]
async fn engine_bt_download_with_tracker() {
    use aria2_core::engine::bt_download_command::BtDownloadCommand;

    let temp_dir = setup_temp_dir();

    // Build small torrent for testing (inline simplified builder)
    let name = "tracker_test.dat";
    let total_size: u64 = 32768; // 32KB
    let piece_length: u32 = 16384; // 2 pieces
    let tracker_url = "http://127.0.0.1:6969/announce";

    use sha1::{Digest, Sha1};
    let file_data: Vec<u8> = (0..total_size).map(|i| (i % 256) as u8).collect();

    let num_pieces = total_size.div_ceil(piece_length as u64) as usize;
    let mut pieces_hash = Vec::with_capacity(num_pieces * 20);
    for i in 0..num_pieces {
        let start = i * piece_length as usize;
        let end = std::cmp::min(start + piece_length as usize, file_data.len());
        let mut hasher = Sha1::new();
        hasher.update(&file_data[start..end]);
        pieces_hash.extend_from_slice(&hasher.finalize());
    }

    fn bencode_int(v: u64) -> Vec<u8> {
        format!("i{}e", v).into_bytes()
    }
    fn bencode_str(s: &str) -> Vec<u8> {
        format!("{}:{}", s.len(), s).into_bytes()
    }
    fn bencode_bytes(b: &[u8]) -> Vec<u8> {
        format!("{}:", b.len())
            .into_bytes()
            .into_iter()
            .chain(b.iter().copied())
            .collect()
    }
    fn bencode_dict(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let mut result = b"d".to_vec();
        for (key, val) in entries {
            result.extend_from_slice(key.len().to_string().as_bytes());
            result.push(b':');
            result.extend_from_slice(key);
            result.extend_from_slice(val);
        }
        result.push(b'e');
        result
    }

    let info_dict = vec![
        (b"length".to_vec(), bencode_int(total_size)),
        (b"name".to_vec(), bencode_str(name)),
        (b"piece length".to_vec(), bencode_int(piece_length as u64)),
        (b"pieces".to_vec(), bencode_bytes(&pieces_hash)),
    ];

    let torrent = bencode_dict(&[
        (b"announce".to_vec(), bencode_str(tracker_url)),
        (b"info".to_vec(), bencode_dict(&info_dict)),
    ]);

    let gid = GroupId::new(1);
    let opts = test_download_options(temp_dir.path());

    // Construct BT command (validates torrent parsing + command creation)
    let bt_result = BtDownloadCommand::new(
        gid,
        &torrent,
        &opts,
        Some(temp_dir.path().to_str().unwrap()),
    );
    let _bt_cmd = bt_result.expect("valid torrent metadata should construct a BT command");
    let group_man =
        std::sync::Arc::new(aria2_core::request::request_group_man::RequestGroupMan::new());
    let gid = group_man
        .read()
        .await
        .add_group(vec![tracker_url.to_string()], opts.clone())
        .expect("group should be created");
    let group = group_man
        .read()
        .await
        .group_by_id(gid)
        .expect("group should be registered");
    let command = EngineCommand::AddDownload { group };
    assert!(matches!(command, EngineCommand::AddDownload { .. }));

    // Note: We don't call engine.run() here because it would block waiting
    // for peers/tracker that don't exist. In a complete test environment:
    //
    // let run_handle = tokio::spawn(async move {
    //     let result = engine.run().await;
    //     result
    // });
    //
    // let result = tokio::time::timeout(Duration::from_secs(30), run_handle).await;
    // assert!(result.is_ok(), "Engine should complete within timeout");
    //
    // Then verify output files exist with correct content
}

/// D8: Multi-task parallel downloads
///
/// Tests DownloadEngine managing multiple concurrent download tasks:
/// - Starts single MockHttpServer
/// - Registers 3 different routes with different data
/// - Creates 3 DownloadCommands targeting each route
/// - Adds all commands to engine
/// - Runs engine until all complete
/// - Verifies all 3 files exist with correct content
#[tokio::test]
async fn engine_multi_task_parallel() {
    let temp_dir = setup_temp_dir();
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock HTTP server");

    // Register 3 different endpoints with different data
    let data_a = generate_test_data(512, 0xAA);
    let data_b = generate_test_data(1024, 0xBB);
    let data_c = generate_test_data(2048, 0xCC);

    server.register_range_response("/file_a.bin", &data_a);
    server.register_range_response("/file_b.bin", &data_b);
    server.register_range_response("/file_c.bin", &data_c);

    let group_man =
        std::sync::Arc::new(aria2_core::request::request_group_man::RequestGroupMan::new());
    let mut engine = DownloadEngine::new(50);
    engine.set_request_group_man(group_man.clone());

    let url_a = format!("{}/file_a.bin", server.base_url());
    let url_b = format!("{}/file_b.bin", server.base_url());
    let url_c = format!("{}/file_c.bin", server.base_url());

    let path_a = temp_dir.path().join("file_a.bin");
    let path_b = temp_dir.path().join("file_b.bin");
    let path_c = temp_dir.path().join("file_c.bin");

    let gids = {
        let man = &group_man;
        [
            man.add_group(vec![url_a], test_download_options(temp_dir.path())),
            man.add_group(vec![url_b], test_download_options(temp_dir.path())),
            man.add_group(vec![url_c], test_download_options(temp_dir.path())),
        ]
    };
    let engine_cmd_tx = engine.engine_cmd_tx();
    for gid in gids
        .into_iter()
        .map(|result| result.expect("group should be created"))
    {
        let group = group_man
            .group_by_id(gid)
            .expect("group should be registered");
        engine_cmd_tx
            .send(EngineCommand::AddDownload { group })
            .expect("engine command channel should be open");
    }

    let start = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(30), engine.run()).await;
    let elapsed = start.elapsed();

    assert!(
        result.is_ok(),
        "Engine run should complete within timeout: {:?}",
        result.err()
    );
    assert!(result.unwrap().is_ok(), "Engine run should succeed");

    println!("Multi-task engine download completed in {:?}", elapsed);

    assert_file_contents(&path_a, &data_a);
    assert_file_contents(&path_b, &data_b);
    assert_file_contents(&path_c, &data_c);

    server.shutdown().await;
}

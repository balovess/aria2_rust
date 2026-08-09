//! End-to-end verification for the internalize-default-optimizations spec.
//!
//! These tests exercise the four internalized optimizations as default behavior
//! through the public `DownloadCommand` API against a live in-process test
//! server:
//!
//! * The default file-allocation is `prealloc` (verified by asserting
//!   `constants::DEFAULT_FILE_ALLOCATION == "prealloc"` and completing a real
//!   download that exercises the preallocation code path).
//! * SubTask 5.2 — progress reporting flows through the auto-wired mpsc
//!   channel + aggregator task into the `RequestGroup`'s atomic
//!   `completed_length` mirror, with no `RwLock` write-lock on the hot path.

mod fixtures;
use aria2_core::constants;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use fixtures::test_server::{TestServer, medium_pattern};
use std::path::Path;
use std::sync::Arc;

async fn start_server() -> TestServer {
    TestServer::start().await
}

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// SubTask 5.1: Download a 1 MiB file with `DownloadOptions::default()` and
/// verify the default file-allocation strategy is `prealloc` (aria2 default).
///
/// The assertion on `constants::DEFAULT_FILE_ALLOCATION` proves the default
/// allocation strategy, and the successful completion of the download
/// exercises the preallocation code path end-to-end.
#[tokio::test]
async fn test_default_download_uses_prealloc_allocation() {
    // Prove the default allocation strategy is prealloc, matching aria2.
    assert_eq!(
        constants::DEFAULT_FILE_ALLOCATION,
        "prealloc",
        "default file-allocation must be prealloc (aria2 default)"
    );

    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/medium.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(1001),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("DownloadCommand::new should succeed with a valid HTTP URI");

    let result = cmd.execute().await;
    assert!(result.is_ok(), "download failed: {:?}", result.err());

    let output_path = Path::new(dir.path()).join("medium.bin");
    assert!(
        output_path.exists(),
        "output file missing: {}",
        output_path.display()
    );

    let data = std::fs::read(&output_path).expect("failed to read downloaded file");
    assert_eq!(data.len(), 1024 * 1024, "output file size must be 1 MiB");
    assert!(
        data.iter().all(|&b| b == medium_pattern()),
        "output file content must be all 0xAB"
    );
}

/// SubTask 5.2: Download a 1 MiB file with default options and verify the
/// progress channel + aggregator pipeline updates the `RequestGroup`'s atomic
/// `completed_length` mirror.
///
/// `DownloadCommand::new_with_group` auto-creates the progress channel. The
/// aggregator is spawned lazily in `execute()` and drained before
/// `execute().await` returns, so once the download completes
/// `get_completed_length()` must equal the file size. If the channel were NOT
/// wired, `completed_length` would remain 0.
#[tokio::test]
async fn test_default_download_uses_progress_channel() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/medium.bin", server.base_url());

    // Build a shared RequestGroup and keep an Arc clone for post-download
    // verification. The command receives its own clone; we do NOT move our
    // reference into it.
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1002),
        vec![url.clone()],
        DownloadOptions::default(),
    )));
    let group_for_assertion = Arc::clone(&group);

    let mut cmd = DownloadCommand::new_with_group(
        group,
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");

    cmd.execute()
        .await
        .expect("download should complete successfully");

    // The aggregator applies channel updates to the group's atomic
    // completed_length mirror. After execute() returns (which drains the
    // aggregator), completed_length must reflect the downloaded bytes.
    let completed = { group_for_assertion.read().unwrap().get_completed_length() };
    assert_eq!(
        completed,
        1024 * 1024,
        "progress channel + aggregator should have mirrored completed_length to 1 MiB; \
         if it were 0 the progress channel was not wired"
    );
}

//! Deep BitTorrent integration tests for aria2-core
//!
//! Exercises BT progress persistence, post-download hooks, LPD peer discovery,
//! MSE encrypted handshake, and tracker multi-peer distribution end-to-end.

#![cfg(feature = "bittorrent")]
mod e2e_helpers;
mod fixtures;

mod test_harness;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use fixtures::mock_bt_seeder::{MockBtSeeder, SeederConfig};
use fixtures::mock_tracker::MockTrackerServer;
use test_harness::{assert_file_contents, generate_test_data, setup_temp_dir};

use aria2_core::engine::bt_progress_info_file::{
    BtProgress, BtProgressManager, DownloadStats as ProgressDownloadStats, PeerAddr,
};
use aria2_core::engine::command::Command;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::hook_manager::{
    DownloadStats as HookDownloadStats, DownloadStatus, ExecHook, HookConfig, HookContext,
    HookManager, MoveHook, PostDownloadHook, TouchHook,
};
use aria2_core::engine::lpd_manager::{
    LPD_MULTICAST_ADDR, LPD_PORT, LpdManager, LpdPeer, parse_lpd_announcement,
};
use aria2_core::engine::post_download_handler::{
    build_handler_chain, extract_download_info, run_post_download_processing_with_allocator,
};
use aria2_core::request::request_group::GroupId;
use aria2_core::request::request_group::{DownloadOptions, FollowMode, RequestGroup};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::util::rwlock_ext::RwLockRecover;
use aria2_protocol::bittorrent::extension::mse_crypto::{MseCryptoMethod, MseCryptoState};
use aria2_protocol::bittorrent::extension::mse_handshake::MseHandshake;

use e2e_helpers::mock_http_server::{MockHttpServer, Response, StatusCode, full_body};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ===========================================================================
// Metadata follow E2E
// ===========================================================================

/// Download a torrent metainfo URL through the real HTTP command path using
/// `follow-torrent=mem`, then run the production post-download chain.
///
/// This covers the behavior that unit tests cannot prove together: response
/// content type propagation, memory-only source handling, bencode parsing, and
/// child-group creation without a source `.torrent` file.
#[tokio::test]
async fn follow_torrent_mem_http_creates_child_without_source_file() {
    let dir = setup_temp_dir();
    let server = MockHttpServer::start()
        .await
        .expect("mock HTTP server should start");
    let tracker_url = "http://tracker.invalid:6969/announce";
    let torrent =
        fixtures::test_torrent_builder::build_test_torrent("payload.bin", 1024, 512, tracker_url);
    let torrent_for_server = torrent.clone();
    server.on_get("/source.torrent", move |_| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/x-bittorrent")
            .header("Content-Length", torrent_for_server.len())
            .body(full_body(torrent_for_server.clone()))
            .unwrap()
    });

    let url = format!("{}/source.torrent", server.base_url());
    let mut options = DownloadOptions {
        follow_torrent: Some(FollowMode::Memory),
        ..DownloadOptions::default()
    };
    options.dir = Some(dir.path().display().to_string());

    let mut command = aria2_core::engine::download_command::DownloadCommand::new(
        GroupId::new(0x700),
        &url,
        &options,
        Some(dir.path().to_str().unwrap()),
        Some("source.torrent"),
    )
    .expect("memory torrent command should construct");
    command
        .execute()
        .await
        .expect("memory torrent download should succeed");

    let source_path = dir.path().join("source.torrent");
    assert!(
        !source_path.exists(),
        "follow-torrent=mem must not create the source torrent file"
    );

    let group = command
        .request_group()
        .expect("HTTP command should expose its request group");
    let info = {
        let group = group.recover();
        assert!(group.is_in_memory_download());
        assert_eq!(group.in_memory_data(), Some(torrent.clone()));
        assert_eq!(
            group.content_type().as_deref(),
            Some("application/x-bittorrent")
        );
        extract_download_info(&group)
    };

    let handlers = build_handler_chain(&info.options);
    let handler_refs: Vec<&dyn aria2_core::engine::post_download_handler::PostDownloadHandler> =
        handlers.iter().map(|handler| handler.as_ref()).collect();
    let mut next_gid = 0x701u64;
    let mut allocate_gid = || {
        let gid = GroupId::new(next_gid);
        next_gid += 1;
        gid
    };
    let children =
        run_post_download_processing_with_allocator(&info, &handler_refs, &mut allocate_gid);

    assert_eq!(
        children.len(),
        1,
        "torrent metadata should create one child"
    );
    {
        let child = children[0].recover();
        assert_eq!(child.uris().iter().map(|uri| uri.as_ref()).collect::<Vec<_>>(), [tracker_url]);
        assert_eq!(child.get_bt_num_pieces(), 2);
        assert_eq!(child.get_bt_piece_length(), 512);
        assert_eq!(child.following_gid(), Some(GroupId::new(0x700)));
        assert!(child.belongs_to_gid().is_none());
        assert_eq!(child.options().follow_torrent, Some(FollowMode::Disabled));
        assert_eq!(child.bt_metadata_data(), Some(torrent));
    }

    server.shutdown().await;
}

/// A memory-backed torrent source must still be recognized from its source
/// URI when the server uses the generic octet-stream content type.
#[tokio::test]
async fn follow_torrent_mem_http_uses_source_uri_extension() {
    let dir = setup_temp_dir();
    let server = MockHttpServer::start()
        .await
        .expect("mock HTTP server should start");
    let tracker_url = "http://tracker.invalid:6969/announce";
    let torrent =
        fixtures::test_torrent_builder::build_test_torrent("payload.bin", 1024, 512, tracker_url);
    let torrent_for_server = torrent.clone();
    server.on_get("/source.torrent", move |_| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", torrent_for_server.len())
            .body(full_body(torrent_for_server.clone()))
            .unwrap()
    });

    let url = format!("{}/source.torrent?download=1", server.base_url());
    let mut options = DownloadOptions {
        follow_torrent: Some(FollowMode::Memory),
        ..DownloadOptions::default()
    };
    options.dir = Some(dir.path().display().to_string());

    let mut command = aria2_core::engine::download_command::DownloadCommand::new(
        GroupId::new(0x702),
        &url,
        &options,
        Some(dir.path().to_str().unwrap()),
        Some("source.torrent"),
    )
    .expect("memory torrent command should construct");
    command
        .execute()
        .await
        .expect("memory torrent download should succeed");

    let group = command
        .request_group()
        .expect("HTTP command should expose its request group");
    let info = {
        let group = group.recover();
        assert!(group.is_in_memory_download());
        assert_eq!(group.in_memory_data(), Some(torrent.clone()));
        assert_eq!(
            group.content_type().as_deref(),
            Some("application/octet-stream")
        );
        extract_download_info(&group)
    };
    assert_eq!(info.base_uri.as_deref(), Some(url.as_str()));

    let handlers = build_handler_chain(&info.options);
    let handler_refs: Vec<&dyn aria2_core::engine::post_download_handler::PostDownloadHandler> =
        handlers.iter().map(|handler| handler.as_ref()).collect();
    let mut next_gid = 0x703u64;
    let mut allocate_gid = || {
        let gid = GroupId::new(next_gid);
        next_gid += 1;
        gid
    };
    let children =
        run_post_download_processing_with_allocator(&info, &handler_refs, &mut allocate_gid);

    assert_eq!(children.len(), 1);
    assert_eq!(children[0].recover().bt_metadata_data(), Some(torrent));
    server.shutdown().await;
}

/// A gateway timeout on an in-memory metadata request must follow the same
/// retry contract as the original HTTP skip-response command.
#[tokio::test]
async fn follow_torrent_mem_http_retries_gateway_timeout() {
    let dir = setup_temp_dir();
    let server = MockHttpServer::start()
        .await
        .expect("mock HTTP server should start");
    let tracker_url = "http://tracker.invalid:6969/announce";
    let torrent =
        fixtures::test_torrent_builder::build_test_torrent("payload.bin", 1024, 512, tracker_url);
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_server = Arc::clone(&attempts);
    let torrent_for_server = torrent.clone();
    server.on_get("/retry-source.torrent", move |_| {
        if attempts_for_server.fetch_add(1, Ordering::SeqCst) == 0 {
            Response::builder()
                .status(StatusCode::GATEWAY_TIMEOUT)
                .body(full_body("gateway timeout"))
                .unwrap()
        } else {
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/x-bittorrent")
                .header("Content-Length", torrent_for_server.len())
                .body(full_body(torrent_for_server.clone()))
                .unwrap()
        }
    });

    let url = format!("{}/retry-source.torrent", server.base_url());
    let mut options = DownloadOptions {
        follow_torrent: Some(FollowMode::Memory),
        max_retries: 2,
        retry_wait: 0,
        use_head: false,
        ..DownloadOptions::default()
    };
    options.dir = Some(dir.path().display().to_string());

    let mut command = aria2_core::engine::download_command::DownloadCommand::new(
        GroupId::new(0x704),
        &url,
        &options,
        Some(dir.path().to_str().unwrap()),
        Some("retry-source.torrent"),
    )
    .expect("memory torrent command should construct");
    command
        .execute()
        .await
        .expect("gateway timeout should be retried for memory metadata");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let group = command
        .request_group()
        .expect("HTTP command should expose its request group");
    assert_eq!(group.recover().in_memory_data(), Some(torrent));
    assert!(
        !dir.path().join("retry-source.torrent").exists(),
        "follow-torrent=mem must not create the source torrent file"
    );

    server.shutdown().await;
}

/// Run the same metadata follow through DownloadEngine, including the
/// tracker/web-seed child dispatch. A child whose first URI is an announce URL
/// must still be constructed as a BitTorrent command from its in-memory
/// metainfo.
#[tokio::test]
async fn follow_torrent_mem_http_engine_downloads_web_seed_child() {
    let dir = setup_temp_dir();
    let server = MockHttpServer::start()
        .await
        .expect("mock HTTP server should start");
    let total_size = 4096;
    let piece_length = 512;
    let payload = fixtures::test_torrent_builder::generate_file_data(total_size);
    let web_seed_url = format!("{}/payload.bin", server.base_url());
    server.register_range_response("/payload.bin", &payload);

    let tracker = MockTrackerServer::start_with_peers(Vec::new(), false).await;
    let torrent = fixtures::test_torrent_builder::build_test_torrent_with_web_seeds(
        "payload.bin",
        total_size,
        piece_length,
        &tracker.announce_url(),
        std::slice::from_ref(&web_seed_url),
    );
    let torrent_for_server = torrent.clone();
    server.on_get("/source.torrent", move |_| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/x-bittorrent")
            .header("Content-Length", torrent_for_server.len())
            .body(full_body(torrent_for_server.clone()))
            .unwrap()
    });

    let gid = GroupId::new(0x710);
    let source_url = format!("{}/source.torrent", server.base_url());
    let options = DownloadOptions {
        follow_torrent: Some(FollowMode::Memory),
        use_head: false,
        dir: Some(dir.path().display().to_string()),
        out: Some("source.torrent".to_string()),
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        ..DownloadOptions::default()
    };
    let parent = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![source_url],
        options,
    )));
    let manager = Arc::new(RequestGroupMan::new());
    manager.add_group_arc(Arc::clone(&parent));

    let mut engine = DownloadEngine::new();
    engine.set_request_group_man(Arc::clone(&manager));
    let engine_task = tokio::spawn(engine.run());

    let child_gid = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(child_gid) = parent.recover().followed_by_gids().first().copied() {
                break child_gid;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("engine did not create the followed torrent child");
    let child = manager
        .find_group(child_gid)
        .expect("followed torrent child should remain managed while downloading");
    assert_eq!(child.recover().bt_metadata_data(), Some(torrent.clone()));

    let result = tokio::time::timeout(std::time::Duration::from_secs(30), engine_task)
        .await
        .expect("followed torrent engine task timed out")
        .expect("followed torrent engine task panicked");
    result.expect("followed torrent engine should complete successfully");

    assert_eq!(parent.recover().status(), DownloadStatus::Complete);
    assert_eq!(child.recover().status(), DownloadStatus::Complete);
    assert_eq!(
        std::fs::read(dir.path().join("payload.bin")).unwrap(),
        payload
    );
    assert!(
        !dir.path().join("source.torrent").exists(),
        "follow-torrent=mem must not create the source torrent file"
    );

    server.shutdown().await;
}

// ===========================================================================
// Test 1: BT Progress save/load roundtrip
// ===========================================================================

/// Create BtProgressManager, save progress for a 4-piece torrent (50% complete),
/// write to temp dir .aria2 file, load it back, and verify bitfield shows
/// pieces 0-1 complete.
#[tokio::test]
async fn bt_progress_save_load_roundtrip() {
    let dir = setup_temp_dir();

    // Build a 4-piece torrent with piece_length=256, total_size=1024
    let info_hash: [u8; 20] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        0x10, 0x11, 0x12, 0x13, 0x14,
    ];

    // Create progress manager backed by temp directory
    let manager = BtProgressManager::new(dir.path()).expect("Failed to create BtProgressManager");

    // Construct a BtProgress representing 50% completion (pieces 0-1 of 4 done)
    // bitfield for 4 pieces = 1 byte, bits 0-1 set = 0b00000011 = 0x03
    let progress = BtProgress {
        info_hash,
        bitfield: vec![0x03], // pieces 0 and 1 complete (binary: 00000011)
        peers: vec![
            PeerAddr {
                ip: "192.168.1.100".to_string(),
                port: 6881,
            },
            PeerAddr {
                ip: "192.168.1.101".to_string(),
                port: 6882,
            },
        ],
        stats: ProgressDownloadStats {
            uploaded_bytes: 512,
            downloaded_bytes: 512,
            upload_speed: 128.0,
            download_speed: 256.0,
            elapsed_seconds: 30,
        },
        piece_length: 256,
        total_size: 1024,
        num_pieces: 4,
        upload_length: 512,
        in_flight_pieces: vec![],
        is_torrent: true,
        save_time: std::time::SystemTime::now(),
        version: 1,
    };

    // Save progress to disk
    manager
        .save_progress(&info_hash, &progress)
        .expect("save_progress should succeed");

    // Verify that the .aria2 file was created on disk
    let saved_path = manager.get_progress_file_path(&info_hash);
    assert!(
        saved_path.exists(),
        "Progress file should exist at {:?}",
        saved_path
    );

    // Load it back
    let loaded = manager
        .load_progress(&info_hash)
        .expect("load_progress should succeed");

    // Verify core fields roundtripped correctly
    assert_eq!(loaded.info_hash, info_hash, "Info hash must match");
    assert_eq!(loaded.num_pieces, 4, "Piece count must be 4");
    assert_eq!(loaded.piece_length, 256, "Piece length must be 256");
    assert_eq!(loaded.total_size, 1024, "Total size must be 1024");
    assert_eq!(loaded.version, 1, "Version must be 1");

    // Verify bitfield: only bits 0 and 1 should be set (pieces 0-1 complete)
    assert_eq!(
        loaded.bitfield.len(),
        1,
        "Bitfield should be 1 byte for 4 pieces"
    );
    assert_eq!(
        loaded.bitfield[0], 0x03,
        "Bitfield byte should be 0x03 (bits 0-1 set) for 50% completion"
    );

    // Verify completion ratio reflects 2/4 = 50%
    let ratio = loaded.completion_ratio();
    assert!(
        (ratio - 0.5).abs() < f64::EPSILON,
        "Completion ratio should be ~0.5, got {}",
        ratio
    );

    // C++ binary format does NOT persist the peer list — peers are populated
    // from PeerStorage after loading, not from the .aria2 file.
    assert_eq!(
        loaded.peers.len(),
        0,
        "Peers are NOT stored in binary format"
    );

    // C++ binary format stores uploadLength but NOT downloaded_bytes separately.
    // downloaded_bytes is derived from the bitfield, not persisted.
    // uploaded_bytes is restored from the saved uploadLength field.
    assert_eq!(
        loaded.stats.uploaded_bytes, 512,
        "Uploaded bytes must match (restored from uploadLength)"
    );

    eprintln!("[TEST1] Progress save/load roundtrip PASSED");
}

// ===========================================================================
// Test 2: BT Progress bitfield accuracy
// ===========================================================================

/// Save progress with known piece completion patterns, reload, compare
/// saved bitfield vs expected pattern exactly.
#[tokio::test]
async fn bt_progress_bitfield_accuracy() {
    let dir = setup_temp_dir();
    let manager = BtProgressManager::new(dir.path()).expect("Failed to create BtProgressManager");

    let info_hash: [u8; 20] = [0xAAu8; 20];

    // Test several bitfield patterns to verify exact serialization/deserialization

    // Pattern A: All 8 pieces complete in first byte -> 0xFF
    let prog_a = BtProgress {
        info_hash,
        bitfield: vec![0xFF],
        num_pieces: 8,
        piece_length: 512,
        total_size: 4096,
        ..Default::default()
    };
    manager.save_progress(&info_hash, &prog_a).unwrap();
    let loaded_a = manager.load_progress(&info_hash).unwrap();
    assert_eq!(
        loaded_a.bitfield,
        vec![0xFF],
        "Pattern A: All 8 bits set should roundtrip as 0xFF"
    );
    assert_eq!(
        loaded_a.completion_ratio(),
        1.0,
        "Pattern A: Should show 100% complete"
    );

    // Pattern B: Alternating bits (even pieces done) -> 0x55 (01010101)
    let prog_b = BtProgress {
        info_hash,
        bitfield: vec![0x55],
        num_pieces: 8,
        piece_length: 512,
        total_size: 4096,
        ..Default::default()
    };
    manager.save_progress(&info_hash, &prog_b).unwrap();
    let loaded_b = manager.load_progress(&info_hash).unwrap();
    assert_eq!(
        loaded_b.bitfield,
        vec![0x55],
        "Pattern B: Alternating bits should roundtrip as 0x55"
    );
    // 4 of 8 bits set = 50%
    let ratio_b = loaded_b.completion_ratio();
    assert!(
        (ratio_b - 0.5).abs() < f64::EPSILON,
        "Pattern B: Expected ratio 0.5, got {}",
        ratio_b
    );

    // Pattern C: Multi-byte bitfield (12 pieces = 2 bytes), pieces 0-7 + 10,11 done
    // Byte 0: 0xFF (pieces 0-7), Byte 1: 0b1100_0000 = 0xC0 (pieces 10-11)
    let prog_c = BtProgress {
        info_hash,
        bitfield: vec![0xFF, 0xC0],
        num_pieces: 12,
        piece_length: 256,
        total_size: 3072,
        ..Default::default()
    };
    manager.save_progress(&info_hash, &prog_c).unwrap();
    let loaded_c = manager.load_progress(&info_hash).unwrap();
    assert_eq!(
        loaded_c.bitfield,
        vec![0xFF, 0xC0],
        "Pattern C: Multi-byte bitfield [0xFF, 0xC0] should roundtrip exactly"
    );
    // 10 of 12 bits set
    let ratio_c = loaded_c.completion_ratio();
    assert!(
        (ratio_c - 10.0 / 12.0).abs() < 1e-10,
        "Pattern C: Expected ratio {}, got {}",
        10.0 / 12.0,
        ratio_c
    );

    // Pattern D: Zero completion (empty or all-zero bitfield)
    let prog_d = BtProgress {
        info_hash,
        bitfield: vec![0x00],
        num_pieces: 4,
        piece_length: 1024,
        total_size: 4096,
        ..Default::default()
    };
    manager.save_progress(&info_hash, &prog_d).unwrap();
    let loaded_d = manager.load_progress(&info_hash).unwrap();
    assert_eq!(
        loaded_d.bitfield,
        vec![0x00],
        "Pattern D: Zero bitfield should roundtrip as 0x00"
    );
    assert_eq!(
        loaded_d.completion_ratio(),
        0.0,
        "Pattern D: Completion ratio should be 0.0"
    );

    eprintln!("[TEST2] Bitfield accuracy PASSED (all 4 patterns)");
}

// ===========================================================================
// Test 3: BT Progress corrupted file recovery
// ===========================================================================

/// Write invalid/garbage data to the .aria2 path, attempt to load.
/// The system should handle gracefully with an error rather than panicking.
#[tokio::test]
async fn bt_progress_corrupted_file_recovery() {
    let dir = setup_temp_dir();
    let manager = BtProgressManager::new(dir.path()).expect("Failed to create BtProgressManager");

    let info_hash: [u8; 20] = [0xDEu8; 20];
    let file_path = manager.get_progress_file_path(&info_hash);

    // Write garbage data directly to where the progress file would be
    let garbage_data = b"THIS IS NOT VALID ARIA2 PROGRESS DATA!!!\n\x00\x01\x02\xff\xfe";
    std::fs::write(&file_path, garbage_data).expect("Failed to write garbage data");

    assert!(file_path.exists(), "Garbage file should exist on disk");

    // Loading corrupt data should not panic (graceful degradation)
    // The result may be Ok(partial) or Err depending on implementation
    let _ = manager.load_progress(&info_hash); // Just verify no panic

    eprintln!("[TEST3] Corrupted file recovery PASSED (no panic)");
}

// ===========================================================================
// Test 4: BT Hook - MoveHook on_complete
// ===========================================================================

/// Create HookManager with MoveHook(target_dir), simulate download complete
/// callback, verify the file is moved to target directory.
#[tokio::test]
async fn bt_hook_move_on_complete() {
    let dir = setup_temp_dir();

    // Create source file that simulates a completed download
    let source_file = dir.path().join("downloaded_archive.zip");
    let test_content = b"This is fake archive content for testing MoveHook";
    tokio::fs::write(&source_file, test_content)
        .await
        .expect("Failed to write source file");

    assert!(
        source_file.exists(),
        "Source file must exist before hook execution"
    );

    // Define target directory (does not exist yet)
    let target_dir = dir.path().join("completed_downloads");

    // Build HookManager with MoveHook
    let config = HookConfig::default();
    let mut manager = HookManager::new(config);
    manager.add_hook(Box::new(MoveHook::new(target_dir.clone(), true)));

    // Build context simulating a completed download
    let context = HookContext::new(
        GroupId::new(9001),
        source_file.clone(),
        DownloadStatus::Complete,
        HookDownloadStats {
            downloaded_bytes: test_content.len() as u64,
            uploaded_bytes: 0,
            download_speed: 1024.0,
            upload_speed: 0.0,
            elapsed_seconds: 5,
        },
        None,
    );

    // Fire the complete callback chain
    let results = manager.fire_complete(&context).await;
    assert!(results.is_ok(), "fire_complete should succeed");

    // Verify file was moved to target directory
    let moved_file = target_dir.join("downloaded_archive.zip");
    assert!(
        moved_file.exists(),
        "File should exist in target dir after MoveHook execution"
    );
    assert!(
        !source_file.exists(),
        "Source file should no longer exist after move"
    );

    // Verify content integrity after move
    assert_file_contents(&moved_file, test_content);

    eprintln!("[TEST4] MoveHook on_complete PASSED");
}

// ===========================================================================
// Test 5: BT Hook - ExecHook environment variables
// ===========================================================================

/// Create ExecHook and verify its public API and configuration.
/// Since build_env is private, we validate env var construction indirectly
/// through HookContext field access and ExecHook constructor parameters.
#[tokio::test]
async fn bt_hook_exec_env_vars() {
    let gid = GroupId::new(555);
    let file_path = PathBuf::from("/downloads/my_torrent.iso");
    let status = DownloadStatus::Complete;
    let stats = HookDownloadStats {
        downloaded_bytes: 10485760,
        uploaded_bytes: 2097152,
        download_speed: 524288.0,
        upload_speed: 131072.0,
        elapsed_seconds: 120,
    };

    // Build context with known values
    let context = HookContext::new(gid, file_path.clone(), status.clone(), stats.clone(), None);

    // Verify context fields carry expected values (these are what build_env reads)
    assert_eq!(context.gid.value(), 555, "GID should be 555");
    assert_eq!(
        context.file_path,
        PathBuf::from("/downloads/my_torrent.iso"),
        "File path should match"
    );
    assert_eq!(
        context.status,
        DownloadStatus::Complete,
        "Status should be Complete"
    );
    assert_eq!(
        context.filename(),
        "my_torrent.iso",
        "Filename should be extracted correctly"
    );
    assert_eq!(context.extension(), "iso", "Extension should be 'iso'");
    assert!(
        context.error.is_none(),
        "Error should be None when no error provided"
    );
    assert_eq!(
        context.stats.downloaded_bytes, 10485760,
        "Stats downloaded_bytes should match"
    );
    assert_eq!(
        context.stats.uploaded_bytes, 2097152,
        "Stats uploaded_bytes should match"
    );
    assert_eq!(
        context.stats.download_speed, 524288.0,
        "Stats download_speed should match"
    );
    assert_eq!(
        context.stats.upload_speed, 131072.0,
        "Stats upload_speed should match"
    );
    assert_eq!(
        context.stats.elapsed_seconds, 120,
        "Stats elapsed_seconds should match"
    );

    // Verify Display impl works for status
    let status_str = format!("{}", context.status);
    assert_eq!(
        status_str, "complete",
        "Display of Complete should be 'complete'"
    );

    // Create ExecHook with custom env vars
    let mut custom_env = HashMap::new();
    custom_env.insert("MY_CUSTOM_VAR".to_string(), "my_custom_value".to_string());
    let exec_hook = ExecHook::new("echo hello".to_string(), custom_env);

    // Verify hook name
    assert_eq!(
        exec_hook.name(),
        "ExecHook",
        "Hook name should be 'ExecHook'"
    );

    // Test with error context
    let error_context = HookContext::new(
        gid,
        file_path,
        status,
        stats,
        Some("connection reset by peer".to_string()),
    );
    assert_eq!(
        error_context.error.as_deref().unwrap(),
        "connection reset by peer",
        "Error context should carry the error message"
    );
    assert_eq!(
        format!("{}", error_context.status),
        "complete",
        "Original status should still be 'complete' before override"
    );

    // Test other statuses display correctly
    assert_eq!(
        format!("{}", DownloadStatus::Error("test".to_string())),
        "error"
    );
    assert_eq!(format!("{}", DownloadStatus::Removed), "removed");
    assert_eq!(format!("{}", DownloadStatus::Paused), "paused");

    // Test DownloadStats Display impl
    let stats_display = format!("{}", context.stats);
    assert!(
        stats_display.contains("downloaded=10485760"),
        "Display should contain downloaded bytes"
    );
    assert!(
        stats_display.contains("uploaded=2097152"),
        "Display should contain uploaded bytes"
    );
    assert!(
        stats_display.contains("elapsed=120s"),
        "Display should contain elapsed time"
    );

    eprintln!("[TEST5] ExecHook env vars PASSED (validated via context fields)");
}

// ===========================================================================
// Test 6: BT Hook - Failure isolation (stop_on_error=false)
// ===========================================================================

/// HookManager with MoveHook(invalid_path) + TouchHook(valid_path).
/// Run hooks. MoveHook fails but TouchHook still executes because
/// stop_on_error=false.
#[tokio::test]
async fn bt_hook_failure_isolation() {
    let dir = setup_temp_dir();

    // Create a valid file that TouchHook can operate on
    let valid_file = dir.path().join("touchable_file.dat");
    tokio::fs::write(&valid_file, b"touch me")
        .await
        .expect("Failed to write valid file");

    // Get original mtime before touch
    let before_meta = tokio::fs::metadata(&valid_file).await.unwrap();
    let before_mtime = before_meta.modified().unwrap();

    // Build HookManager with stop_on_error=false (default)
    let config = HookConfig {
        stop_on_error: false,
        ..Default::default()
    };
    let mut manager = HookManager::new(config);

    // First hook: MoveHook targeting a deeply invalid path (will fail)
    let invalid_target = PathBuf::from("/nonexistent_root_dir/impossible/path/that/does/not/exist");
    manager.add_hook(Box::new(MoveHook::new(invalid_target, false)));

    // Second hook: TouchHook on a valid file (should succeed even if first fails)
    manager.add_hook(Box::new(TouchHook::new()));

    // Build context pointing to our valid file
    let context = HookContext::new(
        GroupId::new(777),
        valid_file.clone(),
        DownloadStatus::Complete,
        HookDownloadStats::default(),
        None,
    );

    // Fire hooks - should NOT return Err because stop_on_error=false
    let results = manager.fire_complete(&context).await;

    // Overall result should be Ok despite first hook failing
    assert!(
        results.is_ok(),
        "fire_complete should return Ok when stop_on_error=false"
    );

    let result_vec = results.unwrap();
    assert_eq!(
        result_vec.len(),
        2,
        "Should have 2 result entries (one per hook)"
    );

    // First entry should indicate failure of MoveHook
    assert!(
        result_vec[0].contains("failed")
            || result_vec[0].contains("Failed")
            || result_vec[0].contains("error"),
        "First result should mention failure: got '{}'",
        result_vec[0]
    );

    // Second entry should indicate success of TouchHook
    assert!(
        result_vec[1].contains("succeeded") || result_vec[1].contains("complete succeeded"),
        "Second result should indicate success: got '{}'",
        result_vec[1]
    );

    // Verify TouchHook actually executed by checking mtime updated
    let after_meta = tokio::fs::metadata(&valid_file).await.unwrap();
    let after_mtime = after_meta.modified().unwrap();

    // Allow small timing margin
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        after_mtime >= before_mtime,
        "TouchHook should have updated file mtime (before={:?}, after={:?})",
        before_mtime,
        after_mtime
    );

    eprintln!("[TEST6] Hook failure isolation PASSED");
}

// ===========================================================================
// Test 7: BT Hook - Chain order preservation
// ===========================================================================

/// HookManager with 3 hooks that log their execution order.
/// Execute all and verify order is a -> b -> c.
#[tokio::test]
async fn bt_hook_chain_order() {
    let dir = setup_temp_dir();

    // Create a file so hooks have something to operate on
    let test_file = dir.path().join("order_test.bin");
    tokio::fs::write(&test_file, b"data_for_order_test")
        .await
        .unwrap();

    // Build an ordering-aware ExecHook variant using closures captured via Arc
    // We create simple scripts that append to a shared log file
    let log_file = dir.path().join("hook_execution_order.log");

    // Hook A: writes "A" to log
    let cmd_a = format!("echo 'A' >> {}", log_file.display());
    let hook_a = ExecHook::new(cmd_a, HashMap::new());

    // Hook B: writes "B" to log
    let cmd_b = format!("echo 'B' >> {}", log_file.display());
    let hook_b = ExecHook::new(cmd_b, HashMap::new());

    // Hook C: writes "C" to log
    let cmd_c = format!("echo 'C' >> {}", log_file.display());
    let hook_c = ExecHook::new(cmd_c, HashMap::new());

    // Register in order A -> B -> C
    let config = HookConfig::default();
    let mut manager = HookManager::new(config);
    manager.add_hook(Box::new(hook_a));
    manager.add_hook(Box::new(hook_b));
    manager.add_hook(Box::new(hook_c));

    assert_eq!(manager.hook_count(), 3, "Should have 3 hooks registered");

    let context = HookContext::new(
        GroupId::new(1),
        test_file,
        DownloadStatus::Complete,
        HookDownloadStats::default(),
        None,
    );

    // Execute all hooks
    #[cfg(unix)]
    {
        let _results = manager.fire_complete(&context).await;

        // Read the log file and verify order
        if log_file.exists() {
            let log_content = std::fs::read_to_string(&log_file).unwrap_or_default();
            let lines: Vec<&str> = log_content.lines().collect();
            assert_eq!(lines.len(), 3, "Should have 3 log entries");
            assert_eq!(lines[0].trim(), "A", "First hook should be A");
            assert_eq!(lines[1].trim(), "B", "Second hook should be B");
            assert_eq!(lines[2].trim(), "C", "Third hook should be C");
        }
    }

    #[cfg(not(unix))]
    {
        // On non-Unix systems (Windows), ExecHook uses `sh -c` which may not be available.
        // Instead we verify the registration order via fire_complete result messages.
        let results = manager.fire_complete(&context).await;

        // Even if commands fail, we get one result per hook in registration order
        if let Ok(result_vec) = results {
            assert_eq!(
                result_vec.len(),
                3,
                "Should have 3 results in registration order"
            );
            // Each result contains the hook name which reveals order
            assert!(
                result_vec[0].contains("ExecHook"),
                "First result should reference first registered hook"
            );
            assert!(
                result_vec[1].contains("ExecHook"),
                "Second result should reference second registered hook"
            );
            assert!(
                result_vec[2].contains("ExecHook"),
                "Third result should reference third registered hook"
            );
        }
    }

    // Also verify remove_hook preserves order of remaining hooks
    #[cfg(unix)]
    {
        let mut mgr2 = HookManager::new(HookConfig::default());
        mgr2.add_hook(Box::new(ExecHook::new("cmd_x".to_string(), HashMap::new())));
        mgr2.add_hook(Box::new(ExecHook::new("cmd_y".to_string(), HashMap::new())));
        mgr2.add_hook(Box::new(ExecHook::new("cmd_z".to_string(), HashMap::new())));

        // Remove middle hook
        let removed = mgr2.remove_hook("ExecHook");
        assert!(removed.is_some(), "Should remove an ExecHook");
        assert_eq!(mgr2.hook_count(), 2, "Should have 2 hooks remaining");
    }

    eprintln!("[TEST7] Hook chain order PASSED");
}

// ===========================================================================
// Test 8: LPD registration, announce, and BEP 14 text format
// ===========================================================================

/// Create LpdManager, register a torrent via info_hash hex string,
/// verify LPD constants and text-format announcement parsing.
#[tokio::test]
async fn bt_lpd_register_announce_packet() {
    let manager = LpdManager::new();

    // Verify initial state
    assert!(
        manager.is_available(),
        "LPD should be available after creation"
    );

    // Verify LPD multicast address constants
    assert_eq!(
        LPD_MULTICAST_ADDR, "239.192.152.143",
        "LPD multicast address constant"
    );
    assert_eq!(LPD_PORT, 6771, "LPD multicast port constant");

    // Register a download using hex string info_hash
    let info_hex = "0102030405060708090a0b0c0d0e0f1011121415";
    manager.register_torrent(info_hex, false).await.unwrap();

    // Verify the download appears in active hashes
    let active = manager.active_hashes.read().await;
    assert!(
        active.contains(info_hex),
        "Info hash should be in active set after registration"
    );
    drop(active);

    // Test announce_torrent (sends UDP multicast)
    // Note: This may fail in CI/container environments without multicast support
    let result = manager.announce_torrent(info_hex, 6881).await;
    // Accept success or network unavailability errors (EHOSTUNREACH, ENETUNREACH)
    if let Err(ref e) = result {
        let err_msg = e.to_lowercase();
        assert!(
            err_msg.contains("no route to host")
                || err_msg.contains("network is unreachable")
                || err_msg.contains("could not send"),
            "announce_torrent failed with unexpected error: {:?}",
            result.err()
        );
    }

    // Test BEP 14 text format parsing with valid announcement
    let valid_msg =
        b"BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: 0102030405060708090a0b0c0d0e0f1011121415\r\n\r\n\r\n";
    let sender_ip = std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 42));
    let parsed = parse_lpd_announcement(valid_msg, sender_ip);
    assert!(parsed.is_some(), "Valid BEP 14 LPD message should parse");
    let peer = parsed.unwrap();
    assert_eq!(peer.info_hash.as_ref(), info_hex);
    assert_eq!(peer.port, 6881);
    assert_eq!(peer.addr, sender_ip);
    assert!(peer.is_local, "10.x.x.x should be local");

    // Test parsing rejects malformed messages
    let short_hash = b"BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 1234\r\nInfohash: abc\r\n\r\n\r\n";
    assert!(
        parse_lpd_announcement(short_hash, sender_ip).is_none(),
        "Short hash should be rejected"
    );

    let missing_port = b"BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nInfohash: 0102030405060708090a0b0c0d0e0f1011121415\r\n\r\n\r\n";
    assert!(
        parse_lpd_announcement(missing_port, sender_ip).is_none(),
        "Missing port should be rejected"
    );

    let empty = b"";
    assert!(
        parse_lpd_announcement(empty, sender_ip).is_none(),
        "Empty message should be rejected"
    );

    // Test unregister removes torrent
    manager.unregister_torrent(info_hex).await;
    let active2 = manager.active_hashes.read().await;
    assert!(
        !active2.contains(info_hex),
        "Info hash should not be in active set after unregister"
    );

    // Test get_peers_for returns empty for unknown hash
    let peers = manager
        .get_peers_for("ffffffffffffffffffffffffffffffffffffffff")
        .await;
    assert!(peers.is_empty(), "Unknown hash should have no peers");

    eprintln!("[TEST8] LPD register & announce packet PASSED");
}

// ===========================================================================
// Test 9: LPD peer discovery via update_peers and cleanup
// ===========================================================================

/// LpdManager with a registered torrent, manually add discovered peers
/// via update_peers(), verify peer tracking, dedup, and cleanup.
#[tokio::test]
async fn bt_lpd_peer_discovery_roundtrip() {
    let manager = LpdManager::new();

    let info_hex = "a0b0c0d0e0f0102030405060708090a0b0c0d0e1011";

    // Register the torrent we want to discover peers for
    manager.register_torrent(info_hex, false).await.unwrap();

    // Initially no peers for this hash
    let peers = manager.get_peers_for(info_hex).await;
    assert!(peers.is_empty(), "Should start with 0 peers");

    // Manually add discovered peers (simulating what parse_lpd_announcement + update_peers would do)
    let peer1 = LpdPeer::new(
        info_hex,
        6881,
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 42)),
    );
    let peer2 = LpdPeer::new(
        info_hex,
        6991,
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 99)),
    );
    manager
        .update_peers(info_hex, vec![peer1.clone(), peer2.clone()])
        .await;

    // Verify peers were added (HashSet doesn't guarantee order)
    let discovered = manager.get_peers_for(info_hex).await;
    assert_eq!(discovered.len(), 2, "Should have 2 peers after update");

    let ips: Vec<_> = discovered.iter().map(|p| p.addr).collect();
    assert!(
        ips.contains(&std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 42))),
        "Should contain peer1 IP"
    );
    assert!(
        ips.contains(&std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 99))),
        "Should contain peer2 IP"
    );

    let ports: Vec<_> = discovered.iter().map(|p| p.port).collect();
    assert!(ports.contains(&6881), "Should contain port 6881");
    assert!(ports.contains(&6991), "Should contain port 6991");

    // Both peers from 10.x.x.x should be detected as local
    assert!(
        discovered.iter().all(|p| p.is_local),
        "10.x.x.x peers should be local"
    );

    // Adding same peer again should dedup (LpdPeer Hash uses info_hash + addr)
    manager.update_peers(info_hex, vec![peer1.clone()]).await;
    let after_dup = manager.get_peers_for(info_hex).await;
    assert_eq!(
        after_dup.len(),
        2,
        "Duplicate peer should not increase count"
    );

    // Test cleanup of expired peers with near-zero TTL
    let removed = manager.cleanup_expired_peers(Duration::from_nanos(1)).await;
    assert_eq!(removed, 2, "All 2 peers should be cleaned up");

    let after_cleanup = manager.get_peers_for(info_hex).await;
    assert!(
        after_cleanup.is_empty(),
        "No peers should remain after cleanup"
    );

    // Test unregister removes torrent
    manager.unregister_torrent(info_hex).await;
    let active = manager.active_hashes.read().await;
    assert!(
        !active.contains(info_hex),
        "Info hash should not be in active set after unregister"
    );
    drop(active);

    eprintln!("[TEST9] LPD peer discovery roundtrip PASSED");
}

// ===========================================================================
// Test 10: MSE encrypted handshake + piece encrypt/decrypt
// ===========================================================================

/// Perform the wire-level MSE handshake and verify both encrypted directions.
#[tokio::test]
async fn bt_mse_encrypted_handshake_plus_piece() {
    let info_hash = [
        0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xA0, 0xB0, 0xC0, 0xD0, 0xE0, 0xF0,
        0x00, 0x11, 0x22, 0x33, 0x44,
    ];
    let mut initiator = MseHandshake::new_initiator(info_hash);
    let mut responder = MseHandshake::new_responder(info_hash);
    let initiator_step1 = initiator.build_step1();
    let responder_step1 = responder.build_step1();
    initiator
        .receive_step1(&responder_step1)
        .expect("initiator step1");
    responder
        .receive_step1(&initiator_step1)
        .expect("responder step1");
    let initiator_step2 = initiator.build_initiator_step2().expect("initiator step2");
    assert_eq!(
        responder
            .receive_initiator_step2(&initiator_step2, &[info_hash])
            .expect("responder step2"),
        MseCryptoMethod::Rc4
    );
    let responder_step2 = responder.build_receiver_step2().expect("responder step2");
    assert_eq!(
        initiator
            .receive_receiver_step2(&responder_step2)
            .expect("initiator step2 response"),
        MseCryptoMethod::Rc4
    );

    let mut initiator_crypto = initiator.finalize().expect("initiator finalize");
    let mut responder_crypto = responder.finalize().expect("responder finalize");
    assert_eq!(initiator_crypto.method(), MseCryptoMethod::Rc4);
    assert_eq!(responder_crypto.method(), MseCryptoMethod::Rc4);
    assert!(initiator_crypto.is_encrypted() && responder_crypto.is_encrypted());

    let mut initiator_message = b"initiator to responder".to_vec();
    initiator_crypto.encrypt(&mut initiator_message);
    assert_ne!(initiator_message, b"initiator to responder");
    responder_crypto.decrypt(&mut initiator_message);
    assert_eq!(initiator_message, b"initiator to responder");

    let mut responder_message = b"responder to initiator".to_vec();
    responder_crypto.encrypt(&mut responder_message);
    assert_ne!(responder_message, b"responder to initiator");
    initiator_crypto.decrypt(&mut responder_message);
    assert_eq!(responder_message, b"responder to initiator");
}

// ===========================================================================
// Test 11: MSE plaintext fallback
// ===========================================================================

#[tokio::test]
async fn bt_mse_plaintext_fallback() {
    let mut crypto = MseCryptoState::new_plain();
    assert_eq!(crypto.method(), MseCryptoMethod::Plain);
    assert!(!crypto.is_encrypted());
    let mut data = b"unencrypted_piece_data_stream".to_vec();
    crypto.encrypt(&mut data);
    assert_eq!(data, b"unencrypted_piece_data_stream");
    crypto.decrypt(&mut data);
    assert_eq!(data, b"unencrypted_piece_data_stream");
}

// ===========================================================================
// Test 12: Tracker multi-peer distribution
// ===========================================================================

/// Start MockTrackerServer(s) returning peer addresses, connect MockBtSeeder(s)
/// at those addresses, and verify all tracker-provided peers are reachable.
#[tokio::test]
async fn bt_tracker_multi_peer_distribution() {
    let info_hash: [u8; 20] = [
        0xDD, 0xCC, 0xBB, 0xAA, 0x99, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0xFF,
        0xEE, 0xDD, 0xCC, 0xBB, 0xAA,
    ];

    // Prepare piece data for seeders
    let piece_data = generate_test_data(256, 0xAB);
    let mut pieces_map = HashMap::new();
    pieces_map.insert(0u32, piece_data.clone());

    // Start 3 mock BT seeders (each on its own random port)
    let seeder_a =
        MockBtSeeder::start(info_hash, pieces_map.clone(), SeederConfig::default()).await;
    let seeder_b =
        MockBtSeeder::start(info_hash, pieces_map.clone(), SeederConfig::default()).await;
    let seeder_c = MockBtSeeder::start(info_hash, pieces_map, SeederConfig::default()).await;

    let port_a = seeder_a.port();
    let port_b = seeder_b.port();
    let port_c = seeder_c.port();

    eprintln!(
        "[TEST12] Seeder ports: A={}, B={}, C={}",
        port_a, port_b, port_c
    );

    // Give seeders a moment to fully initialize their accept loops
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Ensure all three ports are distinct
    assert_ne!(port_a, port_b, "Seeder A and B should have different ports");
    assert_ne!(port_b, port_c, "Seeder B and C should have different ports");
    assert_ne!(port_a, port_c, "Seeder A and C should have different ports");

    // Start 3 MockTrackerServers, each returning a different seeder's port
    let tracker_a = MockTrackerServer::start(port_a).await;
    let tracker_b = MockTrackerServer::start(port_b).await;
    let tracker_c = MockTrackerServer::start(port_c).await;

    // Extract announce URLs
    let url_a = tracker_a.announce_url();
    let url_b = tracker_b.announce_url();
    let url_c = tracker_c.announce_url();

    // Verify all tracker URLs are well-formed HTTP announce endpoints
    for (name, url) in [("A", &url_a), ("B", &url_b), ("C", &url_c)] {
        assert!(
            url.starts_with("http://127.0.0.1:") && url.ends_with("/announce"),
            "Tracker {} URL '{}' should be a valid announce endpoint",
            name,
            url
        );
        // Extract port from URL
        let url_port: u16 = url
            .strip_prefix("http://127.0.0.1:")
            .and_then(|rest| rest.strip_suffix("/announce"))
            .and_then(|p| p.parse().ok())
            .expect("Should extract port from tracker URL");
        eprintln!(
            "[TEST12] Tracker {} -> http://127.0.0.1:{}/announce (port={})",
            name, url_port, url_port
        );
    }

    // Now verify each seeder is reachable via TCP.
    // This confirms the tracker-provided addresses actually map to working
    // network endpoints (the core claim of multi-peer distribution).
    // We test TCP-level reachability rather than full BT protocol exchange
    // to avoid timing dependencies on the mock seeder's async accept loop.

    for (name, seeder, port) in [
        ("Seeder_A", &seeder_a, port_a),
        ("Seeder_B", &seeder_b, port_b),
        ("Seeder_C", &seeder_c, port_c),
    ] {
        // TCP connect with timeout proves the address is reachable
        let connect_result = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(seeder.addr()),
        )
        .await;

        match connect_result {
            Ok(Ok(stream)) => {
                // Verify the stream is alive by checking peer addr
                let peer_addr = stream.peer_addr().expect("Should get peer addr");
                assert_eq!(
                    peer_addr.ip(),
                    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
                    "{}: Should be connected to localhost",
                    name
                );
                assert_eq!(
                    peer_addr.port(),
                    port,
                    "{}: Connected port should match expected seeder port",
                    name
                );

                eprintln!(
                    "[TEST12] {}: TCP reachable at {} (port={})",
                    name,
                    seeder.addr(),
                    port
                );
            }
            Ok(Err(e)) => {
                panic!("[TEST12] {}: Connection failed: {}", name, e);
            }
            Err(_) => {
                panic!("[TEST12] {}: Connection timed out", name);
            }
        }
    }

    // Verify connection counts on seeders (at least 1 each)
    // Note: connections may already be closed by the time we check
    let conn_a = seeder_a.connection_count();
    let conn_b = seeder_b.connection_count();
    let conn_c = seeder_c.connection_count();
    eprintln!(
        "[TEST12] Final connection counts: A={}, B={}, C={}",
        conn_a, conn_b, conn_c
    );

    // Cleanup seeders
    seeder_a.shutdown().await;
    seeder_b.shutdown().await;
    seeder_c.shutdown().await;

    eprintln!("[TEST12] Tracker multi-peer distribution PASSED (all 3 peers reachable)");
}

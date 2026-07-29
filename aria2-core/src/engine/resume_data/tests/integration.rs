//! Test Group 6: Integration tests - crash -> restart -> recovery flow

use super::super::ext_trait::ResumeDataExt;
use super::super::types::{ChecksumInfo, RestoreState, ResumeData, UriState};
use super::create_test_dir;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use std::collections::HashMap;
use std::fs;

#[test]
fn test_integration_crash_restart_recovery_flow() {
    // Simulate the complete lifecycle:
    // 1. Download starts, makes progress
    // 2. Process crashes (state saved to disk)
    // 3. Process restarts, loads saved state
    // 4. Validates state and prepares for restoration

    let test_dir = create_test_dir();
    let resume_file = test_dir.join("crash_recovery_test.aria2");

    // --- Phase 1: Simulate active download with progress ---
    let active_download = ResumeData {
        gid: "deadbeefcafebabe".to_string(),
        uris: vec![UriState {
            uri: "http://primary.server/big-release.iso".to_string(),
            tried: true,
            used: true,
            last_result: Some("ok".to_string()),
            speed_bytes_per_sec: Some(8 * 1024 * 1024),
        }],
        total_length: 2147483648,     // 2 GB
        completed_length: 1073741824, // 1 GB (50% done)
        uploaded_length: 0,
        bitfield: vec![],
        num_pieces: None,
        piece_length: None,
        status: "active".to_string(),
        error_message: None,
        last_download_time: 1700010000,
        created_at: 1700009000,
        output_path: Some("/downloads/big-release.iso".to_string()),
        checksum: Some(ChecksumInfo {
            algorithm: "sha-256".to_string(),
            expected: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
        }),
        options: {
            let mut m = HashMap::new();
            m.insert("split".to_string(), "8".to_string());
            m.insert("max-connection-per-server".to_string(), "4".to_string());
            m.insert("dir".to_string(), "/downloads".to_string());
            m
        },
        resume_offset: Some(1073741824),
        bt_info_hash: None,
        bt_saved_metadata_path: None,
    };

    // --- Phase 2: Simulate crash - save state to disk ---
    active_download
        .save_to_file(&resume_file)
        .expect("Crash-save should succeed");
    assert!(
        resume_file.exists(),
        "Resume file must exist after crash-save"
    );

    // --- Phase 3: Simulate restart - load state from disk ---
    let loaded = ResumeData::load_from_file(&resume_file)
        .expect("Load should succeed")
        .expect("Resume data should exist after crash");

    // --- Phase 4: Validate loaded state ---
    assert_eq!(
        loaded.gid, "deadbeefcafebabe",
        "GID must match after crash/restart"
    );
    assert_eq!(
        loaded.completed_length, 1073741824,
        "Progress must be preserved across crash"
    );
    assert_eq!(loaded.status, "active", "Status must be preserved");
    assert_eq!(
        loaded.resume_offset,
        Some(1073741824),
        "Resume offset must allow continuation from where we stopped"
    );

    // --- Phase 5: Prepare for restoration ---
    let validation = loaded.validate_for_restore();
    assert!(
        validation.is_ok(),
        "Saved state must pass validation for restoration: {:?}",
        validation.err()
    );

    // Decompose into restore components
    let (gid, uris, options, restore_state) = loaded.to_restore_components();

    assert_eq!(gid, "deadbeefcafebabe", "Restoration GID must match");
    assert_eq!(uris.len(), 1, "URI must be available for restoration");
    assert!(
        options.contains_key("split"),
        "Options must include split setting"
    );

    // Verify correct restore state variant
    match restore_state {
        RestoreState::HttpFtp {
            resume_offset,
            total_length,
            completed_length,
        } => {
            assert_eq!(resume_offset, 1073741824, "HTTP resume offset must match");
            assert_eq!(total_length, 2147483648, "Total length must match");
            assert_eq!(completed_length, 1073741824, "Completed must match");
        }
        other => panic!("Expected HttpFtp restore state, got: {:?}", other),
    }

    // --- Phase 6: Verify data is ready for restoration ---
    let validation = loaded.validate_for_restore();
    assert!(
        validation.is_ok(),
        "Loaded data should be valid for restoration: {:?}",
        validation.err()
    );

    // Clean up
    let _ = fs::remove_dir_all(&test_dir);

    println!("Integration crash->restart->recovery flow test passed");
}

#[tokio::test]
async fn test_integration_from_request_group_roundtrip() {
    // Test the full pipeline: RequestGroup -> ResumeData -> file -> load -> validate

    let group = RequestGroup::new(
        GroupId::new(0xDEADBEEF),
        vec![
            "http://example.com/test-file.bin".to_string(),
            "http://mirror.example.com/test-file.bin".to_string(),
        ],
        {
            DownloadOptions {
                split: Some(4),
                dir: Some("/downloads".to_string()),
                out: Some("test-file.bin".to_string()),
                checksum: Some((
                    "sha-256".to_string(),
                    "abc123def4567890abcdef1234567890abcdef1234567890abcdef1234567890".to_string(),
                )),
                ..DownloadOptions::default()
            }
        },
    );

    // Simulate some download progress
    group.set_total_length_atomic(104857600); // 100 MB
    group.set_completed_length(52428800); // 50 MB downloaded
    group.set_uploaded_length(0);
    group.set_download_speed_cached(5242880); // 5 MB/s
    group.set_resume_offset(52428800);

    // Extract resume data from the live RequestGroup
    let resume_data: ResumeData = <ResumeData as ResumeDataExt>::from_request_group(&group)
        .expect("Extraction from RequestGroup should succeed");

    // Verify extraction produced valid data
    assert_eq!(
        resume_data.gid,
        group.gid().to_hex_string(),
        "GID should match"
    );
    assert_eq!(
        resume_data.total_length, 104857600,
        "Total length should match"
    );
    assert_eq!(
        resume_data.completed_length, 52428800,
        "Completed length should match"
    );
    assert_eq!(resume_data.uris.len(), 2, "Both URIs should be extracted");
    assert!(
        resume_data.checksum.is_some(),
        "Checksum should be extracted from options"
    );

    // Validate the extracted data
    resume_data
        .validate_for_restore()
        .expect("Extracted data should be valid for restoration");

    // Roundtrip through serialization
    let json = resume_data.serialize().unwrap();
    let restored = ResumeData::deserialize(&json).unwrap();

    assert_eq!(restored.gid, resume_data.gid, "Roundtrip GID should match");
    assert_eq!(
        restored.completed_length, resume_data.completed_length,
        "Roundtrip completed_length should match"
    );

    println!(
        "RequestGroup -> ResumeData roundtrip test passed. GID: {}",
        resume_data.gid
    );
}

#[tokio::test]
async fn test_integration_bt_request_group_extraction() {
    // Test BT-specific extraction from RequestGroup with bitfield

    let group = RequestGroup::new(
        GroupId::new(0xB7C01234),
        vec![
            "magnet:?xt=urn:btih:a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0&dn=TestTorrent"
                .to_string(),
        ],
        {
            DownloadOptions {
                seed_time: Some(3600.0),
                seed_ratio: Some(1.5),
                enable_dht: true,
                ..DownloadOptions::default()
            }
        },
    );

    // Set BT-specific state
    group.set_total_length_atomic(1073741824); // 1 GB
    group.set_completed_length(536870912); // 512 MB
    group.set_uploaded_length(134217728); // 128 MB seeded
    group.set_bt_bitfield(Some(vec![0xFF, 0xFF, 0x00, 0x00]));

    // Extract
    let resume_data: ResumeData = <ResumeData as ResumeDataExt>::from_request_group(&group)
        .expect("BT extraction should succeed");

    // Verify BT detection
    assert!(resume_data.is_bit_torrent(), "Should be detected as BT");
    assert_eq!(resume_data.detect_protocol(), "bt");
    assert_eq!(
        resume_data.bitfield,
        vec![0xFF, 0xFF, 0x00, 0x00],
        "Bitfield should be extracted"
    );
    assert_eq!(
        resume_data.uploaded_length, 134217728,
        "Upload should be tracked"
    );

    // Verify info hash extracted from magnet
    assert!(
        resume_data.bt_info_hash.is_some(),
        "Info hash should be extracted from magnet URI"
    );

    // Verify restore components produce BT variant
    let (_, _, _, restore_state) = resume_data.to_restore_components();
    match restore_state {
        RestoreState::BitTorrent { bitfield, .. } => {
            assert_eq!(bitfield, vec![0xFF, 0xFF, 0x00, 0x00]);
        }
        other => panic!("Expected BitTorrent restore state, got: {:?}", other),
    }

    println!("BT RequestGroup extraction test passed");
}

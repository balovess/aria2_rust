//! Unit Tests for Resume Data system

use super::ext_trait::ResumeDataExt;
use super::types::{ChecksumInfo, RestoreState, ResumeData, UriState};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Helper to create a temporary directory for tests
fn create_test_dir() -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        % 1_000_000_000;
    let dir = std::env::temp_dir().join(format!("resume_test_{}_{}", std::process::id(), ts));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("Failed to create test directory");
    dir
}

/// Helper to create sample ResumeData with realistic values (HTTP download)
fn create_sample_resume_data() -> ResumeData {
    ResumeData {
        gid: "2089b05ecca3d829".to_string(),
        uris: vec![
            UriState {
                uri: "http://example.com/files/ubuntu-22.04-desktop-amd64.iso".to_string(),
                tried: true,
                used: true,
                last_result: Some("ok".to_string()),
                speed_bytes_per_sec: Some(5 * 1024 * 1024), // 5 MB/s
            },
            UriState {
                uri: "http://mirror.example.com/ubuntu-22.04-desktop-amd64.iso".to_string(),
                tried: false,
                used: false,
                last_result: None,
                speed_bytes_per_sec: None,
            },
            UriState {
                uri: "ftp://archive.ubuntu.com/ubuntu-22.04-desktop-amd64.iso".to_string(),
                tried: true,
                used: false,
                last_result: Some("Connection timeout".to_string()),
                speed_bytes_per_sec: None,
            },
        ],
        total_length: 4705785856,     // ~4.38 GB
        completed_length: 2352892928, // ~50% done
        uploaded_length: 0,
        bitfield: vec![],
        num_pieces: None,
        piece_length: None,
        status: "active".to_string(),
        error_message: None,
        last_download_time: 1700000000,
        created_at: 1699999000,
        output_path: Some("/downloads/ubuntu-22.04-desktop-amd64.iso".to_string()),
        checksum: Some(ChecksumInfo {
            algorithm: "sha-256".to_string(),
            expected: "b4517b7c8a...".to_string(), // truncated for brevity
        }),
        options: {
            let mut m = HashMap::new();
            m.insert("split".to_string(), "4".to_string());
            m.insert("dir".to_string(), "/downloads".to_string());
            m
        },
        resume_offset: Some(2352892928),
        bt_info_hash: None,
        bt_saved_metadata_path: None,
    }
}

/// Helper to create sample BT-specific ResumeData
fn create_bt_resume_data() -> ResumeData {
    ResumeData {
        gid: "bt123456789abcdef".to_string(),
        uris: vec![UriState {
            uri: "magnet:?xt=urn:btih:abcdef1234567890abcdef1234567890abc&dn=TestTorrent"
                .to_string(),
            tried: true,
            used: true,
            last_result: Some("ok".to_string()),
            speed_bytes_per_sec: Some(2 * 1024 * 1024),
        }],
        total_length: 1073741824,    // 1 GB
        completed_length: 536870912, // 512 MB (50%)
        uploaded_length: 134217728,  // 128 MB uploaded
        bitfield: vec![0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00], // 50% pieces done
        num_pieces: Some(64),
        piece_length: Some(16777216), // 16 MB per piece
        status: "paused".to_string(),
        error_message: None,
        last_download_time: 1700000100,
        created_at: 1699999100,
        output_path: Some("/downloads/test.torrent".to_string()),
        checksum: None,
        options: {
            let mut m = HashMap::new();
            m.insert("seed-time".to_string(), "3600".to_string());
            m.insert("seed-ratio".to_string(), "1.0".to_string());
            m
        },
        resume_offset: None,
        bt_info_hash: Some("abcdef1234567890abcdef1234567890abcdef12".to_string()),
        bt_saved_metadata_path: Some("/downloads/.cache/test.torrent".to_string()),
    }
}

/// Helper to create Metalink-style ResumeData with multiple mirrors
fn create_metalink_resume_data() -> ResumeData {
    ResumeData {
        gid: "ml98765fedcba4321".to_string(),
        uris: vec![
            UriState {
                uri: "http://mirror1.example.com/large-file.bin".to_string(),
                tried: true,
                used: true,
                last_result: Some("ok".to_string()),
                speed_bytes_per_sec: Some(10 * 1024 * 1024), // 10 MB/s - fastest
            },
            UriState {
                uri: "http://mirror2.example.com/large-file.bin".to_string(),
                tried: true,
                used: false,
                last_result: Some("ok".to_string()),
                speed_bytes_per_sec: Some(3 * 1024 * 1024), // 3 MB/s
            },
            UriState {
                uri: "http://mirror3.example.com/large-file.bin".to_string(),
                tried: true,
                used: false,
                last_result: Some("Connection refused".to_string()),
                speed_bytes_per_sec: None,
            },
            UriState {
                uri: "ftp://backup.example.com/large-file.bin".to_string(),
                tried: false,
                used: false,
                last_result: None,
                speed_bytes_per_sec: None,
            },
        ],
        total_length: 524288000,     // 500 MB
        completed_length: 262144000, // 250 MB (50%)
        uploaded_length: 0,
        bitfield: vec![],
        num_pieces: None,
        piece_length: None,
        status: "active".to_string(),
        error_message: None,
        last_download_time: 1700000200,
        created_at: 1699999200,
        output_path: Some("/downloads/large-file.bin".to_string()),
        checksum: Some(ChecksumInfo {
            algorithm: "sha-1".to_string(),
            expected: "a1b2c3d4e5f6...".to_string(),
        }),
        options: {
            let mut m = HashMap::new();
            m.insert("split".to_string(), "4".to_string());
            m.insert("max-connection-per-server".to_string(), "2".to_string());
            m
        },
        resume_offset: Some(262144000),
        bt_info_hash: None,
        bt_saved_metadata_path: None,
    }
}

// =====================================================================
// Test Group 1: HTTP Save -> Restore Round-trip (5+ tests)
// =====================================================================

#[test]
fn test_http_serialize_deserialize_roundtrip() {
    let original = create_sample_resume_data();

    let json = original.serialize().expect("HTTP serialization failed");
    let restored = ResumeData::deserialize(&json).expect("HTTP deserialization failed");

    // Verify core HTTP fields survive roundtrip
    assert_eq!(restored.gid, original.gid, "GID mismatch");
    assert_eq!(
        restored.total_length, original.total_length,
        "Total length mismatch"
    );
    assert_eq!(
        restored.completed_length, original.completed_length,
        "Completed length mismatch"
    );
    assert_eq!(
        restored.uploaded_length, original.uploaded_length,
        "Upload length mismatch"
    );
    assert_eq!(restored.status, original.status, "Status mismatch");
    assert_eq!(
        restored.resume_offset, original.resume_offset,
        "Resume offset mismatch"
    );
    assert_eq!(
        restored.output_path, original.output_path,
        "Output path mismatch"
    );
    assert_eq!(restored.options, original.options, "Options map mismatch");

    // Verify checksum preserved
    assert_eq!(
        restored
            .checksum
            .as_ref()
            .map(|c| (&c.algorithm, &c.expected)),
        original
            .checksum
            .as_ref()
            .map(|c| (&c.algorithm, &c.expected)),
        "Checksum mismatch"
    );
}

#[test]
fn test_http_resume_offset_preserved() {
    let data = create_sample_resume_data();
    assert_eq!(data.resume_offset, Some(2352892928));

    let json = data.serialize().unwrap();
    let restored = ResumeData::deserialize(&json).unwrap();

    assert_eq!(
        restored.resume_offset,
        Some(2352892928),
        "HTTP resume offset must survive roundtrip"
    );

    // Verify resume offset equals completed_length for normal HTTP downloads
    assert_eq!(
        restored.resume_offset,
        Some(restored.completed_length),
        "Resume offset should equal completed_length for HTTP"
    );
}

#[test]
fn test_http_single_uri_roundtrip() {
    let single = ResumeData {
        gid: "http-single-test".to_string(),
        uris: vec![UriState {
            uri: "http://example.com/single-file.dat".to_string(),
            tried: true,
            used: true,
            last_result: Some("ok".to_string()),
            speed_bytes_per_sec: Some(1048576),
        }],
        total_length: 10485760,
        completed_length: 5242880,
        ..Default::default()
    };

    let json = single.serialize().unwrap();
    let restored = ResumeData::deserialize(&json).unwrap();

    assert_eq!(restored.uris.len(), 1);
    assert_eq!(restored.uris[0].uri, "http://example.com/single-file.dat");
    assert!(
        !restored.is_metalink(),
        "Single URI should not be detected as metalink"
    );
    assert_eq!(restored.detect_protocol(), "http");
}

#[test]
fn test_http_zero_completed_roundtrip() {
    let zero_progress = ResumeData {
        gid: "http-zero-progress".to_string(),
        uris: vec![UriState {
            uri: "http://example.com/not-started.zip".to_string(),
            tried: false,
            used: false,
            last_result: None,
            speed_bytes_per_sec: None,
        }],
        total_length: 1000000,
        completed_length: 0,
        resume_offset: None,
        ..Default::default()
    };

    let json = zero_progress.serialize().unwrap();
    let restored = ResumeData::deserialize(&json).unwrap();

    assert_eq!(restored.completed_length, 0);
    assert_eq!(
        restored.resume_offset, None,
        "Zero progress should have no resume offset"
    );
    assert!((restored.completion_ratio() - 0.0).abs() < f64::EPSILON);
}

#[test]
fn test_http_unknown_total_size_roundtrip() {
    let unknown_size = ResumeData {
        gid: "http-unknown-size".to_string(),
        uris: vec![UriState {
            uri: "http://streaming.example.com/live.m3u8".to_string(),
            tried: true,
            used: true,
            last_result: Some("ok".to_string()),
            speed_bytes_per_sec: Some(500000),
        }],
        total_length: 0, // Unknown size (streaming)
        completed_length: 999999,
        ..Default::default()
    };

    let json = unknown_size.serialize().unwrap();
    let restored = ResumeData::deserialize(&json).unwrap();

    assert_eq!(restored.total_length, 0);
    assert_eq!(
        restored.completion_ratio(),
        0.0,
        "Unknown size should yield 0% ratio"
    );
}

// =====================================================================
// Test Group 2: BT Save -> Restore Round-trip with bitfield (5+ tests)
// =====================================================================

#[test]
fn test_bt_serialize_deserialize_roundtrip() {
    let original = create_bt_resume_data();

    let json = original.serialize().expect("BT serialization failed");
    let restored = ResumeData::deserialize(&json).expect("BT deserialization failed");

    // Verify BT-specific fields survive roundtrip
    assert_eq!(restored.gid, original.gid, "BT GID mismatch");
    assert_eq!(restored.bitfield, original.bitfield, "Bitfield mismatch");
    assert_eq!(
        restored.num_pieces, original.num_pieces,
        "Num pieces mismatch"
    );
    assert_eq!(
        restored.piece_length, original.piece_length,
        "Piece length mismatch"
    );
    assert_eq!(
        restored.bt_info_hash, original.bt_info_hash,
        "BT info hash mismatch"
    );
    assert_eq!(
        restored.bt_saved_metadata_path, original.bt_saved_metadata_path,
        "BT metadata path mismatch"
    );
    assert_eq!(
        restored.uploaded_length, original.uploaded_length,
        "Upload length mismatch"
    );

    // Verify BT detection works
    assert!(
        restored.is_bit_torrent(),
        "Should be detected as BT download"
    );
    assert_eq!(restored.detect_protocol(), "bt");
}

#[test]
fn test_bt_bitfield_exact_preservation() {
    let bt = create_bt_resume_data();
    let expected_bitfield = vec![0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00];

    assert_eq!(
        bt.bitfield, expected_bitfield,
        "Original bitfield should match"
    );

    let json = bt.serialize().unwrap();
    let restored = ResumeData::deserialize(&json).unwrap();

    assert_eq!(
        restored.bitfield, expected_bitfield,
        "Bitfield must be exactly preserved after roundtrip"
    );
    assert_eq!(
        restored.bitfield.len(),
        8,
        "Bitfield length must be preserved"
    );
}

#[test]
fn test_bt_piece_metadata_roundtrip() {
    let bt = create_bt_resume_data();

    let json = bt.serialize().unwrap();
    let restored = ResumeData::deserialize(&json).unwrap();

    assert_eq!(
        restored.num_pieces,
        Some(64),
        "Num pieces (64) must survive roundtrip"
    );
    assert_eq!(
        restored.piece_length,
        Some(16777216),
        "Piece length (16MB) must survive roundtrip"
    );

    // Verify consistency: num_pieces * piece_length should approximate total_length
    if let (Some(np), Some(pl)) = (restored.num_pieces, restored.piece_length) {
        let calc_total = (np as u64) * (pl as u64);
        assert!(
            calc_total >= restored.total_length,
            "Calculated total ({}) should be >= reported total ({})",
            calc_total,
            restored.total_length
        );
    }
}

#[test]
fn test_bt_upload_length_tracking() {
    let bt = create_bt_resume_data();
    assert_eq!(bt.uploaded_length, 134217728); // 128 MB uploaded

    let json = bt.serialize().unwrap();
    let restored = ResumeData::deserialize(&json).unwrap();

    assert_eq!(
        restored.uploaded_length, 134217728,
        "Uploaded length must be tracked separately for BT seeding"
    );
}

#[test]
fn test_bt_no_resume_offset() {
    let bt = create_bt_resume_data();

    // BT downloads should NOT use resume_offset (they use bitfield instead)
    assert_eq!(
        bt.resume_offset, None,
        "BT downloads should have no HTTP-style resume offset"
    );

    let json = bt.serialize().unwrap();
    let restored = ResumeData::deserialize(&json).unwrap();

    assert_eq!(restored.resume_offset, None);
}

#[test]
fn test_bt_magnet_info_hash_extraction() {
    // Test that we can extract info hash from magnet links
    let magnet = "magnet:?xt=urn:btih:abcdef1234567890abcdef1234567890abc&dn=TestFile";
    let hash = ResumeData::extract_info_hash_from_magnet(magnet);

    assert!(hash.is_some(), "Should extract info hash from magnet link");
    assert_eq!(
        hash.unwrap(),
        "abcdef1234567890abcdef1234567890abc",
        "Extracted hash should match"
    );
}

#[test]
fn test_bt_empty_bitfield_is_not_bt() {
    // A download with empty bitfield and no info_hash should NOT be detected as BT
    let not_bt = ResumeData {
        gid: "not-bt-test".to_string(),
        uris: vec![UriState {
            uri: "http://example.com/file.zip".to_string(),
            ..Default::default()
        }],
        bitfield: vec![],
        bt_info_hash: None,
        ..Default::default()
    };

    assert!(
        !not_bt.is_bit_torrent(),
        "Empty bitfield + no info_hash should not be detected as BT"
    );
}

// =====================================================================
// Test Group 3: Metalink Save -> Restore Round-trip (5+ tests)
// =====================================================================

#[test]
fn test_metalink_serialize_deserialize_roundtrip() {
    let original = create_metalink_resume_data();

    let json = original.serialize().expect("Metalink serialization failed");
    let restored = ResumeData::deserialize(&json).expect("Metalink deserialization failed");

    // Verify all mirrors preserved
    assert_eq!(
        restored.uris.len(),
        4,
        "All 4 mirrors must survive roundtrip"
    );

    // Verify Metalink detection
    assert!(
        restored.is_metalink(),
        "Multiple URIs should be detected as metalink"
    );
    assert_eq!(restored.detect_protocol(), "metalink");

    // Verify per-mirror state preservation
    for (orig_u, rest_u) in original.uris.iter().zip(restored.uris.iter()) {
        assert_eq!(orig_u.uri, rest_u.uri, "Mirror URI mismatch");
        assert_eq!(orig_u.tried, rest_u.tried, "Tried flag mismatch");
        assert_eq!(orig_u.used, rest_u.used, "Used flag mismatch");
        assert_eq!(orig_u.last_result, rest_u.last_result, "Result mismatch");
        assert_eq!(
            orig_u.speed_bytes_per_sec, rest_u.speed_bytes_per_sec,
            "Speed mismatch"
        );
    }
}

#[test]
fn test_metalink_mirror_priority_ordering() {
    let ml = create_metalink_resume_data();

    // Convert to restore components and check mirror ordering
    let (_gid, _uris, _options, restore_state) = ml.to_restore_components();

    match restore_state {
        RestoreState::Metalink { mirrors, .. } => {
            // First mirror should be highest priority (working, fastest)
            assert_eq!(
                mirrors[0].uri, "http://mirror1.example.com/large-file.bin",
                "Fastest working mirror should be first"
            );
            assert_eq!(
                mirrors[0].priority_score, 0,
                "Best mirror should have score 0"
            );

            // Failed mirror should have lower priority
            let failed_mirror = mirrors
                .iter()
                .find(|m| m.uri.contains("mirror3"))
                .expect("Failed mirror should be present");
            assert!(
                failed_mirror.priority_score > 10,
                "Failed mirror should have low priority (high score)"
            );

            // Untried backup mirror should be in middle (between working and failed)
            let untried = mirrors
                .iter()
                .find(|m| m.uri.contains("backup"))
                .expect("Untried mirror should be present");
            assert!(
                untried.priority_score < failed_mirror.priority_score,
                "Untried mirror (score={}) should have better priority than failed mirror (score={})",
                untried.priority_score,
                failed_mirror.priority_score
            );
            assert!(
                untried.priority_score > mirrors[0].priority_score,
                "Untried mirror (score={}) should have lower priority than best working mirror (score={})",
                untried.priority_score,
                mirrors[0].priority_score
            );
        }
        _ => panic!("Expected Metalink restore state"),
    }
}

#[test]
fn test_metalink_speed_based_ranking() {
    let ml = create_metalink_resume_data();
    let (_, _, _, restore_state) = ml.to_restore_components();

    match restore_state {
        RestoreState::Metalink { mirrors, .. } => {
            // Mirrors should be sorted by priority (ascending)
            for window in mirrors.windows(2) {
                assert!(
                    window[0].priority_score <= window[1].priority_score,
                    "Mirrors should be sorted by priority ascending: {} (score={}) <= {} (score={})",
                    window[0].uri,
                    window[0].priority_score,
                    window[1].uri,
                    window[1].priority_score
                );
            }
        }
        _ => panic!("Expected Metalink restore state"),
    }
}

#[test]
fn test_metalink_checksum_preserved() {
    let ml = create_metalink_resume_data();
    assert!(ml.checksum.is_some());

    let json = ml.serialize().unwrap();
    let restored = ResumeData::deserialize(&json).unwrap();

    assert_eq!(
        restored.checksum.as_ref().map(|c| c.algorithm.as_str()),
        Some("sha-1"),
        "Metalink checksum algorithm should be preserved"
    );
}

#[test]
fn test_metalink_resume_offset_in_restore_state() {
    let ml = create_metalink_resume_data();
    assert_eq!(ml.resume_offset, Some(262144000));

    let (_, _, _, restore_state) = ml.to_restore_components();

    match restore_state {
        RestoreState::Metalink { resume_offset, .. } => {
            assert_eq!(
                resume_offset,
                Some(262144000),
                "Metalink resume offset must be in restore state"
            );
        }
        _ => panic!("Expected Metalink restore state"),
    }
}

// =====================================================================
// Test Group 4: Edge cases and error handling
// =====================================================================

#[test]
fn test_edge_case_empty_uris() {
    let empty_uris = ResumeData {
        gid: "empty-uri-test".to_string(),
        uris: vec![],
        ..Default::default()
    };

    let json = empty_uris.serialize().unwrap();
    let restored = ResumeData::deserialize(&json).unwrap();

    assert!(
        restored.uris.is_empty(),
        "Empty URI list should be preserved"
    );
    assert!(!restored.is_metalink(), "Empty URIs should not be metalink");
    assert_eq!(restored.detect_protocol(), "unknown");
}

#[test]
fn test_edge_case_zero_length_file() {
    let zero_len = ResumeData {
        gid: "zero-len-test".to_string(),
        uris: vec![UriState {
            uri: "http://example.com/empty-file".to_string(),
            ..Default::default()
        }],
        total_length: 0,
        completed_length: 0,
        status: "complete".to_string(),
        ..Default::default()
    };

    let json = zero_len.serialize().unwrap();
    let restored = ResumeData::deserialize(&json).unwrap();

    assert_eq!(restored.total_length, 0);
    assert_eq!(restored.completed_length, 0);
    assert_eq!(restored.status, "complete");
    assert_eq!(restored.completion_ratio(), 0.0);
}

#[test]
fn test_validate_good_data_passes() {
    let good = create_sample_resume_data();
    let result = good.validate_for_restore();
    assert!(
        result.is_ok(),
        "Valid data should pass validation: {:?}",
        result.err()
    );
}

#[test]
fn test_validate_empty_gid_fails() {
    let bad = ResumeData {
        gid: String::new(),
        uris: vec![UriState {
            uri: "http://example.com/f".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let result = bad.validate_for_restore();
    assert!(result.is_err(), "Empty GID should fail validation");
    assert!(
        result.unwrap_err().contains("GID"),
        "Error should mention GID"
    );
}

#[test]
fn test_validate_no_uris_fails() {
    let bad = ResumeData {
        gid: "has-gid-but-no-uris".to_string(),
        uris: vec![],
        ..Default::default()
    };
    let result = bad.validate_for_restore();
    assert!(result.is_err(), "No URIs should fail validation");
    assert!(
        result.unwrap_err().contains("URI"),
        "Error should mention URI"
    );
}

#[test]
fn test_validate_completed_exceeds_total_fails() {
    let bad = ResumeData {
        gid: "overflow-test".to_string(),
        uris: vec![UriState {
            uri: "http://example.com/f".to_string(),
            ..Default::default()
        }],
        total_length: 1000,
        completed_length: 2000, // Exceeds total!
        ..Default::default()
    };
    let result = bad.validate_for_restore();
    assert!(result.is_err(), "Completed > total should fail validation");
    assert!(
        result.unwrap_err().contains("exceeds"),
        "Error should mention overflow"
    );
}

#[test]
fn test_validate_invalid_status_fails() {
    let bad = ResumeData {
        gid: "bad-status-test".to_string(),
        uris: vec![UriState {
            uri: "http://example.com/f".to_string(),
            ..Default::default()
        }],
        status: "invalid_status_xyz".to_string(),
        ..Default::default()
    };
    let result = bad.validate_for_restore();
    assert!(result.is_err(), "Invalid status should fail validation");
    assert!(
        result.unwrap_err().contains("Unknown status"),
        "Error should mention unknown status"
    );
}

#[test]
fn test_detect_protocol_variants() {
    // HTTP
    let http = ResumeData {
        gid: "1".to_string(),
        uris: vec![UriState {
            uri: "https://secure.example.com/f".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(http.detect_protocol(), "http");

    // FTP
    let ftp = ResumeData {
        gid: "2".to_string(),
        uris: vec![UriState {
            uri: "sftp://server/file".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(ftp.detect_protocol(), "ftp");

    // BT via info_hash
    let bt = ResumeData {
        gid: "3".to_string(),
        uris: vec![UriState {
            uri: "http://tracker.example.com/f".to_string(),
            ..Default::default()
        }],
        bt_info_hash: Some("abcd1234".to_string()),
        ..Default::default()
    };
    assert_eq!(bt.detect_protocol(), "bt");

    // Unknown
    let unknown = ResumeData {
        gid: "4".to_string(),
        uris: vec![],
        ..Default::default()
    };
    assert_eq!(unknown.detect_protocol(), "unknown");
}

// =====================================================================
// Test Group 5: Existing tests
// =====================================================================

#[test]
fn test_resume_data_serialize_deserialize_full_roundtrip() {
    let original = create_sample_resume_data();

    // Serialize to JSON
    let json = original.serialize().expect("Serialization failed");

    // Verify JSON contains key fields
    assert!(json.contains("2089b05ecca3d829"), "JSON should contain GID");
    assert!(json.contains("active"), "JSON should contain status");
    assert!(
        json.contains("4705785856"),
        "JSON should contain total_length"
    );
    assert!(
        json.contains("ubuntu-22.04-desktop-amd64.iso"),
        "JSON should contain filename"
    );

    // Deserialize back
    let restored = ResumeData::deserialize(&json).expect("Deserialization failed");

    // Verify all fields match exactly
    assert_eq!(restored.gid, original.gid, "GID mismatch");
    assert_eq!(
        restored.uris.len(),
        original.uris.len(),
        "URI count mismatch"
    );
    assert_eq!(
        restored.total_length, original.total_length,
        "Total length mismatch"
    );
    assert_eq!(
        restored.completed_length, original.completed_length,
        "Completed length mismatch"
    );
    assert_eq!(restored.status, original.status, "Status mismatch");
    assert_eq!(
        restored.error_message, original.error_message,
        "Error message mismatch"
    );
    assert_eq!(
        restored.last_download_time, original.last_download_time,
        "Timestamp mismatch"
    );
    assert_eq!(
        restored.created_at, original.created_at,
        "Created at mismatch"
    );
    assert_eq!(
        restored.output_path, original.output_path,
        "Output path mismatch"
    );

    println!("Full roundtrip test passed. JSON:\n{}", json);
}

#[test]
fn test_resume_data_save_load_file() {
    let test_dir = create_test_dir();
    let file_path = test_dir.join("test_download.aria2");
    let original = create_sample_resume_data();

    // Save to file
    original.save_to_file(&file_path).expect("Save failed");

    // Verify file exists
    assert!(file_path.exists(), "Resume file should exist after save");

    // Load from file
    let loaded = ResumeData::load_from_file(&file_path)
        .expect("Load failed")
        .expect("Should have returned Some(data)");

    // Verify data integrity
    assert_eq!(
        loaded.gid, original.gid,
        "GID mismatch after file roundtrip"
    );
    assert_eq!(loaded.uris.len(), original.uris.len(), "URI count mismatch");
    assert_eq!(
        loaded.total_length, original.total_length,
        "Total length mismatch"
    );
    assert_eq!(
        loaded.completed_length, original.completed_length,
        "Completed length mismatch"
    );
    assert_eq!(loaded.status, original.status, "Status mismatch");
    assert_eq!(
        loaded.output_path, original.output_path,
        "Output path mismatch"
    );

    // Verify no temp file left behind
    let tmp_path = file_path.with_extension("aria2.tmp");
    assert!(!tmp_path.exists(), "No temporary file should remain");

    // Clean up
    let _ = fs::remove_dir_all(&test_dir);

    println!("File save/load test passed");
}

#[test]
fn test_resume_data_missing_file_returns_none() {
    let test_dir = create_test_dir();
    let nonexistent_path = test_dir.join("nonexistent.aria2");

    // Should return Ok(None), not error
    let result = ResumeData::load_from_file(&nonexistent_path)
        .expect("Missing file should not return error");

    assert!(result.is_none(), "Should return None for non-existent file");

    // Clean up
    let _ = fs::remove_dir_all(&test_dir);

    println!("Missing file test passed");
}

#[test]
fn test_resume_data_corrupt_json_returns_error() {
    let test_dir = create_test_dir();
    let file_path = test_dir.join("corrupt.aria2");

    // Test case 1: Completely invalid content
    fs::write(&file_path, "This is not JSON at all! @#$%^&*()")
        .expect("Failed to write corrupt file");
    let result = ResumeData::load_from_file(&file_path);
    assert!(result.is_err(), "Corrupt JSON should return error");
    assert!(
        result.unwrap_err().contains("Failed to deserialize"),
        "Error should mention deserialization"
    );

    // Test case 2: Truncated JSON
    fs::write(&file_path, "{\"gid\":\"test\",\"uris\":[]").expect("Failed to write truncated JSON");
    let result = ResumeData::load_from_file(&file_path);
    assert!(result.is_err(), "Truncated JSON should return error");

    // Test case 3: Valid JSON but wrong structure (missing required fields)
    fs::write(&file_path, "{\"wrong_field\": 123}").expect("Failed to write invalid structure");
    let result = ResumeData::load_from_file(&file_path);
    assert!(result.is_err(), "Invalid structure should return error");

    // Test case 4: Empty file
    fs::write(&file_path, "").expect("Failed to write empty file");
    let result = ResumeData::load_from_file(&file_path);
    assert!(result.is_err(), "Empty file should return error");

    // Clean up
    let _ = fs::remove_dir_all(&test_dir);

    println!("Corrupt JSON handling test passed");
}

#[test]
fn test_resume_data_bt_fields_optional() {
    // Create HTTP download (no BT fields)
    let http_data = create_sample_resume_data();

    assert!(
        !http_data.is_bit_torrent(),
        "HTTP download should not be detected as BT"
    );
    assert!(
        http_data.bt_info_hash.is_none(),
        "HTTP download should have no BT hash"
    );
    assert!(
        http_data.bt_saved_metadata_path.is_none(),
        "HTTP download should have no BT metadata"
    );
    assert!(
        http_data.bitfield.is_empty(),
        "HTTP download should have empty bitfield"
    );

    // Create BT download (with BT fields)
    let bt_data = create_bt_resume_data();

    assert!(
        bt_data.is_bit_torrent(),
        "BT download should be detected as BT"
    );
    assert!(
        bt_data.bt_info_hash.is_some(),
        "BT download should have info hash"
    );
    assert!(
        bt_data.bt_saved_metadata_path.is_some(),
        "BT download should have metadata path"
    );
    assert!(
        !bt_data.bitfield.is_empty(),
        "BT download should have bitfield"
    );

    // Roundtrip BT data to ensure BT fields persist
    let json = bt_data.serialize().expect("BT serialization failed");
    let restored_bt = ResumeData::deserialize(&json).expect("BT deserialization failed");

    assert_eq!(
        restored_bt.bt_info_hash, bt_data.bt_info_hash,
        "BT hash should survive roundtrip"
    );
    assert_eq!(
        restored_bt.bitfield, bt_data.bitfield,
        "Bitfield should survive roundtrip"
    );
    assert_eq!(
        restored_bt.bt_saved_metadata_path, bt_data.bt_saved_metadata_path,
        "Metadata path should survive roundtrip"
    );

    println!("BT optional fields test passed");
}

#[test]
fn test_resume_data_multiple_uris_preserved() {
    let data = create_sample_resume_data();
    assert_eq!(data.uris.len(), 3, "Sample data should have 3 URIs");

    // Roundtrip through serialization
    let json = data.serialize().expect("Serialize failed");
    let restored = ResumeData::deserialize(&json).expect("Deserialize failed");

    // Verify exact URI count
    assert_eq!(
        restored.uris.len(),
        3,
        "Should preserve 3 URIs after roundtrip"
    );

    // Verify each URI's complete state
    // URI 1: Active, working
    assert_eq!(
        restored.uris[0].uri,
        "http://example.com/files/ubuntu-22.04-desktop-amd64.iso"
    );
    assert!(restored.uris[0].tried, "URI 1 should be marked as tried");
    assert!(restored.uris[0].used, "URI 1 should be marked as used");
    assert_eq!(restored.uris[0].last_result.as_deref(), Some("ok"));
    assert_eq!(restored.uris[0].speed_bytes_per_sec, Some(5 * 1024 * 1024));

    // URI 2: Unused mirror
    assert_eq!(
        restored.uris[1].uri,
        "http://mirror.example.com/ubuntu-22.04-desktop-amd64.iso"
    );
    assert!(
        !restored.uris[1].tried,
        "URI 2 should NOT be marked as tried"
    );
    assert!(!restored.uris[1].used, "URI 2 should NOT be marked as used");
    assert!(
        restored.uris[1].last_result.is_none(),
        "URI 2 should have no result"
    );
    assert!(
        restored.uris[1].speed_bytes_per_sec.is_none(),
        "URI 2 should have no speed"
    );

    // URI 3: Failed attempt
    assert_eq!(
        restored.uris[2].uri,
        "ftp://archive.ubuntu.com/ubuntu-22.04-desktop-amd64.iso"
    );
    assert!(restored.uris[2].tried, "URI 3 should be marked as tried");
    assert!(!restored.uris[2].used, "URI 3 should NOT be marked as used");
    assert_eq!(
        restored.uris[2].last_result.as_deref(),
        Some("Connection timeout")
    );
    assert!(
        restored.uris[2].speed_bytes_per_sec.is_none(),
        "URI 3 should have no speed"
    );

    // Test edge case: Single URI
    let single_uri = ResumeData {
        gid: "single-uri-test".to_string(),
        uris: vec![UriState {
            uri: "http://example.com/single.file".to_string(),
            tried: true,
            used: true,
            last_result: Some("ok".to_string()),
            speed_bytes_per_sec: Some(1000),
        }],
        ..Default::default()
    };

    let single_json = single_uri.serialize().unwrap();
    let single_restored = ResumeData::deserialize(&single_json).unwrap();
    assert_eq!(
        single_restored.uris.len(),
        1,
        "Single URI should be preserved"
    );
    assert_eq!(
        single_restored.uris[0].uri,
        "http://example.com/single.file"
    );

    // Test edge case: Empty URI list
    let no_uris = ResumeData {
        gid: "no-uris-test".to_string(),
        uris: vec![],
        ..Default::default()
    };

    let no_uris_json = no_uris.serialize().unwrap();
    let no_uris_restored = ResumeData::deserialize(&no_uris_json).unwrap();
    assert!(
        no_uris_restored.uris.is_empty(),
        "Empty URI list should be preserved"
    );

    println!("Multiple URIs preservation test passed");
}

#[test]
fn test_completion_ratio_calculation() {
    // Normal case: 50% complete
    let data = ResumeData {
        total_length: 1000,
        completed_length: 500,
        ..Default::default()
    };
    assert!(
        (data.completion_ratio() - 0.5).abs() < f64::EPSILON,
        "50% should be 0.5"
    );

    // Edge case: 0% complete
    let zero = ResumeData {
        total_length: 1000,
        completed_length: 0,
        ..Default::default()
    };
    assert_eq!(zero.completion_ratio(), 0.0, "0 bytes should be 0%");

    // Edge case: 100% complete
    let full = ResumeData {
        total_length: 1000,
        completed_length: 1000,
        ..Default::default()
    };
    assert!(
        (full.completion_ratio() - 1.0).abs() < f64::EPSILON,
        "100% should be 1.0"
    );

    // Edge case: Unknown total size (should return 0.0)
    let unknown = ResumeData {
        total_length: 0,
        completed_length: 500,
        ..Default::default()
    };
    assert_eq!(unknown.completion_ratio(), 0.0, "Unknown size should be 0%");
}

#[test]
fn test_get_filename_generation() {
    let data = ResumeData {
        gid: "abc123".to_string(),
        ..Default::default()
    };
    assert_eq!(data.get_filename(), "abc123.aria2");

    let data2 = ResumeData {
        gid: "long-gid-with-dashes_and_underscores".to_string(),
        ..Default::default()
    };
    assert_eq!(
        data2.get_filename(),
        "long-gid-with-dashes_and_underscores.aria2"
    );
}

#[test]
fn test_default_values() {
    let data = ResumeData::default();

    assert!(data.gid.is_empty(), "Default GID should be empty");
    assert!(data.uris.is_empty(), "Default URIs should be empty");
    assert_eq!(data.total_length, 0, "Default total_length should be 0");
    assert_eq!(
        data.completed_length, 0,
        "Default completed_length should be 0"
    );
    assert_eq!(
        data.uploaded_length, 0,
        "Default uploaded_length should be 0"
    );
    assert!(data.bitfield.is_empty(), "Default bitfield should be empty");
    assert_eq!(data.status, "waiting", "Default status should be 'waiting'");
    assert!(data.error_message.is_none(), "Default error should be None");
    assert_eq!(data.last_download_time, 0, "Default timestamp should be 0");
    assert_eq!(data.created_at, 0, "Default created_at should be 0");
    assert!(
        data.output_path.is_none(),
        "Default output_path should be None"
    );
    assert!(data.checksum.is_none(), "Default checksum should be None");
    assert!(data.options.is_empty(), "Default options should be empty");
    assert!(
        data.resume_offset.is_none(),
        "Default resume_offset should be None"
    );
    assert!(
        data.bt_info_hash.is_none(),
        "Default bt_info_hash should be None"
    );
    assert!(
        data.bt_saved_metadata_path.is_none(),
        "Default bt_metadata should be None"
    );
    assert!(
        data.num_pieces.is_none(),
        "Default num_pieces should be None"
    );
    assert!(
        data.piece_length.is_none(),
        "Default piece_length should be None"
    );
}

// =====================================================================
// Test Group 6: Integration test - crash -> restart -> recovery flow
// =====================================================================

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

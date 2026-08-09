//! Unit Tests for Resume Data system

mod bt_roundtrip;
mod edge_cases;
mod http_roundtrip;
mod integration;
mod metalink_roundtrip;
mod serialization;

use super::types::{ChecksumInfo, ResumeData, UriState};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Helper to create a temporary directory for tests
pub(super) fn create_test_dir() -> PathBuf {
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
pub(super) fn create_sample_resume_data() -> ResumeData {
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
        metalink_data: None,
        metalink_file_index: None,
    }
}

/// Helper to create sample BT-specific ResumeData
pub(super) fn create_bt_resume_data() -> ResumeData {
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
        metalink_data: None,
        metalink_file_index: None,
    }
}

/// Helper to create Metalink-style ResumeData with multiple mirrors
pub(super) fn create_metalink_resume_data() -> ResumeData {
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
        metalink_data: None,
        metalink_file_index: None,
    }
}

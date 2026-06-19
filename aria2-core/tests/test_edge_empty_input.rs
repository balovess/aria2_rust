//! Edge case tests for empty inputs.
//!
//! Tests that empty inputs are handled gracefully without panics.

mod fixtures;

use aria2_core::engine::bt_download_command::BtDownloadCommand;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::engine::metalink_download_command::MetalinkDownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::validation::protocol_detector::detect;
use aria2_core::validation::uri::validate;

// =============================================================================
// Empty URI Tests
// =============================================================================

#[test]
fn test_empty_uri_validation() {
    let result = validate("");
    assert!(result.is_err(), "Empty URI should return error");
}

#[test]
fn test_whitespace_only_uri_validation() {
    let result = validate("   ");
    assert!(result.is_err(), "Whitespace-only URI should return error");
}

#[test]
fn test_empty_uri_protocol_detection() {
    let result = detect("");
    assert!(result.is_err(), "Empty input detection should return error");
}

#[test]
fn test_whitespace_only_protocol_detection() {
    let result = detect("   \t\n");
    assert!(
        result.is_err(),
        "Whitespace-only input detection should return error"
    );
}

#[test]
fn test_empty_uri_download_command_creation() {
    // DownloadCommand::new does not validate URI at creation time
    // It validates during execute()
    let result = DownloadCommand::new(
        GroupId::new(1),
        "",
        &DownloadOptions::default(),
        None,
        None,
    );
    // Creation succeeds, validation happens during execute()
    assert!(
        result.is_ok(),
        "DownloadCommand::new accepts empty URI (validation happens during execute)"
    );
}

#[test]
fn test_whitespace_uri_download_command_creation() {
    // DownloadCommand::new does not validate URI at creation time
    let result = DownloadCommand::new(
        GroupId::new(2),
        "   ",
        &DownloadOptions::default(),
        None,
        None,
    );
    // Creation succeeds, validation happens during execute()
    assert!(
        result.is_ok(),
        "DownloadCommand::new accepts whitespace URI (validation happens during execute)"
    );
}

#[tokio::test]
async fn test_empty_uri_download_command_execute_returns_error() {
    let mut cmd = DownloadCommand::new(
        GroupId::new(3),
        "",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new should succeed");

    let result = cmd.execute().await;
    assert!(
        result.is_err(),
        "execute() with empty URI should return error"
    );
}

#[tokio::test]
async fn test_whitespace_uri_download_command_execute_returns_error() {
    let mut cmd = DownloadCommand::new(
        GroupId::new(4),
        "   ",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new should succeed");

    let result = cmd.execute().await;
    assert!(
        result.is_err(),
        "execute() with whitespace URI should return error"
    );
}

// =============================================================================
// Empty Torrent File Tests
// =============================================================================

#[test]
fn test_empty_torrent_file_detection() {
    let dir = tempfile::tempdir().unwrap();
    let empty_torrent_path = dir.path().join("empty.torrent");
    std::fs::write(&empty_torrent_path, b"").unwrap();

    let result = detect(empty_torrent_path.to_str().unwrap());
    assert!(
        result.is_err(),
        "Empty torrent file detection should return error"
    );
}

#[test]
fn test_empty_torrent_file_command_creation() {
    let empty_torrent: Vec<u8> = Vec::new();
    let result = BtDownloadCommand::new(
        GroupId::new(10),
        &empty_torrent,
        &DownloadOptions::default(),
        None,
    );
    assert!(
        result.is_err(),
        "BtDownloadCommand with empty torrent should return error"
    );
}

#[tokio::test]
async fn test_empty_torrent_file_no_panic() {
    let empty_torrent: Vec<u8> = Vec::new();
    let result = BtDownloadCommand::new(
        GroupId::new(11),
        &empty_torrent,
        &DownloadOptions::default(),
        None,
    );
    // Should return error, not panic
    assert!(result.is_err());
}

// =============================================================================
// Empty Metalink File Tests
// =============================================================================

#[test]
fn test_empty_metalink_file_detection() {
    let dir = tempfile::tempdir().unwrap();
    let empty_metalink_path = dir.path().join("empty.metalink");
    std::fs::write(&empty_metalink_path, b"").unwrap();

    let result = detect(empty_metalink_path.to_str().unwrap());
    assert!(
        result.is_err(),
        "Empty metalink file detection should return error"
    );
}

#[test]
fn test_empty_metalink_file_command_creation() {
    let empty_metalink: Vec<u8> = Vec::new();
    let result = MetalinkDownloadCommand::new(
        GroupId::new(20),
        &empty_metalink,
        &DownloadOptions::default(),
        None,
    );
    assert!(
        result.is_err(),
        "MetalinkDownloadCommand with empty metalink should return error"
    );
}

#[test]
fn test_empty_meta4_file_detection() {
    let dir = tempfile::tempdir().unwrap();
    let empty_meta4_path = dir.path().join("empty.meta4");
    std::fs::write(&empty_meta4_path, b"").unwrap();

    let result = detect(empty_meta4_path.to_str().unwrap());
    assert!(
        result.is_err(),
        "Empty .meta4 file detection should return error"
    );
}

#[tokio::test]
async fn test_empty_metalink_file_no_panic() {
    let empty_metalink: Vec<u8> = Vec::new();
    let result = MetalinkDownloadCommand::new(
        GroupId::new(21),
        &empty_metalink,
        &DownloadOptions::default(),
        None,
    );
    // Should return error, not panic
    assert!(result.is_err());
}

// =============================================================================
// Empty Magnet Link Tests
// =============================================================================

#[test]
fn test_empty_magnet_link_validation() {
    let result = validate("magnet:?");
    // magnet:? is technically valid but empty, should handle gracefully
    // The behavior depends on implementation - it may be valid or invalid
    // Just ensure no panic
    let _ = result;
}

#[test]
fn test_magnet_link_with_empty_xt() {
    // Magnet link with empty xt parameter
    let result = detect("magnet:?xt=");
    // Should handle gracefully without panic
    let _ = result;
}

// =============================================================================
// Edge Cases: Null Bytes and Control Characters
// =============================================================================

#[test]
fn test_uri_with_null_byte() {
    let result = validate("http://example.com/file\0.txt");
    // Should handle gracefully - may be error or sanitized
    let _ = result;
}

#[test]
fn test_uri_with_control_characters() {
    let result = validate("http://example.com/\x01\x02\x03file.txt");
    let _ = result;
}

// =============================================================================
// Edge Cases: Very Long Empty-ish Inputs
// =============================================================================

#[test]
fn test_very_long_whitespace_uri() {
    let long_whitespace = " ".repeat(10000);
    let result = validate(&long_whitespace);
    assert!(
        result.is_err(),
        "Very long whitespace URI should return error"
    );
}

#[test]
fn test_very_long_whitespace_detection() {
    let long_whitespace = " \t\n".repeat(10000);
    let result = detect(&long_whitespace);
    assert!(
        result.is_err(),
        "Very long whitespace detection should return error"
    );
}

// =============================================================================
// Edge Cases: Empty File Content Detection
// =============================================================================

#[test]
fn test_torrent_file_with_only_bencode_dict_start() {
    // Just 'd' - incomplete bencode
    let dir = tempfile::tempdir().unwrap();
    let incomplete_path = dir.path().join("incomplete.torrent");
    std::fs::write(&incomplete_path, b"d").unwrap();

    let result = detect(incomplete_path.to_str().unwrap());
    // Should handle gracefully - may error or detect as invalid torrent
    let _ = result;
}

#[test]
fn test_metalink_file_with_only_xml_declaration() {
    // Just XML declaration - incomplete metalink
    let dir = tempfile::tempdir().unwrap();
    let incomplete_path = dir.path().join("incomplete.metalink");
    std::fs::write(&incomplete_path, b"<?xml version=\"1.0\"?>").unwrap();

    let result = detect(incomplete_path.to_str().unwrap());
    // Should handle gracefully
    let _ = result;
}

// =============================================================================
// Edge Cases: Empty Path Components
// =============================================================================

#[test]
fn test_file_uri_with_empty_path() {
    let result = validate("file:///");
    // File URI with empty path - should handle gracefully
    let _ = result;
}

#[test]
fn test_http_uri_with_empty_host() {
    let result = validate("http:///path/to/file");
    // HTTP URI with empty host - should handle gracefully
    let _ = result;
}

// =============================================================================
// Stress Tests: Multiple Empty Inputs
// =============================================================================

#[test]
fn test_multiple_empty_inputs_no_panic() {
    let empty_inputs = vec![
        "",
        "   ",
        "\t\n\r",
        "\u{2000}\u{2001}\u{2002}", // Various Unicode whitespace
    ];

    for input in empty_inputs {
        // Should not panic
        let _ = validate(input);
        let _ = detect(input);
    }
}

#[test]
fn test_empty_torrent_variants_no_panic() {
    let empty_variants: Vec<Vec<u8>> = vec![
        vec![],
        vec![0],
        vec![b'd'], // Just dict start
        vec![b'd', b'e'], // Empty dict
    ];

    for (i, torrent) in empty_variants.iter().enumerate() {
        let result = BtDownloadCommand::new(
            GroupId::new(100 + i as u64),
            torrent,
            &DownloadOptions::default(),
            None,
        );
        // Should return error, not panic
        let _ = result;
    }
}

#[test]
fn test_empty_metalink_variants_no_panic() {
    let empty_variants: Vec<Vec<u8>> = vec![
        vec![],
        vec![b'<'],
        vec![b'<', b'm'],
        b"<?xml?>".to_vec(),
        b"<metalink>".to_vec(),
        b"<metalink></metalink>".to_vec(),
    ];

    for (i, metalink) in empty_variants.iter().enumerate() {
        let result = MetalinkDownloadCommand::new(
            GroupId::new(200 + i as u64),
            metalink,
            &DownloadOptions::default(),
            None,
        );
        // Should return error, not panic
        let _ = result;
    }
}
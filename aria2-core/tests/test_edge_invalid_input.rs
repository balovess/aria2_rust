//! Edge case tests for invalid inputs.
//!
//! Tests that invalid inputs are handled gracefully without crashes.

mod fixtures;

use aria2_core::engine::bt_download_command::BtDownloadCommand;
use aria2_core::engine::metalink_download_command::MetalinkDownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::validation::protocol_detector::detect;
use aria2_core::validation::uri::validate;

// =============================================================================
// Invalid URI Format Tests
// =============================================================================

#[test]
fn test_invalid_uri_no_scheme() {
    let result = validate("example.com/file.zip");
    assert!(result.is_err(), "URI without scheme should return error");
}

#[test]
fn test_invalid_uri_no_path() {
    let result = validate("http://");
    assert!(result.is_err(), "URI without path should return error");
}

#[test]
fn test_invalid_uri_dangerous_scheme_javascript() {
    let result = validate("javascript:alert(1)");
    assert!(
        result.is_err(),
        "JavaScript URI should be rejected as dangerous"
    );
}

#[test]
fn test_invalid_uri_dangerous_scheme_data() {
    let result = validate("data:text/html,<h1>hi</h1>");
    assert!(result.is_err(), "Data URI should be rejected as dangerous");
}

#[test]
fn test_invalid_uri_dangerous_scheme_vbscript() {
    let result = validate("vbscript:msgbox(1)");
    assert!(
        result.is_err(),
        "VBScript URI should be rejected as dangerous"
    );
}

#[test]
fn test_invalid_uri_unsupported_scheme_ssh() {
    let result = validate("ssh://user@host/path");
    assert!(result.is_err(), "SSH URI should be rejected as unsupported");
}

#[test]
fn test_invalid_uri_unsupported_scheme_rsync() {
    let result = validate("rsync://server/path");
    assert!(
        result.is_err(),
        "rsync URI should be rejected as unsupported"
    );
}

#[test]
fn test_invalid_uri_unsupported_scheme_git() {
    let result = validate("git://github.com/repo.git");
    assert!(result.is_err(), "git URI should be rejected as unsupported");
}

#[test]
fn test_invalid_uri_malformed_scheme() {
    let result = validate("://example.com/file");
    assert!(
        result.is_err(),
        "Malformed URI (no scheme before ://) should return error"
    );
}

#[test]
fn test_invalid_uri_double_colon() {
    let result = validate("http:://example.com/file");
    // Should handle gracefully - may error or normalize
    let _ = result;
}

#[test]
fn test_invalid_uri_triple_slash() {
    let result = validate("http:///example.com/file");
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_invalid_uri_with_special_characters() {
    let result = validate("http://exa mple.com/file");
    // Should handle gracefully - may error or sanitize
    let _ = result;
}

#[test]
fn test_invalid_uri_with_unicode() {
    let result = validate("http://例子.测试/file");
    // Should handle gracefully - may work with IDN or error
    let _ = result;
}

#[test]
fn test_invalid_uri_with_newline() {
    let result = validate("http://example.com/file\nmalicious");
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_invalid_uri_with_carriage_return() {
    let result = validate("http://example.com/file\rmalicious");
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_invalid_uri_with_null_byte() {
    let result = validate("http://example.com/file\0.txt");
    // Should handle gracefully
    let _ = result;
}

// =============================================================================
// Corrupted Torrent File Tests
// =============================================================================

#[test]
fn test_corrupted_torrent_random_bytes() {
    let corrupted: Vec<u8> = vec![0x00, 0xFF, 0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56];
    let result = BtDownloadCommand::new(
        GroupId::new(100),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    assert!(
        result.is_err(),
        "Corrupted torrent (random bytes) should return error"
    );
}

#[test]
fn test_corrupted_torrent_partial_bencode() {
    // Incomplete bencode - starts with 'd' but invalid structure
    let corrupted: Vec<u8> = b"d8:announce".to_vec();
    let result = BtDownloadCommand::new(
        GroupId::new(101),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    assert!(
        result.is_err(),
        "Corrupted torrent (partial bencode) should return error"
    );
}

#[test]
fn test_corrupted_torrent_missing_info() {
    // Valid bencode structure but missing required 'info' key
    let corrupted: Vec<u8> = b"d8:announce40:http://tracker.example.com/announcee".to_vec();
    let result = BtDownloadCommand::new(
        GroupId::new(102),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    assert!(
        result.is_err(),
        "Corrupted torrent (missing info) should return error"
    );
}

#[test]
fn test_corrupted_torrent_invalid_announce() {
    // Torrent with invalid announce URL
    let corrupted: Vec<u8> = b"d8:announce15:not-a-valid-urle4:infod6:lengthi1000eee".to_vec();
    let result = BtDownloadCommand::new(
        GroupId::new(103),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    // May or may not error depending on validation strictness
    let _ = result;
}

#[test]
fn test_corrupted_torrent_negative_length() {
    // Torrent with negative file length (invalid)
    let corrupted: Vec<u8> =
        b"d8:announce30:http://tracker.example.com/announce4:infod6:lengthi-1000eee".to_vec();
    let result = BtDownloadCommand::new(
        GroupId::new(104),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_corrupted_torrent_zero_pieces() {
    // Torrent with empty pieces hash
    let corrupted: Vec<u8> = b"d8:announce30:http://tracker.example.com/announce4:infod6:lengthi1000e12:piece lengthi16384e6:pieces0:ee".to_vec();
    let result = BtDownloadCommand::new(
        GroupId::new(105),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    assert!(
        result.is_err(),
        "Corrupted torrent (zero pieces) should return error"
    );
}

#[test]
fn test_corrupted_torrent_truncated() {
    // Truncated torrent file
    let full_torrent = b"d8:announce30:http://tracker.example.com/announce4:infod6:lengthi1000e12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
    let truncated: Vec<u8> = full_torrent[..full_torrent.len() - 10].to_vec();
    let result = BtDownloadCommand::new(
        GroupId::new(106),
        &truncated,
        &DownloadOptions::default(),
        None,
    );
    assert!(result.is_err(), "Truncated torrent should return error");
}

#[test]
fn test_corrupted_torrent_html_content() {
    // File with .torrent extension but HTML content
    let html_content = b"<html><body>Not a torrent</body></html>";
    let dir = tempfile::tempdir().unwrap();
    let fake_torrent_path = dir.path().join("fake.torrent");
    std::fs::write(&fake_torrent_path, html_content).unwrap();

    let result = detect(fake_torrent_path.to_str().unwrap());
    assert!(
        result.is_err(),
        "HTML file with .torrent extension should return error"
    );
}

#[test]
fn test_corrupted_torrent_json_content() {
    // File with .torrent extension but JSON content
    let json_content = b"{\"not\": \"a torrent\"}";
    let dir = tempfile::tempdir().unwrap();
    let fake_torrent_path = dir.path().join("fake.torrent");
    std::fs::write(&fake_torrent_path, json_content).unwrap();

    let result = detect(fake_torrent_path.to_str().unwrap());
    assert!(
        result.is_err(),
        "JSON file with .torrent extension should return error"
    );
}

// =============================================================================
// Corrupted Metalink File Tests
// =============================================================================

#[test]
fn test_corrupted_metalink_random_bytes() {
    let corrupted: Vec<u8> = vec![0x00, 0xFF, 0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56];
    let result = MetalinkDownloadCommand::new(
        GroupId::new(200),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    assert!(
        result.is_err(),
        "Corrupted metalink (random bytes) should return error"
    );
}

#[test]
fn test_corrupted_metalink_malformed_xml() {
    // Malformed XML - unclosed tags
    let corrupted: Vec<u8> =
        b"<?xml version=\"1.0\"?><metalink><files><file name=\"test.bin\">".to_vec();
    let result = MetalinkDownloadCommand::new(
        GroupId::new(201),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    assert!(
        result.is_err(),
        "Corrupted metalink (malformed XML) should return error"
    );
}

#[test]
fn test_corrupted_metalink_missing_files() {
    // Metalink without files element
    let corrupted: Vec<u8> =
        b"<?xml version=\"1.0\"?><metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"></metalink>"
            .to_vec();
    let result = MetalinkDownloadCommand::new(
        GroupId::new(202),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    assert!(
        result.is_err(),
        "Corrupted metalink (missing files) should return error"
    );
}

#[test]
fn test_corrupted_metalink_empty_file_name() {
    // Metalink with empty file name
    let corrupted: Vec<u8> = b"<?xml version=\"1.0\"?><metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"><file name=\"\"><size>100</size><url>http://example.com/file</url></file></metalink>".to_vec();
    let result = MetalinkDownloadCommand::new(
        GroupId::new(203),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_corrupted_metalink_negative_size() {
    // Metalink with negative file size
    let corrupted: Vec<u8> = b"<?xml version=\"1.0\"?><metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"><file name=\"test.bin\"><size>-100</size><url>http://example.com/file</url></file></metalink>".to_vec();
    let result = MetalinkDownloadCommand::new(
        GroupId::new(204),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_corrupted_metalink_invalid_url() {
    // Metalink with invalid URL
    let corrupted: Vec<u8> = b"<?xml version=\"1.0\"?><metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"><file name=\"test.bin\"><size>100</size><url>not-a-valid-url</url></file></metalink>".to_vec();
    let result = MetalinkDownloadCommand::new(
        GroupId::new(205),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_corrupted_metalink_truncated() {
    // Truncated metalink file
    let full_metalink = b"<?xml version=\"1.0\"?><metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"><file name=\"test.bin\"><size>100</size><url>http://example.com/file</url></file></metalink>";
    let truncated: Vec<u8> = full_metalink[..full_metalink.len() - 20].to_vec();
    let result = MetalinkDownloadCommand::new(
        GroupId::new(206),
        &truncated,
        &DownloadOptions::default(),
        None,
    );
    assert!(result.is_err(), "Truncated metalink should return error");
}

#[test]
fn test_corrupted_metalink_wrong_namespace() {
    // Metalink with wrong namespace
    let corrupted: Vec<u8> = b"<?xml version=\"1.0\"?><metalink xmlns=\"http://wrong.namespace.org\"><file name=\"test.bin\"><size>100</size><url>http://example.com/file</url></file></metalink>".to_vec();
    let result = MetalinkDownloadCommand::new(
        GroupId::new(207),
        &corrupted,
        &DownloadOptions::default(),
        None,
    );
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_corrupted_metalink_binary_content() {
    // File with .metalink extension but binary content
    let binary_content: Vec<u8> = (0..255).collect();
    let dir = tempfile::tempdir().unwrap();
    let fake_metalink_path = dir.path().join("fake.metalink");
    std::fs::write(&fake_metalink_path, &binary_content).unwrap();

    let result = detect(fake_metalink_path.to_str().unwrap());
    assert!(
        result.is_err(),
        "Binary file with .metalink extension should return error"
    );
}

#[test]
fn test_corrupted_meta4_file() {
    // Corrupted .meta4 file
    let corrupted: Vec<u8> = b"<not><valid>metalink</not></valid>".to_vec();
    let dir = tempfile::tempdir().unwrap();
    let fake_meta4_path = dir.path().join("fake.meta4");
    std::fs::write(&fake_meta4_path, &corrupted).unwrap();

    let result = detect(fake_meta4_path.to_str().unwrap());
    assert!(result.is_err(), "Corrupted .meta4 file should return error");
}

// =============================================================================
// Invalid Magnet Link Tests
// =============================================================================

#[test]
fn test_invalid_magnet_no_xt() {
    // Magnet link without xt parameter
    let result = detect("magnet:?dn=test");
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_invalid_magnet_empty_xt() {
    // Magnet link with empty xt parameter
    let result = detect("magnet:?xt=");
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_invalid_magnet_malformed_xt() {
    // Magnet link with malformed xt parameter
    let result = detect("magnet:?xt=urn:btih:invalid_hash");
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_invalid_magnet_short_hash() {
    // Magnet link with too short hash
    let result = detect("magnet:?xt=urn:btih:abc");
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_invalid_magnet_wrong_urn() {
    // Magnet link with wrong URN type
    let result = detect("magnet:?xt=urn:sha1:abc123");
    // Should handle gracefully
    let _ = result;
}

// =============================================================================
// Protocol Detection Edge Cases
// =============================================================================

#[test]
fn test_detection_nonexistent_file() {
    let result = detect("/nonexistent/path/to/file.torrent");
    assert!(result.is_err(), "Non-existent file should return error");
}

#[test]
fn test_detection_directory_instead_of_file() {
    let dir = tempfile::tempdir().unwrap();
    let result = detect(dir.path().to_str().unwrap());
    // Should handle gracefully - may error or detect as something
    let _ = result;
}

#[test]
fn test_detection_file_without_extension() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("noextension");
    std::fs::write(&file_path, b"some content").unwrap();

    let result = detect(file_path.to_str().unwrap());
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_detection_file_with_wrong_extension() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("file.txt");
    std::fs::write(&file_path, b"not a torrent or metalink").unwrap();

    let result = detect(file_path.to_str().unwrap());
    // Should handle gracefully
    let _ = result;
}

// =============================================================================
// Stress Tests: Multiple Invalid Inputs
// =============================================================================

#[test]
fn test_multiple_invalid_uris_no_panic() {
    let invalid_uris = vec![
        "",
        "   ",
        "not-a-uri",
        "://no-scheme",
        "http://",
        "ftp://",
        "javascript:void(0)",
        "data:text/plain,hello",
        "ssh://user@host",
        "git://repo",
        "http://\nmalicious",
        "http://\r\nmalicious",
        "http://\x00null",
    ];

    for uri in invalid_uris {
        // Should not panic
        let _ = validate(uri);
        let _ = detect(uri);
    }
}

#[test]
fn test_multiple_corrupted_torrents_no_panic() {
    let corrupted_torrents: Vec<Vec<u8>> = vec![
        vec![],
        vec![0x00],
        vec![0xFF, 0xFF, 0xFF],
        b"d".to_vec(),
        b"de".to_vec(),
        b"d8:announce30:http://tracker.example.com/announcee".to_vec(),
        b"<html>not a torrent</html>".to_vec(),
        b"{\"json\": \"not torrent\"}".to_vec(),
        (0..255).collect(),
    ];

    for (i, torrent) in corrupted_torrents.iter().enumerate() {
        let result = BtDownloadCommand::new(
            GroupId::new(300 + i as u64),
            torrent,
            &DownloadOptions::default(),
            None,
        );
        // Should return error, not panic
        let _ = result;
    }
}

#[test]
fn test_multiple_corrupted_metalinks_no_panic() {
    let corrupted_metalinks: Vec<Vec<u8>> = vec![
        vec![],
        vec![b'<'],
        vec![b'<', b'<', b'<'],
        b"<metalink>".to_vec(),
        b"</metalink>".to_vec(),
        b"<notmetalink></notmetalink>".to_vec(),
        b"<?xml?><root></root>".to_vec(),
        b"{\"json\": \"not metalink\"}".to_vec(),
        (0..255).collect(),
    ];

    for (i, metalink) in corrupted_metalinks.iter().enumerate() {
        let result = MetalinkDownloadCommand::new(
            GroupId::new(400 + i as u64),
            metalink,
            &DownloadOptions::default(),
            None,
        );
        // Should return error, not panic
        let _ = result;
    }
}

// =============================================================================
// Edge Cases: Path Traversal and Security
// =============================================================================

#[test]
fn test_uri_with_path_traversal() {
    let result = validate("http://example.com/../../../etc/passwd");
    // Should handle gracefully - may sanitize or error
    let _ = result;
}

#[test]
fn test_uri_with_double_slashes() {
    let result = validate("http://example.com//path//to//file");
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_uri_with_encoded_path_traversal() {
    let result = validate("http://example.com/%2e%2e/%2e%2e/etc/passwd");
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_uri_with_backslash() {
    let result = validate("http://example.com\\path\\to\\file");
    // Should handle gracefully
    let _ = result;
}

// =============================================================================
// Edge Cases: Very Long Inputs
// =============================================================================

#[test]
fn test_very_long_uri() {
    let long_uri = format!("http://example.com/{}", "a".repeat(10000));
    let result = validate(&long_uri);
    // Should handle gracefully - may error or accept
    let _ = result;
}

#[test]
fn test_very_long_torrent_info() {
    // Create a torrent with very long announce URL
    let long_announce = format!("http://tracker.example.com/{}", "a".repeat(10000));
    let mut torrent = b"d8:announce".to_vec();
    torrent.extend_from_slice(long_announce.len().to_string().as_bytes());
    torrent.push(b':');
    torrent.extend_from_slice(long_announce.as_bytes());
    torrent.extend_from_slice(
        b"4:infod6:lengthi1000e12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee",
    );

    let result = BtDownloadCommand::new(
        GroupId::new(500),
        &torrent,
        &DownloadOptions::default(),
        None,
    );
    // Should handle gracefully
    let _ = result;
}

#[test]
fn test_very_long_metalink_url() {
    let long_url = format!("http://example.com/{}", "a".repeat(10000));
    let metalink = format!(
        r#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="test.bin"><size>100</size><url>{}</url></file></metalink>"#,
        long_url
    );

    let result = MetalinkDownloadCommand::new(
        GroupId::new(501),
        metalink.as_bytes(),
        &DownloadOptions::default(),
        None,
    );
    // Should handle gracefully
    let _ = result;
}

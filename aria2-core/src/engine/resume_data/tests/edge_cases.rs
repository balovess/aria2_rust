//! Test Group 4: Edge cases and error handling

use super::super::types::{ResumeData, UriState};

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
    let good = super::create_sample_resume_data();
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

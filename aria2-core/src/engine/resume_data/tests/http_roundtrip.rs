//! Test Group 1: HTTP Save -> Restore Round-trip

use super::super::types::{ResumeData, UriState};
use super::create_sample_resume_data;

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

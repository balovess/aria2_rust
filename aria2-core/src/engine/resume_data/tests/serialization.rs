//! Test Group 5: Core serialization, file I/O, and utility tests

use super::super::types::{ResumeData, UriState};
use super::{create_bt_resume_data, create_sample_resume_data, create_test_dir};
use std::fs;

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

//! Test Group 2: BT Save -> Restore Round-trip with bitfield

use super::super::types::{ResumeData, UriState};
use super::create_bt_resume_data;

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

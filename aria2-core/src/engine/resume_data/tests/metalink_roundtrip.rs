//! Test Group 3: Metalink Save -> Restore Round-trip

use super::super::ext_trait::ResumeDataExt;
use super::super::types::{ResumeData, RestoreState};
use super::create_metalink_resume_data;

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

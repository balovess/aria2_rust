use super::*;
use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_download_execute::types::PeerKey;
use std::collections::HashSet;

#[test]
fn test_endgame_state_new_is_inactive() {
    let es = EndgameState::new();
    assert!(!es.is_endgame_active());
    assert_eq!(es.tracked_count(), 0);
}

#[test]
fn test_endgame_state_default_is_inactive() {
    let es = EndgameState::default();
    assert!(!es.is_endgame_active());
}

#[test]
fn test_endgame_enter_and_exit() {
    let mut es = EndgameState::new();
    assert!(!es.is_endgame_active());

    es.enter_endgame();
    assert!(es.is_endgame_active());

    // Double enter should be idempotent
    es.enter_endgame();
    assert!(es.is_endgame_active());

    es.exit_endgame();
    assert!(!es.is_endgame_active());
}

#[test]
fn test_endgame_track_request() {
    let mut es = EndgameState::new();
    es.enter_endgame();

    // Track requests from 3 peers for the same block
    es.track_request(0, 0, 16384, 0);
    es.track_request(0, 0, 16384, 1);
    es.track_request(0, 0, 16384, 2);

    assert_eq!(es.tracked_count(), 1); // One unique block tracked

    let targets = es.get_cancel_targets(0, 0, 16384);
    assert_eq!(targets.len(), 3);
    assert!(targets.contains(&PeerKey::from(0)));
    assert!(targets.contains(&PeerKey::from(1)));
    assert!(targets.contains(&PeerKey::from(2)));
}

#[test]
fn test_endgame_cancel_removes_on_arrival() {
    let mut es = EndgameState::new();
    es.enter_endgame();

    es.track_request(5, 0, 16384, 0);
    es.track_request(5, 0, 16384, 1);

    let targets = es.get_cancel_targets(5, 0, 16384);
    assert_eq!(targets.len(), 2);

    // After removal, no more targets
    es.remove_request(5, 0, 16384);
    let targets_after = es.get_cancel_targets(5, 0, 16384);
    assert!(targets_after.is_empty());
    assert_eq!(es.tracked_count(), 0);
}

#[test]
fn test_endgame_multiple_blocks_tracked_independently() {
    let mut es = EndgameState::new();
    es.enter_endgame();

    // Track different blocks
    es.track_request(0, 0, 16384, 0);
    es.track_request(0, 0, 16384, 1);
    es.track_request(0, 16384, 16384, 0);
    es.track_request(0, 16384, 16384, 2);

    assert_eq!(es.tracked_count(), 2);

    // Cancel one block doesn't affect the other
    es.remove_request(0, 0, 16384);
    assert_eq!(es.tracked_count(), 1);

    let remaining = es.get_cancel_targets(0, 16384, 16384);
    assert_eq!(remaining.len(), 2);
    assert!(remaining.contains(&PeerKey::from(0)));
    assert!(remaining.contains(&PeerKey::from(2)));
}

#[test]
fn test_endgame_exit_clears_all_tracking() {
    let mut es = EndgameState::new();
    es.enter_endgame();

    es.track_request(10, 0, 16384, 0);
    es.track_request(10, 0, 16384, 1);
    es.track_request(11, 0, 8192, 0);
    assert_eq!(es.tracked_count(), 2);

    es.exit_endgame();
    assert!(!es.is_endgame_active());
    assert_eq!(es.tracked_count(), 0);
}

#[test]
fn test_endgame_get_cancel_targets_empty_when_inactive() {
    let es = EndgameState::new();
    // Even if we somehow track (shouldn't happen when inactive), targets should be empty
    // Actually tracking works regardless, but is_endgate_active gates usage
    let targets = es.get_cancel_targets(99, 0, 16384);
    assert!(targets.is_empty());
}

#[test]
fn test_endgame_track_different_piece_offsets_lengths() {
    let mut es = EndgameState::new();
    es.enter_endgame();

    // Last block might be shorter
    es.track_request(0, 32768, 8000, 0);
    es.track_request(0, 32768, 8000, 1);

    let targets = es.get_cancel_targets(0, 32768, 8000);
    assert_eq!(targets.len(), 2);
}

#[test]
fn test_endgame_remove_peer_preserves_stable_keys() {
    let mut es = EndgameState::new();
    es.enter_endgame();
    es.track_request(0, 0, 16384, 0);
    es.track_request(0, 0, 16384, 1);
    es.track_request(0, 0, 16384, 2);

    es.remove_peers(&[PeerKey::from(1)]);

    assert_eq!(
        es.get_cancel_targets(0, 0, 16384),
        vec![PeerKey::from(0), PeerKey::from(2)]
    );
}

#[test]
fn test_endgame_remove_last_peer_drops_request() {
    let mut es = EndgameState::new();
    es.enter_endgame();
    es.track_request(0, 0, 16384, 1);

    es.remove_peers(&[PeerKey::from(1)]);

    assert_eq!(es.tracked_count(), 0);
}

#[test]
fn test_endgame_remove_nonexistent_is_noop() {
    let mut es = EndgameState::new();
    es.enter_endgame();

    // Remove something that was never tracked - should not panic
    es.remove_request(999, 999, 999);
    assert_eq!(es.tracked_count(), 0);
}

// ==================== BEP 6 Fast Extension Tests ====================

#[test]
fn test_is_bitfield_set_basic() {
    // Test bitfield: [0b11000000] = pieces 0 and 1 set (MSB first)
    let bf = vec![0xC0];
    assert!(BtDownloadCommand::is_bitfield_set(&bf, 0));
    assert!(BtDownloadCommand::is_bitfield_set(&bf, 1));
    assert!(!BtDownloadCommand::is_bitfield_set(&bf, 2));
    assert!(!BtDownloadCommand::is_bitfield_set(&bf, 7));
}

#[test]
fn test_is_bitfield_set_multi_byte() {
    // Bitfield for 16 pieces: all set
    let bf = vec![0xFF, 0xFF];
    for i in 0..16u32 {
        assert!(
            BtDownloadCommand::is_bitfield_set(&bf, i),
            "Piece {} should be set",
            i
        );
    }
}

#[test]
fn test_is_bitfield_set_out_of_range() {
    let bf = vec![0xFF];
    assert!(!BtDownloadCommand::is_bitfield_set(&bf, 8)); // Beyond bitfield length
    assert!(!BtDownloadCommand::is_bitfield_set(&bf, 100));
}

#[test]
fn test_calculate_fast_set_basic() {
    let needed = vec![0u32, 1, 2, 3, 4, 5];
    let peer_bf = vec![0b11111100]; // Peer has pieces 0-5

    let already_sent = HashSet::new();
    let fast_set = BtDownloadCommand::calculate_fast_set(&needed, &peer_bf, &already_sent);

    assert_eq!(fast_set.len(), 6); // All pieces should be selected (<10 limit)
    assert!(fast_set.contains(&0));
    assert!(fast_set.contains(&5));
}

#[test]
fn test_calculate_fast_set_respects_max_limit() {
    // Create 15 needed pieces
    let needed: Vec<u32> = (0..15).collect();
    let peer_bf = vec![0xFF, 0xFF]; // Peer has first 16 pieces

    let already_sent = HashSet::new();
    let fast_set = BtDownloadCommand::calculate_fast_set(&needed, &peer_bf, &already_sent);

    assert_eq!(fast_set.len(), 10); // Should cap at MAX_ALLOWED_FAST_PER_PEER
}

#[test]
fn test_calculate_fast_set_excludes_already_sent() {
    let needed = vec![0u32, 1, 2, 3, 4];
    let peer_bf = vec![0b11111000];

    let mut already_sent = HashSet::new();
    already_sent.insert(0);
    already_sent.insert(1);

    let fast_set = BtDownloadCommand::calculate_fast_set(&needed, &peer_bf, &already_sent);

    assert_eq!(fast_set.len(), 3); // Only 2,3,4 should be selected
    assert!(!fast_set.contains(&0));
    assert!(!fast_set.contains(&1));
    assert!(fast_set.contains(&2));
}

#[test]
fn test_calculate_fast_set_filters_by_peer_bitfield() {
    let needed = vec![0u32, 1, 2, 3, 4];
    let peer_bf = vec![0b00011000]; // bitfield byte: bits 3 and 4 set (pieces 3,4)

    let already_sent = HashSet::new();
    let fast_set = BtDownloadCommand::calculate_fast_set(&needed, &peer_bf, &already_sent);

    assert_eq!(fast_set.len(), 2); // Only pieces that peer has
    assert!(fast_set.contains(&3));
    assert!(fast_set.contains(&4));
    assert!(!fast_set.contains(&0));
    assert!(!fast_set.contains(&1));
    assert!(!fast_set.contains(&2));
}

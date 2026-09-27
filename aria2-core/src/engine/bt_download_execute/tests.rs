use super::*;
use crate::engine::bt_download_execute::types::PeerKey;

#[test]
fn tracker_tiers_are_deduplicated_without_reordering() {
    let tiers = super::execute::deduplicate_tracker_tiers(vec![
        vec![
            "http://one/announce".to_string(),
            "http://two/announce".to_string(),
        ],
        vec![
            "http://two/announce".to_string(),
            "http://three/announce".to_string(),
        ],
        vec!["http://one/announce".to_string()],
    ]);

    assert_eq!(
        tiers,
        vec![
            vec![
                "http://one/announce".to_string(),
                "http://two/announce".to_string(),
            ],
            vec!["http://three/announce".to_string()],
        ]
    );
}

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
fn test_endgame_request_ownership_can_be_queried_and_removed_per_peer() {
    let mut es = EndgameState::new();
    let first = PeerKey::from(0);
    let second = PeerKey::from(1);
    es.track_request(3, 16_384, 16_384, first);
    es.track_request(3, 16_384, 16_384, second);

    assert!(es.has_peer_request(3, 16_384, 16_384, first));
    es.remove_peer_request(3, 16_384, 16_384, first);
    assert!(!es.has_peer_request(3, 16_384, 16_384, first));
    assert!(es.has_peer_request(3, 16_384, 16_384, second));
    assert_eq!(es.get_cancel_targets(3, 16_384, 16_384), vec![second]);

    es.remove_peer_request(3, 16_384, 16_384, second);
    assert_eq!(es.tracked_count(), 0);
}

#[test]
fn test_endgame_request_ownership_is_unique_and_excludes_winner() {
    let mut es = EndgameState::new();
    es.enter_endgame();
    let winner = PeerKey::from(0);
    es.track_request(0, 0, 16384, winner);
    es.track_request(0, 0, 16384, winner);
    es.track_request(0, 0, 16384, PeerKey::from(1));

    assert_eq!(es.tracked_count(), 1);
    assert_eq!(
        es.take_cancel_targets(0, 0, 16384, winner),
        vec![PeerKey::from(1)]
    );
    assert_eq!(es.tracked_count(), 0);
}

#[test]
fn test_endgame_timeout_cleanup_drops_pending_ownership() {
    let mut es = EndgameState::new();
    es.enter_endgame();
    es.track_request(2, 0, 8192, PeerKey::from(0));
    es.track_request(2, 0, 8192, PeerKey::from(1));
    es.remove_request(2, 0, 8192);
    assert!(es.get_cancel_targets(2, 0, 8192).is_empty());
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

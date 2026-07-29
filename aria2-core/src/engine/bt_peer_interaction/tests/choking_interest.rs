//! Tests for choking/interest decisions, check_have_with_callback,
//! and download_finished flag.

use super::super::BtPeerInteractive;
use super::super::types::*;
use super::make_test_conn;

// ── am_choking / am_interested / peer_choking / peer_interested ─────

#[test]
fn test_initial_choking_interest_state() {
    let info_hash = [0u8; 20];
    let interactive = BtPeerInteractive::new(info_hash, 100);
    // Initial state matches C++ defaults:
    // am_choking = true, am_interested = false
    // peer_choking = true, peer_interested = false
    assert!(interactive.am_choking());
    assert!(!interactive.am_interested());
    assert!(interactive.peer_choking());
    assert!(!interactive.peer_interested());
}

#[test]
fn test_decide_choking_no_change_when_already_choking() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.am_choking = true;

    // Create a minimal BtPeerConn with session resource where
    // should_be_choking() returns true (choking_required=true, opt_unchoking=false)
    let mut conn = make_test_conn();
    conn.allocate_session_resource(256 * 1024, 1024 * 1024);

    // Should be choking and already choking → NoChange
    let decision = interactive.decide_choking(&conn);
    assert_eq!(decision, ChokingDecision::NoChange);
}

#[test]
fn test_decide_choking_choke_when_not_choking() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.am_choking = false;

    let mut conn = make_test_conn();
    conn.allocate_session_resource(256 * 1024, 1024 * 1024);
    // Default: choking_required=true, opt_unchoking=false → should_be_choking=true

    let decision = interactive.decide_choking(&conn);
    assert_eq!(decision, ChokingDecision::Choke);
}

#[test]
fn test_decide_choking_unchoke_when_should_not_choke() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.am_choking = true;

    let mut conn = make_test_conn();
    conn.allocate_session_resource(256 * 1024, 1024 * 1024);
    // Set opt_unchoking → should_be_choking = false
    if let Some(ref mut res) = conn.session_resource {
        res.set_opt_unchoking(true);
    }

    let decision = interactive.decide_choking(&conn);
    assert_eq!(decision, ChokingDecision::Unchoke);
}

#[test]
fn test_decide_choking_no_change_when_already_unchoked() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.am_choking = false;

    let mut conn = make_test_conn();
    conn.allocate_session_resource(256 * 1024, 1024 * 1024);
    if let Some(ref mut res) = conn.session_resource {
        res.set_opt_unchoking(true);
    }

    // Should not be choking and already not choking → NoChange
    let decision = interactive.decide_choking(&conn);
    assert_eq!(decision, ChokingDecision::NoChange);
}

#[test]
fn test_decide_choking_no_resource() {
    let info_hash = [0u8; 20];
    let interactive = BtPeerInteractive::new(info_hash, 100);

    let conn = make_test_conn();
    // No session resource → NoChange
    let decision = interactive.decide_choking(&conn);
    assert_eq!(decision, ChokingDecision::NoChange);
}

// ── decide_interest tests ───────────────────────────────────────────

#[test]
fn test_decide_interest_becomes_interested() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.am_interested = false;

    let mut conn = make_test_conn();
    conn.allocate_session_resource(256 * 1024, 1024 * 1024);

    // has_missing_piece returns true, am_interested is false → Interested
    let decision = interactive.decide_interest_with_callback(&conn, &|_| true);
    assert_eq!(decision, InterestDecision::Interested);
}

#[test]
fn test_decide_interest_becomes_not_interested() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.am_interested = true;

    let conn = make_test_conn();

    // has_missing_piece returns false, am_interested is true → NotInterested
    let decision = interactive.decide_interest_with_callback(&conn, &|_| false);
    assert_eq!(decision, InterestDecision::NotInterested);
}

#[test]
fn test_decide_interest_no_change() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.am_interested = true;

    let conn = make_test_conn();

    // has_missing_piece returns true, am_interested is true → NoChange
    let decision = interactive.decide_interest_with_callback(&conn, &|_| true);
    assert_eq!(decision, InterestDecision::NoChange);
}

#[test]
fn test_decide_interest_legacy_heuristic() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.am_interested = false;

    let mut conn = make_test_conn();
    conn.allocate_session_resource(256 * 1024, 1024 * 1024);

    // Legacy: session_resource.is_some() → should_be_interested = true
    let decision = interactive.decide_interest(&conn);
    assert_eq!(decision, InterestDecision::Interested);
}

// ── check_have_with_callback tests ──────────────────────────────────

#[test]
fn test_check_have_with_callback_returns_pieces() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    assert_eq!(interactive.last_have_index(), 0);

    let pieces = interactive.check_have_with_callback(&|| vec![5, 10, 15]);
    assert_eq!(pieces, vec![5, 10, 15]);
    // last_have_index should be updated to max (15)
    assert_eq!(interactive.last_have_index(), 15);
}

#[test]
fn test_check_have_with_callback_empty() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);

    let pieces = interactive.check_have_with_callback(&|| Vec::new());
    assert!(pieces.is_empty());
    // last_have_index should remain unchanged
    assert_eq!(interactive.last_have_index(), 0);
}

#[test]
fn test_check_have_with_callback_updates_last_have_index() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.set_last_have_index(50);

    // Callback returns pieces with max < current last_have_index
    let _pieces = interactive.check_have_with_callback(&|| vec![3, 7]);
    // last_have_index should stay at 50 (max of 50, 7)
    assert_eq!(interactive.last_have_index(), 50);

    // Now callback returns pieces with max > current
    let _pieces = interactive.check_have_with_callback(&|| vec![60, 70]);
    assert_eq!(interactive.last_have_index(), 70);
}

// ── download_finished flag test ─────────────────────────────────────

#[test]
fn test_download_finished_flag() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    assert!(!interactive.download_finished);
    interactive.set_download_finished(true);
    assert!(interactive.download_finished);
}

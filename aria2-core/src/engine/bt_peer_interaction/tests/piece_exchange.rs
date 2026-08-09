//! Tests for fill_piece, add_requests, cancel_all_piece, remove_completed_piece,
//! endgame flag, request_factory accessors, and MockPieceProvider trait test.

use crate::engine::bt_peer_interaction::piece_provider::PieceProvider;
use crate::engine::bt_peer_interaction::types::*;
use crate::segment::piece::Piece;

use super::super::BtPeerInteractive;
use super::{MockPieceProvider, make_test_conn};

// ── fill_piece tests ────────────────────────────────────────────────

#[test]
fn test_fill_piece_no_missing_pieces() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    let conn = make_test_conn();
    let mut mock = MockPieceProvider::new();
    mock.has_missing = false;

    interactive.fill_piece(&mut mock, &conn, 1);
    assert_eq!(interactive.request_factory().count_target_piece(), 0);
}

#[test]
fn test_fill_piece_adds_piece_when_below_max() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    interactive.peer_choking = false;

    // Add 1 piece with 4 missing blocks, below max_outstanding_request (6)
    interactive
        .request_factory_mut()
        .add_target_piece(Piece::new(0, 65536));

    let conn = make_test_conn();
    let mut mock = MockPieceProvider::new();
    mock.missing_pieces = vec![Piece::new(1, 65536)];

    interactive.fill_piece(&mut mock, &conn, 1);
    // Should add piece 1 because 4 missing blocks < max_outstanding (6)
    assert_eq!(interactive.request_factory().count_target_piece(), 2);
}

#[test]
fn test_fill_piece_adds_pieces_when_not_choking() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    interactive.peer_choking = false; // Not choking us
    let conn = make_test_conn();
    let mut mock = MockPieceProvider::new();
    mock.missing_pieces = vec![Piece::new(0, 65536), Piece::new(1, 65536)];

    interactive.fill_piece(&mut mock, &conn, 1);
    assert_eq!(interactive.request_factory().count_target_piece(), 2);
}

#[test]
fn test_fill_piece_choking_no_fast_extension() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    interactive.peer_choking = true; // Choking us
    let conn = make_test_conn();
    // conn has no session_resource → fast extension disabled

    let mut mock = MockPieceProvider::new();
    mock.missing_pieces = vec![Piece::new(0, 65536)];
    mock.fast_pieces = vec![Piece::new(1, 65536)];

    interactive.fill_piece(&mut mock, &conn, 1);
    // Should not add any pieces because peer is choking and no fast extension
    assert_eq!(interactive.request_factory().count_target_piece(), 0);
}

#[test]
fn test_fill_piece_choking_with_fast_extension() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    interactive.peer_choking = true; // Choking us

    let mut conn = make_test_conn();
    conn.allocate_session_resource(65536, 655360);
    conn.session_resource
        .as_mut()
        .unwrap()
        .set_fast_extension_enabled(true);

    let mut mock = MockPieceProvider::new();
    mock.fast_pieces = vec![Piece::new(0, 65536)];

    interactive.fill_piece(&mut mock, &conn, 1);
    assert_eq!(interactive.request_factory().count_target_piece(), 1);
}

#[test]
fn test_fill_piece_enough_blocks_already() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    interactive.peer_choking = false;

    // Add 2 pieces with 4 blocks each = 8 missing blocks >= max_outstanding (6)
    interactive
        .request_factory_mut()
        .add_target_piece(Piece::new(0, 65536));
    interactive
        .request_factory_mut()
        .add_target_piece(Piece::new(1, 65536));

    let conn = make_test_conn();
    let mut mock = MockPieceProvider::new();
    mock.missing_pieces = vec![Piece::new(2, 65536)];

    interactive.fill_piece(&mut mock, &conn, 1);
    // Should NOT add more pieces (8 missing blocks >= max_outstanding_request=6)
    assert_eq!(interactive.request_factory().count_target_piece(), 2);
}

// ── add_requests tests ──────────────────────────────────────────────

#[test]
fn test_add_requests_enters_endgame() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    interactive.peer_choking = false;

    let conn = make_test_conn();
    let mut mock = MockPieceProvider::new();
    mock.has_missing_unused = false; // Triggers endgame
    mock.missing_pieces = vec![Piece::new(0, 65536)];

    let requests = interactive.add_requests(&mut mock, &conn, 1);

    assert!(interactive.is_endgame());
    assert!(mock.entered_end_game);
    // Should have generated some requests from the piece
    assert!(!requests.is_empty());
}

#[test]
fn test_add_requests_does_not_reenter_endgame() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    interactive.peer_choking = false;
    interactive.endgame = true; // Already in endgame

    let conn = make_test_conn();
    let mut mock = MockPieceProvider::new();
    mock.has_missing_unused = false;

    let _ = interactive.add_requests(&mut mock, &conn, 1);

    // enter_end_game should NOT be called again
    assert!(!mock.entered_end_game);
}

#[test]
fn test_add_requests_no_requests_when_max_outstanding_reached() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    interactive.peer_choking = false;

    // Add pieces and make the handler think we already have max outstanding requests
    // by adding request slots directly
    for i in 0..DEFAULT_MAX_OUTSTANDING_REQUEST {
        interactive
            .handler_mut()
            .dispatcher
            .add_request_slot(i as u32, 0, 16384);
    }

    let conn = make_test_conn();
    let mut mock = MockPieceProvider::new();
    mock.missing_pieces = vec![Piece::new(0, 65536)];

    let requests = interactive.add_requests(&mut mock, &conn, 1);
    // No new requests should be created
    assert!(requests.is_empty());
}

#[test]
fn test_add_requests_creates_requests_for_new_pieces() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    interactive.peer_choking = false;

    let conn = make_test_conn();
    let mut mock = MockPieceProvider::new();
    // Provide a piece with 4 blocks
    mock.missing_pieces = vec![Piece::new(0, 65536)];

    let requests = interactive.add_requests(&mut mock, &conn, 1);

    // Should have created some requests
    assert!(!requests.is_empty());
    // All requests should be for piece 0
    for req in &requests {
        assert_eq!(req.index, 0);
    }
}

// ── cancel_all_piece tests ──────────────────────────────────────────

#[test]
fn test_cancel_all_piece() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    interactive
        .request_factory_mut()
        .add_target_piece(Piece::new(0, 65536));
    interactive
        .request_factory_mut()
        .add_target_piece(Piece::new(1, 65536));

    let removed = interactive.cancel_all_piece();
    assert_eq!(removed, vec![0, 1]);
    assert_eq!(interactive.request_factory().count_target_piece(), 0);
}

#[test]
fn test_cancel_all_piece_empty() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    let removed = interactive.cancel_all_piece();
    assert!(removed.is_empty());
}

// ── remove_completed_piece tests ────────────────────────────────────

#[test]
fn test_remove_completed_piece() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);

    let mut piece0 = Piece::new(0, 65536);
    piece0.set_all_blocks(); // Mark as complete
    interactive.request_factory_mut().add_target_piece(piece0);
    interactive
        .request_factory_mut()
        .add_target_piece(Piece::new(1, 65536));

    let completed = interactive.remove_completed_piece();
    assert_eq!(completed, vec![0]);
    assert_eq!(interactive.request_factory().count_target_piece(), 1);
}

#[test]
fn test_remove_completed_piece_none() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);
    interactive
        .request_factory_mut()
        .add_target_piece(Piece::new(0, 65536));

    let completed = interactive.remove_completed_piece();
    assert!(completed.is_empty());
}

// ── endgame flag tests ──────────────────────────────────────────────

#[test]
fn test_endgame_flag_initially_false() {
    let info_hash = [0u8; 20];
    let interactive = BtPeerInteractive::new(info_hash, 10);
    assert!(!interactive.is_endgame());
}

// ── request_factory accessor tests ──────────────────────────────────

#[test]
fn test_request_factory_accessors() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 10);

    assert_eq!(interactive.request_factory().count_target_piece(), 0);

    interactive
        .request_factory_mut()
        .add_target_piece(Piece::new(0, 65536));
    assert_eq!(interactive.request_factory().count_target_piece(), 1);
}

// ── PieceProvider trait tests ────────────────────────────────────────

#[test]
fn test_mock_piece_provider_basic() {
    let mut mock = MockPieceProvider::new();
    let conn = make_test_conn();

    assert!(mock.has_missing_piece(&conn));
    assert!(mock.has_missing_unused_piece());
    assert!(!mock.is_end_game());

    mock.enter_end_game();
    assert!(mock.entered_end_game);
}

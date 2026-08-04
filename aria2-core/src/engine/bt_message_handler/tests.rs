//! Tests for BT message handler module.

use super::peer_message_handler::BtPeerMessageHandler;
use super::types::*;

// ── Legacy BtMessageHandler tests (preserved) ───────────────────────

#[test]
fn test_block_size_constant() {
    assert_eq!(BLOCK_SIZE, 16384);
    assert_eq!(BLOCK_SIZE, 16 * 1024); // 16 KB
}

#[test]
fn test_constants_are_reasonable() {
    const _: () = {
        assert!(MAX_RETRIES >= 1);
        assert!(MAX_RETRIES <= 10);
        assert!(BLOCK_REQUEST_TIMEOUT_SECS >= 1);
        assert!(BLOCK_REQUEST_TIMEOUT_SECS <= 30);
        assert!(MAX_BLOCK_READ_MESSAGES >= 100);
    };
}

#[test]
fn test_block_download_result_default() {
    let result = BlockDownloadResult {
        success: false,
        data: None,
        peer_index: None,
        bytes_received: 0,
        failed_peers: Vec::new(),
    };
    assert!(!result.success);
    assert!(result.data.is_none());
    assert_eq!(result.bytes_received, 0);
}

// ── BtPeerMessageHandler new field initialization tests ────────────

#[test]
fn test_new_handler_initial_state() {
    let h = BtPeerMessageHandler::new(16384);
    assert!(h.peer_choking);
    assert!(!h.peer_snubbing);
    assert!(!h.peer_interested);
    assert!(h.am_choking);
    assert!(!h.fast_extension_enabled);
    assert!(!h.metadata_get_mode);
    assert!(h.peer_allowed_fast_set.is_empty());
}

#[test]
fn test_with_max_outstanding_initial_state() {
    let h = BtPeerMessageHandler::with_max_outstanding(16384, 10);
    assert_eq!(h.max_outstanding_requests(), 10);
    assert!(h.peer_choking);
    assert!(!h.peer_interested);
    assert!(h.am_choking);
    assert!(!h.fast_extension_enabled);
    assert!(!h.metadata_get_mode);
}

// ── on_have_received tests ─────────────────────────────────────────

#[test]
fn test_on_have_received_updates_bitfield() {
    let mut h = BtPeerMessageHandler::new(16384);
    let updates = h.on_have_received(42, false, false);
    assert_eq!(updates.len(), 1);
    assert!(matches!(
        &updates[0],
        PeerStateUpdate::HavePiece { index } if *index == 42
    ));
}

#[test]
fn test_on_have_received_seeder_download_finished_disconnect() {
    let mut h = BtPeerMessageHandler::new(16384);
    let updates = h.on_have_received(7, true, true);
    assert_eq!(updates.len(), 2);
    assert!(matches!(&updates[0], PeerStateUpdate::HavePiece { index } if *index == 7));
    assert!(matches!(&updates[1], PeerStateUpdate::DisconnectSeeder));
}

#[test]
fn test_on_have_received_seeder_download_not_finished_no_disconnect() {
    let mut h = BtPeerMessageHandler::new(16384);
    let updates = h.on_have_received(7, true, false);
    assert_eq!(updates.len(), 1);
    assert!(matches!(&updates[0], PeerStateUpdate::HavePiece { .. }));
}

// ── on_bitfield_received tests ─────────────────────────────────────

#[test]
fn test_on_bitfield_received_sets_bitfield() {
    let mut h = BtPeerMessageHandler::new(16384);
    let bf = vec![0xFF, 0x00];
    let updates = h.on_bitfield_received(bf.clone(), false, false);
    assert_eq!(updates.len(), 1);
    assert!(matches!(
        &updates[0],
        PeerStateUpdate::SetBitfield { data } if data == &bf
    ));
}

#[test]
fn test_on_bitfield_received_seeder_download_finished_disconnect() {
    let mut h = BtPeerMessageHandler::new(16384);
    let updates = h.on_bitfield_received(vec![0xFF], true, true);
    assert_eq!(updates.len(), 2);
    assert!(matches!(&updates[0], PeerStateUpdate::SetBitfield { .. }));
    assert!(matches!(&updates[1], PeerStateUpdate::DisconnectSeeder));
}

// ── on_interested_received tests ───────────────────────────────────

#[test]
fn test_on_interested_received_sets_flag() {
    let mut h = BtPeerMessageHandler::new(16384);
    assert!(!h.is_peer_interested());
    // am_choking is true by default, so ExecuteChoke should be triggered
    let updates = h.on_interested_received();
    assert!(h.is_peer_interested());
    assert_eq!(updates.len(), 1);
    assert!(matches!(&updates[0], PeerStateUpdate::ExecuteChoke));
}

#[test]
fn test_on_interested_received_no_execute_choke_when_not_choking() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.set_am_choking(false);
    let updates = h.on_interested_received();
    assert!(h.is_peer_interested());
    assert!(updates.is_empty());
}

// ── on_not_interested_received tests ───────────────────────────────

#[test]
fn test_on_not_interested_received_clears_flag() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.peer_interested = true;
    // am_choking is true by default, so no ExecuteChoke
    let updates = h.on_not_interested_received();
    assert!(!h.is_peer_interested());
    assert!(updates.is_empty());
}

#[test]
fn test_on_not_interested_received_triggers_execute_choke_when_not_choking() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.peer_interested = true;
    h.set_am_choking(false);
    let updates = h.on_not_interested_received();
    assert!(!h.is_peer_interested());
    assert_eq!(updates.len(), 1);
    assert!(matches!(&updates[0], PeerStateUpdate::ExecuteChoke));
}

// ── on_request_received tests ──────────────────────────────────────

#[test]
fn test_on_request_received_piece_when_has_piece_and_not_choking() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.set_am_choking(false);
    let resp = h.on_request_received(5, 0, 16384, |_| true, |_| false);
    assert_eq!(
        resp,
        RequestResponse::Piece {
            index: 5,
            begin: 0,
            length: 16384
        }
    );
}

#[test]
fn test_on_request_received_piece_when_allowed_fast() {
    let h = BtPeerMessageHandler::new(16384);
    // am_choking is true, but piece is in am-allowed-fast set
    let resp = h.on_request_received(5, 0, 16384, |_| true, |idx| idx == 5);
    assert_eq!(
        resp,
        RequestResponse::Piece {
            index: 5,
            begin: 0,
            length: 16384
        }
    );
}

#[test]
fn test_on_request_received_reject_when_choking_and_fast_extension() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.set_fast_extension_enabled(true);
    // am_choking is true by default, not in allowed-fast
    let resp = h.on_request_received(5, 0, 16384, |_| true, |_| false);
    assert_eq!(
        resp,
        RequestResponse::Reject {
            index: 5,
            begin: 0,
            length: 16384
        }
    );
}

#[test]
fn test_on_request_received_none_when_choking_no_fast_extension() {
    let h = BtPeerMessageHandler::new(16384);
    // am_choking is true, fast_extension is false
    let resp = h.on_request_received(5, 0, 16384, |_| true, |_| false);
    assert_eq!(resp, RequestResponse::None);
}

#[test]
fn test_on_request_received_reject_when_no_piece_and_fast_ext() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.set_am_choking(false);
    h.set_fast_extension_enabled(true);
    let resp = h.on_request_received(5, 0, 16384, |_| false, |_| false);
    assert_eq!(
        resp,
        RequestResponse::Reject {
            index: 5,
            begin: 0,
            length: 16384
        }
    );
}

// ── on_reject_received tests ───────────────────────────────────────

#[test]
fn test_on_reject_received_removes_slot() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.set_fast_extension_enabled(true);
    h.dispatcher.add_request_slot(5, 0, 16384);
    assert_eq!(h.count_outstanding_requests(), 1);

    let result = h.on_reject_received(5, 0, 16384);
    assert!(result.is_ok());
    assert_eq!(h.count_outstanding_requests(), 0);
}

#[test]
fn test_on_reject_received_errors_without_fast_extension() {
    let mut h = BtPeerMessageHandler::new(16384);
    // fast_extension is false by default
    let result = h.on_reject_received(5, 0, 16384);
    assert!(result.is_err());
}

#[test]
fn test_on_reject_received_no_matching_slot_is_ok() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.set_fast_extension_enabled(true);
    let result = h.on_reject_received(99, 0, 16384);
    assert!(result.is_ok());
}

// ── on_allowed_fast_received tests ─────────────────────────────────

#[test]
fn test_on_allowed_fast_received_adds_to_set() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.set_fast_extension_enabled(true);
    let result = h.on_allowed_fast_received(42);
    assert!(result.is_ok());
    assert!(h.is_in_peer_allowed_fast(42));
    assert!(!h.is_in_peer_allowed_fast(43));
}

#[test]
fn test_on_allowed_fast_received_errors_without_fast_extension() {
    let mut h = BtPeerMessageHandler::new(16384);
    let result = h.on_allowed_fast_received(42);
    assert!(result.is_err());
    assert!(!h.is_in_peer_allowed_fast(42));
}

#[test]
fn test_on_allowed_fast_received_duplicate_index() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.set_fast_extension_enabled(true);
    assert!(h.on_allowed_fast_received(42).is_ok());
    assert!(h.on_allowed_fast_received(42).is_ok()); // idempotent
    assert!(h.is_in_peer_allowed_fast(42));
    assert_eq!(h.peer_allowed_fast_set.len(), 1);
}

// ── on_have_all_received tests ─────────────────────────────────────

#[test]
fn test_on_have_all_received_marks_seeder() {
    let mut h = BtPeerMessageHandler::new(16384);
    let updates = h.on_have_all_received(false);
    assert_eq!(updates.len(), 1);
    assert!(matches!(&updates[0], PeerStateUpdate::MarkSeeder));
}

#[test]
fn test_on_have_all_received_disconnect_when_download_finished() {
    let mut h = BtPeerMessageHandler::new(16384);
    let updates = h.on_have_all_received(true);
    assert_eq!(updates.len(), 2);
    assert!(matches!(&updates[0], PeerStateUpdate::MarkSeeder));
    assert!(matches!(&updates[1], PeerStateUpdate::DisconnectSeeder));
}

// ── on_have_none_received tests ────────────────────────────────────

#[test]
fn test_on_have_none_received_clears_bitfield() {
    let mut h = BtPeerMessageHandler::new(16384);
    let updates = h.on_have_none_received();
    assert_eq!(updates.len(), 1);
    assert!(matches!(&updates[0], PeerStateUpdate::ClearBitfield));
}

// ── on_suggest_received tests ──────────────────────────────────────

#[test]
fn test_on_suggest_received_noop() {
    let mut h = BtPeerMessageHandler::new(16384);
    let interested_before = h.peer_interested;
    let choking_before = h.am_choking;
    h.on_suggest_received(42);
    assert_eq!(h.peer_interested, interested_before);
    assert_eq!(h.am_choking, choking_before);
}

// ── on_port_received tests ─────────────────────────────────────────

#[test]
fn test_on_port_received_nonzero() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.on_port_received(6881);
    // No state change expected, just logs
}

#[test]
fn test_on_port_received_zero() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.on_port_received(0);
    // No state change expected, just logs
}

// ── on_extended_received tests ─────────────────────────────────────

#[test]
fn test_on_extended_received_logs() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.on_extended_received(0, &[1, 2, 3]);
    // No state change expected, just logs
}

// ── Fast extension and choking state accessor tests ────────────────

#[test]
fn test_set_and_check_fast_extension() {
    let mut h = BtPeerMessageHandler::new(16384);
    assert!(!h.is_fast_extension_enabled());
    h.set_fast_extension_enabled(true);
    assert!(h.is_fast_extension_enabled());
    h.set_fast_extension_enabled(false);
    assert!(!h.is_fast_extension_enabled());
}

#[test]
fn test_set_and_check_am_choking() {
    let mut h = BtPeerMessageHandler::new(16384);
    assert!(h.is_am_choking());
    h.set_am_choking(false);
    assert!(!h.is_am_choking());
    h.set_am_choking(true);
    assert!(h.is_am_choking());
}

#[test]
fn test_metadata_get_mode() {
    let mut h = BtPeerMessageHandler::new(16384);
    assert!(!h.is_metadata_get_mode());
    h.set_metadata_get_mode(true);
    assert!(h.is_metadata_get_mode());
    h.set_metadata_get_mode(false);
    assert!(!h.is_metadata_get_mode());
}

// ── clear() resets new fields ──────────────────────────────────────

#[test]
fn test_clear_resets_all_state() {
    let mut h = BtPeerMessageHandler::new(16384);
    h.peer_interested = true;
    h.set_am_choking(false);
    h.set_fast_extension_enabled(true);
    h.on_allowed_fast_received(42).ok();
    h.dispatcher.add_request_slot(5, 0, 16384);

    h.clear();

    assert!(!h.peer_interested);
    assert!(h.am_choking);
    assert!(h.peer_allowed_fast_set.is_empty());
    assert_eq!(h.count_outstanding_requests(), 0);
    // fast_extension_enabled and metadata_get_mode are NOT reset by clear()
    // (they are negotiated at handshake and remain for the connection lifetime)
}

// ── RequestResponse enum tests ─────────────────────────────────────

#[test]
fn test_request_response_equality() {
    let a = RequestResponse::Piece {
        index: 5,
        begin: 0,
        length: 16384,
    };
    let b = RequestResponse::Piece {
        index: 5,
        begin: 0,
        length: 16384,
    };
    let c = RequestResponse::Reject {
        index: 5,
        begin: 0,
        length: 16384,
    };
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_ne!(a, RequestResponse::None);
}

// ── PeerStateUpdate variant tests ──────────────────────────────────

#[test]
fn test_peer_state_update_debug_format() {
    let u = PeerStateUpdate::HavePiece { index: 42 };
    assert!(format!("{:?}", u).contains("42"));
    let u = PeerStateUpdate::SetBitfield { data: vec![0xFF] };
    assert!(format!("{:?}", u).contains("255"));
    let u = PeerStateUpdate::MarkSeeder;
    assert!(format!("{:?}", u).contains("MarkSeeder"));
    let u = PeerStateUpdate::ClearBitfield;
    assert!(format!("{:?}", u).contains("ClearBitfield"));
    let u = PeerStateUpdate::ExecuteChoke;
    assert!(format!("{:?}", u).contains("ExecuteChoke"));
    let u = PeerStateUpdate::DisconnectSeeder;
    assert!(format!("{:?}", u).contains("DisconnectSeeder"));
}

// ── Integration: on_interested + on_not_interested cycle ───────────

#[test]
fn test_interested_not_interested_cycle() {
    let mut h = BtPeerMessageHandler::new(16384);
    // Start: peer_interested=false, am_choking=true
    let updates = h.on_interested_received();
    assert!(h.is_peer_interested());
    assert_eq!(updates.len(), 1); // ExecuteChoke

    h.set_am_choking(false);
    let updates = h.on_not_interested_received();
    assert!(!h.is_peer_interested());
    assert_eq!(updates.len(), 1); // ExecuteChoke

    // Now am_choking=true again, NotInterested should NOT trigger ExecuteChoke
    h.set_am_choking(true);
    h.peer_interested = true; // reset
    let updates = h.on_not_interested_received();
    assert!(!h.is_peer_interested());
    assert!(updates.is_empty());
}

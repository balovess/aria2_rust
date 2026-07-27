//! Tests for the BT peer interaction module.

use std::time::{Duration, Instant};

use aria2_protocol::bittorrent::message::types::BtMessage;

use crate::constants;
use crate::engine::bt_message_dispatcher::InactiveReason;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::extension_registry::ExtensionUpdate;
use crate::segment::piece::Piece;

use super::BtPeerInteractive;
use super::piece_provider::PieceProvider;
use super::types::*;

/// Helper to create an `Instant` representing a point in the past.
/// Uses `checked_sub` to avoid panicking on platforms where `Instant`
/// origin is near zero (e.g., shortly after system boot on Windows).
fn instant_past(secs: u64) -> Instant {
    Instant::now()
        .checked_sub(Duration::from_secs(secs))
        .unwrap_or(Instant::now())
}

// ── Legacy tests (preserved) ──────────────────────────────────────

#[test]
fn test_constants_are_reasonable() {
    const _: () = {
        assert!(PEER_CONNECTION_DELAY_MS >= 10);
        assert!(PEER_CONNECTION_DELAY_MS <= 1000);
        assert!(MAX_UNCHOKE_WAIT_ATTEMPTS >= 10);
        assert!(MAX_UNCHOKE_WAIT_ATTEMPTS <= 100);
        assert!(PEER_MESSAGE_TIMEOUT_SECS >= 1);
        assert!(PEER_MESSAGE_TIMEOUT_SECS <= 30);
    };
}

#[test]
fn test_peer_connection_result_default() {
    let result = PeerConnectionResult {
        connections: Vec::new(),
        failed_count: 0,
    };
    assert!(result.connections.is_empty());
    assert_eq!(result.failed_count, 0);
}

// ── New constant tests ─────────────────────────────────────────────

#[test]
fn test_bt_constants_match_cpp() {
    // These must match the C++ BtConstants.h exactly
    assert_eq!(DEFAULT_MAX_OUTSTANDING_REQUEST, 6);
    assert_eq!(UB_MAX_OUTSTANDING_REQUEST, 256);
    assert_eq!(DEFAULT_KEEP_ALIVE_INTERVAL_SECS, 120);
    assert_eq!(DEFAULT_ALLOWED_FAST_SET_SIZE, 10);
    assert_eq!(MUTUAL_UNINTERESTED_TIMEOUT_SECS, 30);
    assert_eq!(INACTIVITY_TIMEOUT_SECS, 60);
    assert_eq!(FLOODING_CHECK_INTERVAL_SECS, 5);
}

// ── PeerConnectionState tests ──────────────────────────────────────

#[test]
fn test_peer_connection_state_transitions() {
    // Initiator path
    let state = PeerConnectionState::InitiatorSendHandshake;
    assert!(state.is_handshake_state());
    assert!(!state.is_wired());

    let state = PeerConnectionState::InitiatorWaitHandshake;
    assert!(state.is_handshake_state());
    assert!(!state.is_wired());

    // Receiver path
    let state = PeerConnectionState::ReceiverWaitHandshake;
    assert!(state.is_handshake_state());
    assert!(!state.is_wired());

    // Wired
    let state = PeerConnectionState::Wired;
    assert!(!state.is_handshake_state());
    assert!(state.is_wired());
}

#[test]
fn test_peer_connection_state_display() {
    assert_eq!(
        PeerConnectionState::InitiatorSendHandshake.to_string(),
        "INITIATOR_SEND_HANDSHAKE"
    );
    assert_eq!(
        PeerConnectionState::InitiatorWaitHandshake.to_string(),
        "INITIATOR_WAIT_HANDSHAKE"
    );
    assert_eq!(
        PeerConnectionState::ReceiverWaitHandshake.to_string(),
        "RECEIVER_WAIT_HANDSHAKE"
    );
    assert_eq!(PeerConnectionState::Wired.to_string(), "WIRED");
}

#[test]
fn test_peer_connection_state_equality() {
    assert_eq!(
        PeerConnectionState::InitiatorSendHandshake,
        PeerConnectionState::InitiatorSendHandshake
    );
    assert_ne!(
        PeerConnectionState::InitiatorSendHandshake,
        PeerConnectionState::InitiatorWaitHandshake
    );
}

// ── InteractionResult tests ────────────────────────────────────────

#[test]
fn test_interaction_result_variants() {
    let r = InteractionResult::Continue {
        pex_pending: false,
        pex_update: None,
    };
    match r {
        InteractionResult::Continue {
            pex_pending,
            pex_update,
        } => {
            assert!(!pex_pending);
            assert!(pex_update.is_none());
        }
        _ => panic!("Expected Continue variant"),
    }

    let r = InteractionResult::Continue {
        pex_pending: true,
        pex_update: None,
    };
    match r {
        InteractionResult::Continue { pex_pending, .. } => {
            assert!(pex_pending);
        }
        _ => panic!("Expected Continue variant"),
    }

    let r = InteractionResult::Disconnect(InactiveReason::MutualUninterested);
    match r {
        InteractionResult::Disconnect(InactiveReason::MutualUninterested) => {}
        _ => panic!("Expected Disconnect(MutualUninterested)"),
    }

    let r = InteractionResult::Disconnect(InactiveReason::NoDataExchange);
    match r {
        InteractionResult::Disconnect(InactiveReason::NoDataExchange) => {}
        _ => panic!("Expected Disconnect(NoDataExchange)"),
    }

    let r = InteractionResult::Disconnect(InactiveReason::SeederToSeeder);
    match r {
        InteractionResult::Disconnect(InactiveReason::SeederToSeeder) => {}
        _ => panic!("Expected Disconnect(SeederToSeeder)"),
    }

    match InteractionResult::FloodingDetected {
        InteractionResult::FloodingDetected => {}
        _ => panic!("Expected FloodingDetected"),
    }

    match InteractionResult::WaitingForHandshake {
        InteractionResult::WaitingForHandshake => {}
        _ => panic!("Expected WaitingForHandshake"),
    }
}

// ── ChokingDecision / InterestDecision tests ───────────────────────

#[test]
fn test_choking_decision_variants() {
    assert_eq!(ChokingDecision::Choke, ChokingDecision::Choke);
    assert_ne!(ChokingDecision::Choke, ChokingDecision::Unchoke);
    assert_ne!(ChokingDecision::Unchoke, ChokingDecision::NoChange);
}

#[test]
fn test_interest_decision_variants() {
    assert_eq!(InterestDecision::Interested, InterestDecision::Interested);
    assert_ne!(
        InterestDecision::Interested,
        InterestDecision::NotInterested
    );
    assert_ne!(InterestDecision::NotInterested, InterestDecision::NoChange);
}

// ── BtPeerInteractive creation tests ───────────────────────────────

#[test]
fn test_bt_peer_interactive_creation() {
    let info_hash = [0u8; 20];
    let interactive = BtPeerInteractive::new(info_hash, 100);

    assert_eq!(
        interactive.state(),
        PeerConnectionState::InitiatorSendHandshake
    );
    assert_eq!(interactive.count_received_message_in_iteration(), 0);
    assert_eq!(
        interactive.max_outstanding_request(),
        DEFAULT_MAX_OUTSTANDING_REQUEST
    );
    assert_eq!(interactive.info_hash(), &[0u8; 20]);
    assert!(!interactive.is_metadata_get_mode());
    assert_eq!(interactive.last_have_index(), 0);
    // New fields
    assert!(interactive.am_choking());
    assert!(!interactive.am_interested());
    assert!(interactive.peer_choking());
    assert!(!interactive.peer_interested());
}

#[test]
fn test_bt_peer_interactive_with_state() {
    let info_hash = [1u8; 20];
    let interactive =
        BtPeerInteractive::with_state(info_hash, 50, PeerConnectionState::ReceiverWaitHandshake);

    assert_eq!(
        interactive.state(),
        PeerConnectionState::ReceiverWaitHandshake
    );
}

// ── Configuration setter tests ─────────────────────────────────────

#[test]
fn test_bt_peer_interactive_configuration() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);

    // Keep-alive interval
    interactive.set_keep_alive_interval(60);
    assert_eq!(interactive.keep_alive_interval_secs, 60);

    // Max outstanding request (clamped to [1, UB])
    interactive.set_max_outstanding_request(100);
    assert_eq!(interactive.max_outstanding_request(), 100);

    interactive.set_max_outstanding_request(0); // clamped to 1
    assert_eq!(interactive.max_outstanding_request(), 1);

    interactive.set_max_outstanding_request(9999); // clamped to UB
    assert_eq!(
        interactive.max_outstanding_request(),
        UB_MAX_OUTSTANDING_REQUEST
    );

    // Allowed fast set size
    interactive.set_allowed_fast_set_size(20);
    assert_eq!(interactive.allowed_fast_set_size, 20);

    // Feature flags
    interactive.set_ut_pex_enabled(true);
    assert!(interactive.ut_pex_enabled);

    interactive.set_dht_enabled(true);
    assert!(interactive.dht_enabled);

    interactive.enable_metadata_get_mode();
    assert!(interactive.is_metadata_get_mode());
}

// ── State machine transition tests ─────────────────────────────────

#[test]
fn test_advance_to_wait_handshake() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);

    assert_eq!(
        interactive.state(),
        PeerConnectionState::InitiatorSendHandshake
    );

    interactive.advance_to_wait_handshake();

    assert_eq!(
        interactive.state(),
        PeerConnectionState::InitiatorWaitHandshake
    );
}

#[test]
#[should_panic(expected = "Can only advance to WAIT_HANDSHAKE")]
fn test_advance_to_wait_handshake_invalid() {
    let info_hash = [0u8; 20];
    let mut interactive =
        BtPeerInteractive::with_state(info_hash, 100, PeerConnectionState::ReceiverWaitHandshake);
    interactive.advance_to_wait_handshake();
}

#[test]
fn test_advance_to_wired_from_initiator_wait() {
    let info_hash = [0u8; 20];
    let mut interactive =
        BtPeerInteractive::with_state(info_hash, 100, PeerConnectionState::InitiatorWaitHandshake);

    interactive.advance_to_wired();

    assert_eq!(interactive.state(), PeerConnectionState::Wired);
}

#[test]
fn test_advance_to_wired_from_receiver_wait() {
    let info_hash = [0u8; 20];
    let mut interactive =
        BtPeerInteractive::with_state(info_hash, 100, PeerConnectionState::ReceiverWaitHandshake);

    interactive.advance_to_wired();

    assert_eq!(interactive.state(), PeerConnectionState::Wired);
}

#[test]
#[should_panic(expected = "Cannot advance to WIRED from WIRED")]
fn test_advance_to_wired_invalid() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::with_state(info_hash, 100, PeerConnectionState::Wired);
    interactive.advance_to_wired();
}

#[test]
fn test_full_initiator_lifecycle() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);

    // INITIATOR_SEND_HANDSHAKE
    assert!(interactive.state().is_handshake_state());

    // → INITIATOR_WAIT_HANDSHAKE
    interactive.advance_to_wait_handshake();
    assert!(interactive.state().is_handshake_state());

    // → WIRED
    interactive.advance_to_wired();
    assert!(interactive.state().is_wired());
}

#[test]
fn test_full_receiver_lifecycle() {
    let info_hash = [0u8; 20];
    let mut interactive =
        BtPeerInteractive::with_state(info_hash, 100, PeerConnectionState::ReceiverWaitHandshake);

    assert!(interactive.state().is_handshake_state());

    interactive.advance_to_wired();
    assert!(interactive.state().is_wired());
}

// ── Keep-alive timer tests ─────────────────────────────────────────

#[test]
fn test_keep_alive_timer_initially_not_needed() {
    let info_hash = [0u8; 20];
    let interactive = BtPeerInteractive::new(info_hash, 100);
    // Just created — should not need keepalive yet
    assert!(!interactive.should_send_keepalive());
}

#[test]
fn test_keep_alive_timer_after_interval() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Force timer to past
    interactive.keep_alive_timer = instant_past(130);
    assert!(interactive.should_send_keepalive());
}

#[test]
fn test_keep_alive_timer_reset() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.keep_alive_timer = instant_past(130);
    assert!(interactive.should_send_keepalive());
    interactive.reset_keep_alive_timer();
    assert!(!interactive.should_send_keepalive());
}

#[test]
fn test_keep_alive_custom_interval() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.set_keep_alive_interval(60);
    interactive.keep_alive_timer = instant_past(65);
    assert!(interactive.should_send_keepalive());
}

// ── Flooding detection tests ───────────────────────────────────────

#[test]
fn test_flooding_detection_no_flooding() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Single choke/unchoke — not flooding
    interactive.on_message_received(0, false); // Choke, was not choking
    // Interval not elapsed yet
    assert!(!interactive.detect_flooding());
}

#[test]
fn test_flooding_detection_choke_flooding() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Two choke/unchoke transitions → flooding
    interactive.on_message_received(0, false); // Choke, was not choking
    interactive.on_message_received(1, true); // Unchoke, was choking
    // Force both outer timer and inner FloodingStat timer elapsed
    interactive.flooding_timer = instant_past(6);
    interactive.flooding_stat.last_reset = instant_past(6);
    assert!(interactive.detect_flooding());
}

#[test]
fn test_flooding_detection_keepalive_flooding() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.on_keepalive_received();
    interactive.on_keepalive_received();
    // Force both outer timer and inner FloodingStat timer elapsed
    interactive.flooding_timer = instant_past(6);
    interactive.flooding_stat.last_reset = instant_past(6);
    assert!(interactive.detect_flooding());
}

#[test]
fn test_flooding_detection_reset_after_interval() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.on_message_received(0, false);
    interactive.on_message_received(1, true);
    interactive.flooding_timer = instant_past(6);
    interactive.flooding_stat.last_reset = instant_past(6);
    assert!(interactive.detect_flooding());
    // After detection, stats are reset — no more flooding
    assert!(!interactive.detect_flooding());
}

// ── Message received processing tests ──────────────────────────────

#[test]
fn test_on_message_received_choke() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Peer was not choking, then sends Choke → transition detected
    interactive.on_message_received(0, false);
    assert_eq!(interactive.flooding_stat.choke_unchoke_count(), 1);

    // Peer was already choking, sends Choke again → no transition
    interactive.on_message_received(0, true);
    assert_eq!(interactive.flooding_stat.choke_unchoke_count(), 1);
}

#[test]
fn test_on_message_received_unchoke() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Peer was choking, sends Unchoke → transition detected
    interactive.on_message_received(1, true);
    assert_eq!(interactive.flooding_stat.choke_unchoke_count(), 1);

    // Peer was not choking, sends Unchoke → no transition
    interactive.on_message_received(1, false);
    assert_eq!(interactive.flooding_stat.choke_unchoke_count(), 1);
}

#[test]
fn test_on_message_received_data_exchange() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Request (ID=6) and Piece (ID=7) record data exchange
    interactive.on_message_received(6, false);
    interactive.on_message_received(7, false);
    // The active_interaction_checker's last_data_exchange was reset
    // We can verify by checking that a subsequent check doesn't
    // immediately return NoDataExchange
}

#[test]
fn test_on_keepalive_received() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.on_keepalive_received();
    interactive.on_keepalive_received();
    assert_eq!(interactive.flooding_stat.keepalive_count(), 2);
}

// ── Max outstanding request scaling tests ──────────────────────────

#[test]
fn test_scale_max_outstanding_request() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    assert_eq!(interactive.max_outstanding_request(), 6);

    // Lost >= 1/4 of outstanding requests → scale up
    // old=6, new=3, diff=3, diff*4=12 >= 6
    interactive.scale_max_outstanding_request(6, 3, false);
    assert_eq!(interactive.max_outstanding_request(), 12);
}

#[test]
fn test_scale_max_outstanding_request_capped() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.max_outstanding_request = 200;

    // Would go to 400, capped at UB=256
    interactive.scale_max_outstanding_request(200, 100, false);
    assert_eq!(
        interactive.max_outstanding_request(),
        UB_MAX_OUTSTANDING_REQUEST
    );
}

#[test]
fn test_scale_max_outstanding_request_no_scale_in_endgame() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // In end-game, don't scale
    interactive.scale_max_outstanding_request(6, 0, true);
    assert_eq!(interactive.max_outstanding_request(), 6);
}

#[test]
fn test_scale_max_outstanding_request_no_scale_small_loss() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    // Lost only 1 request: diff=1, diff*4=4 < 6 → no scale
    interactive.scale_max_outstanding_request(6, 5, false);
    assert_eq!(interactive.max_outstanding_request(), 6);
}

// ── Have index tracking tests ──────────────────────────────────────

#[test]
fn test_have_index_tracking() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    assert_eq!(interactive.last_have_index(), 0);

    interactive.set_last_have_index(42);
    assert_eq!(interactive.last_have_index(), 42);

    interactive.set_last_have_index(100);
    assert_eq!(interactive.last_have_index(), 100);
}

// ── check_have returns empty without piece storage ─────────────────

#[test]
fn test_check_have_returns_empty() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let have = interactive.check_have();
    assert!(have.is_empty());
}

// ── InactiveReason re-export test ──────────────────────────────────

#[test]
fn test_inactive_reason_variants() {
    assert_eq!(
        InactiveReason::SeederToSeeder,
        InactiveReason::SeederToSeeder
    );
    assert_eq!(
        InactiveReason::MutualUninterested,
        InactiveReason::MutualUninterested
    );
    assert_eq!(
        InactiveReason::NoDataExchange,
        InactiveReason::NoDataExchange
    );
    assert_ne!(
        InactiveReason::SeederToSeeder,
        InactiveReason::MutualUninterested
    );
}

// ==================================================================
// NEW TESTS — dispatch_message, choking/interest state, check_have
// ==================================================================

// ── DispatchUpdate tests ────────────────────────────────────────────

#[test]
fn test_dispatch_update_default() {
    let update = DispatchUpdate::default();
    assert!(update.cancelled_slots.is_empty());
    assert!(update.have_index.is_none());
    assert!(update.bitfield_data.is_none());
    assert!(!update.peer_choking_changed);
    assert!(!update.peer_choking);
    assert!(update.extension_update.is_none());
}

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

// ── post_handshake_processing tests ─────────────────────────────────

#[test]
fn test_post_handshake_processing_defaults() {
    let info_hash = [0u8; 20];
    let interactive = BtPeerInteractive::new(info_hash, 100);
    let actions = interactive.post_handshake_processing(None);
    assert!(actions.send_bitfield);
    assert!(actions.send_extension_handshake);
    assert!(!actions.send_dht_port);
    assert!(actions.allowed_fast_pieces.is_empty());
}

#[test]
fn test_post_handshake_processing_with_dht() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.set_dht_enabled(true);
    let actions = interactive.post_handshake_processing(None);
    assert!(actions.send_dht_port);
}

#[test]
fn test_post_handshake_processing_computes_fast_set() {
    let mut info_hash = [0u8; 20];
    info_hash[0] = 0xFF;
    let interactive = BtPeerInteractive::new(info_hash, 1000);
    let actions = interactive.post_handshake_processing(Some("192.168.0.1"));
    // C++ test vector: should produce the BEP 6 fast set
    assert!(!actions.allowed_fast_pieces.is_empty());
    assert!(actions.allowed_fast_pieces.len() <= 10);
    for &idx in &actions.allowed_fast_pieces {
        assert!(idx < 1000);
    }
}

// ── dispatch_message tests (no connection I/O) ─────────────────────

#[test]
fn test_dispatch_choke_updates_peer_choking() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.peer_choking = false;

    let mut conn = make_test_conn();
    let update = interactive.dispatch_message(BtMessage::Choke, &mut conn, |_| false);

    assert!(interactive.peer_choking());
    assert!(update.peer_choking_changed);
    assert!(update.peer_choking);
}

#[test]
fn test_dispatch_unchoke_updates_peer_choking() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.peer_choking = true;

    let mut conn = make_test_conn();
    let update = interactive.dispatch_message(BtMessage::Unchoke, &mut conn, |_| false);

    assert!(!interactive.peer_choking());
    assert!(update.peer_choking_changed);
    assert!(!update.peer_choking);
}

#[test]
fn test_dispatch_interested_updates_peer_interested() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);

    let mut conn = make_test_conn();
    let _update = interactive.dispatch_message(BtMessage::Interested, &mut conn, |_| false);

    assert!(interactive.peer_interested());
}

#[test]
fn test_dispatch_not_interested_updates_peer_interested() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    interactive.peer_interested = true;

    let mut conn = make_test_conn();
    let _update = interactive.dispatch_message(BtMessage::NotInterested, &mut conn, |_| false);

    assert!(!interactive.peer_interested());
}

#[test]
fn test_dispatch_have_updates_bitfield() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);

    let mut conn = make_test_conn();
    conn.allocate_session_resource(256 * 1024, 1024 * 1024);

    let update =
        interactive.dispatch_message(BtMessage::Have { piece_index: 0 }, &mut conn, |_| false);

    assert_eq!(update.have_index, Some(0));
    // The peer should now have piece 0
    assert!(conn.has_piece(0));
}

#[test]
fn test_dispatch_keepalive_updates_flooding() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);

    let mut conn = make_test_conn();
    let _update = interactive.dispatch_message(BtMessage::KeepAlive, &mut conn, |_| false);

    assert_eq!(interactive.flooding_stat.keepalive_count(), 1);
}

#[test]
fn test_dispatch_allowed_fast_updates_conn() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);

    let mut conn = make_test_conn();
    let _update =
        interactive.dispatch_message(BtMessage::AllowedFast { index: 42 }, &mut conn, |_| false);

    assert!(conn.is_allowed_fast(42));
    assert!(!conn.is_allowed_fast(43));
}

#[test]
fn test_dispatch_have_all_marks_seeder() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);

    let mut conn = make_test_conn();
    conn.allocate_session_resource(256 * 1024, 1024 * 1024);

    let _update = interactive.dispatch_message(BtMessage::HaveAll, &mut conn, |_| false);

    assert!(conn.seeder);
}

#[test]
fn test_dispatch_choke_removes_non_allowed_fast_slots() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);

    // Pre-populate some outstanding requests
    interactive
        .handler
        .send_request(5, 0, constants::BT_BLOCK_SIZE as u32, vec![1]);
    interactive
        .handler
        .send_request(6, 0, constants::BT_BLOCK_SIZE as u32, vec![2]);

    let mut conn = make_test_conn();
    // Piece 6 is in allowed-fast set
    let update = interactive.dispatch_message(BtMessage::Choke, &mut conn, |idx| idx == 6);

    // Should have removed slot for piece 5 but kept piece 6
    assert_eq!(update.cancelled_slots.len(), 1);
    assert_eq!(update.cancelled_slots[0].index, 5);
    assert!(interactive.handler.is_outstanding_request(6, 0));
    assert!(!interactive.handler.is_outstanding_request(5, 0));
}

#[test]
fn test_dispatch_piece_removes_outstanding_slot() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);

    interactive
        .handler
        .send_request(5, 0, constants::BT_BLOCK_SIZE as u32, vec![1]);

    let mut conn = make_test_conn();
    let _update = interactive.dispatch_message(
        BtMessage::Piece {
            index: 5,
            begin: 0,
            data: vec![0u8; constants::BT_BLOCK_SIZE],
        },
        &mut conn,
        |_| false,
    );

    // The outstanding request should be removed
    assert!(!interactive.handler.is_outstanding_request(5, 0));
}

#[test]
fn test_handler_access() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);

    // Verify handler is accessible
    assert_eq!(interactive.handler().count_outstanding_requests(), 0);

    // Mut access
    interactive
        .handler_mut()
        .send_request(5, 0, constants::BT_BLOCK_SIZE as u32, vec![1]);
    assert_eq!(interactive.handler().count_outstanding_requests(), 1);
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

// ── PostHandshakeActions tests ──────────────────────────────────────

#[test]
fn test_post_handshake_actions_fields() {
    let actions = PostHandshakeActions {
        send_bitfield: true,
        send_extension_handshake: true,
        send_dht_port: false,
        allowed_fast_pieces: vec![1, 2, 3],
    };
    assert!(actions.send_bitfield);
    assert!(actions.send_extension_handshake);
    assert!(!actions.send_dht_port);
    assert_eq!(actions.allowed_fast_pieces, vec![1, 2, 3]);
}

// ── Extension registry integration tests ────────────────────────────

#[test]
fn test_extension_registry_initial_state() {
    let info_hash = [0u8; 20];
    let interactive = BtPeerInteractive::new(info_hash, 100);
    assert_eq!(interactive.extension_registry().local_ut_metadata_id(), 1);
    assert_eq!(interactive.extension_registry().local_ut_pex_id(), 2);
    assert!(
        interactive
            .extension_registry()
            .peer_ut_metadata_id()
            .is_none()
    );
    assert!(interactive.extension_registry().peer_ut_pex_id().is_none());
}

#[test]
fn test_dispatch_extended_handshake() {
    use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;

    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    // Build and dispatch an extension handshake message
    let hs = ExtensionHandshake::new();
    let payload = hs.to_bytes();
    let msg = BtMessage::Extended { ext_id: 0, payload };

    let update = interactive.dispatch_message(msg, &mut conn, |_| false);

    // Verify the extension update was produced
    assert!(update.extension_update.is_some());
    match update.extension_update.unwrap() {
        ExtensionUpdate::HandshakeReceived {
            ut_metadata_id,
            ut_pex_id,
            reqq,
        } => {
            assert_eq!(ut_metadata_id, Some(1));
            assert_eq!(ut_pex_id, Some(2));
            assert_eq!(reqq, 500);
        }
        other => panic!("Expected HandshakeReceived, got {:?}", other),
    }

    // Verify the registry was updated
    assert_eq!(
        interactive.extension_registry().peer_ut_metadata_id(),
        Some(1)
    );
    assert_eq!(interactive.extension_registry().peer_ut_pex_id(), Some(2));

    // PEX should be auto-enabled after handshake
    assert!(interactive.ut_pex_enabled);
}

#[test]
fn test_dispatch_extended_ut_metadata_request() {
    use aria2_protocol::bittorrent::message::extension::{ExtensionHandshake, UtMetadataMessage};

    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    // First, receive a handshake so the registry knows the peer's IDs
    let hs = ExtensionHandshake::new();
    let hs_payload = hs.to_bytes();
    let _ = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 0,
            payload: hs_payload,
        },
        &mut conn,
        |_| false,
    );

    // Now dispatch a ut_metadata request (peer's id = 1)
    let msg = UtMetadataMessage::Request { piece: 0 };
    let payload = msg.to_payload();
    let update = interactive.dispatch_message(
        BtMessage::Extended { ext_id: 1, payload },
        &mut conn,
        |_| false,
    );

    assert!(update.extension_update.is_some());
    match update.extension_update.unwrap() {
        ExtensionUpdate::MetadataRequest { piece } => {
            assert_eq!(piece, 0);
        }
        other => panic!("Expected MetadataRequest, got {:?}", other),
    }
}

#[test]
fn test_dispatch_extended_ut_metadata_data() {
    use aria2_protocol::bittorrent::message::extension::{ExtensionHandshake, UtMetadataMessage};

    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    let hs = ExtensionHandshake::new();
    let hs_payload = hs.to_bytes();
    let _ = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 0,
            payload: hs_payload,
        },
        &mut conn,
        |_| false,
    );

    let msg = UtMetadataMessage::Data {
        piece: 2,
        total_size: 50000,
        data: b"test metadata".to_vec(),
    };
    let payload = msg.to_payload();
    let update = interactive.dispatch_message(
        BtMessage::Extended { ext_id: 1, payload },
        &mut conn,
        |_| false,
    );

    assert!(update.extension_update.is_some());
    match update.extension_update.unwrap() {
        ExtensionUpdate::MetadataPiece {
            piece,
            total_size,
            data,
        } => {
            assert_eq!(piece, 2);
            assert_eq!(total_size, 50000);
            assert_eq!(data, b"test metadata");
        }
        other => panic!("Expected MetadataPiece, got {:?}", other),
    }
}

#[test]
fn test_dispatch_extended_ut_pex() {
    use aria2_protocol::bittorrent::message::extension::{
        CompactPeerV4, ExtensionHandshake, UtPexMessage,
    };

    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    let hs = ExtensionHandshake::new();
    let hs_payload = hs.to_bytes();
    let _ = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 0,
            payload: hs_payload,
        },
        &mut conn,
        |_| false,
    );

    // Build a PEX message
    let mut pex = UtPexMessage::new();
    let mut peer_bytes = [0u8; 6];
    peer_bytes[..4].copy_from_slice(&[10, 0, 0, 1]);
    peer_bytes[4..6].copy_from_slice(&6881u16.to_be_bytes());
    pex.added.push(CompactPeerV4(peer_bytes));

    let payload = pex.to_payload();
    // Peer's ut_pex id = 2
    let update = interactive.dispatch_message(
        BtMessage::Extended { ext_id: 2, payload },
        &mut conn,
        |_| false,
    );

    assert!(update.extension_update.is_some());
    match update.extension_update.unwrap() {
        ExtensionUpdate::PeerExchange {
            added_v4,
            added_v6,
            dropped_v4,
            dropped_v6,
        } => {
            assert_eq!(added_v4.len(), 1);
            assert!(added_v6.is_empty());
            assert!(dropped_v4.is_empty());
            assert!(dropped_v6.is_empty());
            assert_eq!(added_v4[0], CompactPeerV4(peer_bytes));
        }
        other => panic!("Expected PeerExchange, got {:?}", other),
    }
}

#[test]
fn test_dispatch_extended_unknown_ext_id() {
    use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;

    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    // Receive handshake first
    let hs = ExtensionHandshake::new();
    let hs_payload = hs.to_bytes();
    let _ = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 0,
            payload: hs_payload,
        },
        &mut conn,
        |_| false,
    );

    // Dispatch with unknown ext_id
    let update = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 99,
            payload: vec![],
        },
        &mut conn,
        |_| false,
    );

    assert!(update.extension_update.is_none());
}

#[test]
fn test_dispatch_extended_handshake_enables_pex() {
    use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;

    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    // PEX should be disabled initially
    assert!(!interactive.ut_pex_enabled);

    // Receive handshake that includes ut_pex
    let hs = ExtensionHandshake::new();
    let hs_payload = hs.to_bytes();
    let _ = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 0,
            payload: hs_payload,
        },
        &mut conn,
        |_| false,
    );

    // PEX should now be enabled
    assert!(interactive.ut_pex_enabled);
}

#[test]
fn test_dispatch_extended_handshake_without_pex() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use std::collections::BTreeMap;

    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    // Build a handshake with only ut_metadata (no ut_pex)
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    root.insert(b"reqq".to_vec(), BencodeValue::Int(500));
    let bytes = BencodeValue::Dict(root).encode();

    let _ = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 0,
            payload: bytes,
        },
        &mut conn,
        |_| false,
    );

    // PEX should remain disabled since peer doesn't support it
    assert!(!interactive.ut_pex_enabled);
    // But ut_metadata should be available
    assert!(interactive.extension_registry().supports_ut_metadata());
}

// ── Helper to create a test BtPeerConn ─────────────────────────────

/// Create a minimal `BtPeerConn` for testing purposes.
fn make_test_conn() -> BtPeerConn {
    BtPeerConn::new_stub(&[0u8; 20])
}

// ── Mock PieceProvider for addRequests/fillPiece tests ──────────────

/// Mock piece provider that simulates PieceStorage operations.
struct MockPieceProvider {
    /// Whether has_missing_piece() returns true.
    has_missing: bool,
    /// Whether has_missing_unused_piece() returns true.
    has_missing_unused: bool,
    /// Whether is_end_game() returns true.
    is_end_game: bool,
    /// Whether enter_end_game() was called.
    entered_end_game: bool,
    /// Pieces to return from get_missing_pieces().
    missing_pieces: Vec<Piece>,
    /// Pieces to return from get_missing_fast_pieces().
    fast_pieces: Vec<Piece>,
}

impl MockPieceProvider {
    fn new() -> Self {
        Self {
            has_missing: true,
            has_missing_unused: true,
            is_end_game: false,
            entered_end_game: false,
            missing_pieces: Vec::new(),
            fast_pieces: Vec::new(),
        }
    }
}

impl PieceProvider for MockPieceProvider {
    fn has_missing_piece(&self, _peer: &BtPeerConn) -> bool {
        self.has_missing
    }

    fn get_missing_pieces(
        &mut self,
        count: usize,
        _peer: &BtPeerConn,
        _target_piece_indexes: &[u32],
        _cuid: u64,
    ) -> Vec<Piece> {
        self.missing_pieces
            .drain(..count.min(self.missing_pieces.len()))
            .collect()
    }

    fn get_missing_fast_pieces(
        &mut self,
        count: usize,
        _peer: &BtPeerConn,
        _target_piece_indexes: &[u32],
        _cuid: u64,
    ) -> Vec<Piece> {
        self.fast_pieces
            .drain(..count.min(self.fast_pieces.len()))
            .collect()
    }

    fn is_end_game(&self) -> bool {
        self.is_end_game
    }

    fn has_missing_unused_piece(&self) -> bool {
        self.has_missing_unused
    }

    fn enter_end_game(&mut self) {
        self.entered_end_game = true;
    }

    fn get_advertised_piece_indexes_ext(
        &self,
        _my_cuid: u64,
        _last_have_index: u64,
    ) -> (Vec<usize>, u64) {
        (Vec::new(), 0)
    }

    fn get_bitfield_length_ext(&self) -> usize {
        0
    }

    fn get_bitfield_ext(&self) -> Vec<u8> {
        Vec::new()
    }

    fn all_download_finished_ext(&self) -> bool {
        false
    }

    fn get_completed_length_ext(&self) -> u64 {
        0
    }
}

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

// ── checkHave optimization tests ──────────────────────────────────────

#[test]
fn test_check_have_result_none_when_no_indexes() {
    let mut interactive = BtPeerInteractive::new([0u8; 20], 100);
    let result = interactive.check_have_optimized(
        &|_last| (Vec::new(), 0u64), // no new pieces
        100,                         // bitfield_length
        false,                       // fast_ext
        false,                       // all_done
        0,                           // completed_len
    );
    assert_eq!(result, CheckHaveResult::None);
}

#[test]
fn test_check_have_result_bitfield_when_many_indexes() {
    let mut interactive = BtPeerInteractive::new([0u8; 20], 100);
    // 20 Have messages = 20 * 9 = 180 bytes
    // Bitfield = 5 + 10 = 15 bytes
    // Condition: 5 + 10 <= 20 * 9 → true → use Bitfield
    let indexes: Vec<usize> = (0..20).collect();
    let result = interactive.check_have_optimized(
        &|_last| (indexes.clone(), 20u64),
        10,    // bitfield_length=10 → 5+10=15 <= 180
        false, // fast_ext
        false, // all_done
        1024,  // completed_len > 0
    );
    assert_eq!(result, CheckHaveResult::Bitfield);
}

#[test]
fn test_check_have_result_have_all_when_fast_ext_and_complete() {
    let mut interactive = BtPeerInteractive::new([0u8; 20], 100);
    let indexes: Vec<usize> = (0..20).collect();
    let result = interactive.check_have_optimized(
        &|_last| (indexes.clone(), 20u64),
        10,
        true, // fast_ext enabled
        true, // all done
        1024,
    );
    assert_eq!(result, CheckHaveResult::HaveAll);
}

#[test]
fn test_check_have_result_have_indexes_when_few() {
    let mut interactive = BtPeerInteractive::new([0u8; 20], 100);
    // 2 Have messages = 2 * 9 = 18 bytes
    // Bitfield = 5 + 100 = 105 bytes
    // Condition: 5 + 100 <= 2 * 9 → false → use Have messages
    let indexes = vec![0usize, 1];
    let result = interactive.check_have_optimized(
        &|_last| (indexes.clone(), 2u64),
        100, // bitfield_length=100 → 5+100=105 > 18
        false,
        false,
        1024,
    );
    assert_eq!(result, CheckHaveResult::HaveIndexes(indexes));
}

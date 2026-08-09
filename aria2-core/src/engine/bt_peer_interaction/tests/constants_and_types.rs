//! Tests for constants and type variants.

use crate::engine::bt_message_dispatcher::InactiveReason;
use crate::engine::bt_peer_interaction::types::*;

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

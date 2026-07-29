//! Tests for PeerConnectionState, BtPeerInteractive creation, configuration,
//! state machine transitions, and post-handshake processing.

use super::super::BtPeerInteractive;
use super::super::types::*;

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

    // -> INITIATOR_WAIT_HANDSHAKE
    interactive.advance_to_wait_handshake();
    assert!(interactive.state().is_handshake_state());

    // -> WIRED
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

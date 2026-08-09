//! Tests for same-Peer-ID duplicate detection and handshake validation.

use aria2_protocol::bittorrent::message::handshake::Handshake;

use crate::error::{Aria2Error, RecoverableError};

use super::super::BtPeerInteractive;
use super::super::types::PeerIdCheckResult;

#[test]
fn test_peer_id_check_result_ok_when_unique() {
    let received = [1u8; 20];
    let our_id = [2u8; 20];
    let connected: Vec<[u8; 20]> = Vec::new();

    let result = BtPeerInteractive::check_duplicate_peer_id(&received, &our_id, &connected);
    assert_eq!(result, PeerIdCheckResult::Ok);
}

#[test]
fn test_peer_id_check_result_ok_when_different_from_all_connected() {
    let received = [10u8; 20];
    let our_id = [20u8; 20];
    let connected: [[u8; 20]; 3] = [[30u8; 20], [40u8; 20], [50u8; 20]];

    let result = BtPeerInteractive::check_duplicate_peer_id(&received, &our_id, &connected);
    assert_eq!(result, PeerIdCheckResult::Ok);
}

#[test]
fn test_peer_id_check_result_self_connection() {
    // When the remote peer ID matches our own static peer ID, it is
    // a self-connection (C++: "Drop connection from the same Peer ID").
    let our_id = [0xAA; 20];
    let received = our_id; // Same as ours

    let result = BtPeerInteractive::check_duplicate_peer_id(&received, &our_id, &[]);
    assert_eq!(result, PeerIdCheckResult::SelfConnection);
}

#[test]
fn test_peer_id_check_result_duplicate_peer() {
    // When the remote peer ID matches an already-connected peer,
    // it is a duplicate (C++: "Same Peer ID has been already seen").
    let our_id = [0u8; 20];
    let received = [0xBB; 20];
    let connected: [[u8; 20]; 2] = [[0xAA; 20], [0xBB; 20]]; // Second matches received

    let result = BtPeerInteractive::check_duplicate_peer_id(&received, &our_id, &connected);
    assert_eq!(result, PeerIdCheckResult::DuplicatePeer);
}

#[test]
fn test_peer_id_check_result_self_connection_takes_priority() {
    // Self-connection is checked first. If the remote peer ID matches
    // our own AND is also present in the connected list, we still
    // report SelfConnection (matching C++ order of checks).
    let our_id = [0xFF; 20];
    let received = our_id;
    let connected: [[u8; 20]; 1] = [our_id];

    let result = BtPeerInteractive::check_duplicate_peer_id(&received, &our_id, &connected);
    assert_eq!(result, PeerIdCheckResult::SelfConnection);
}

#[test]
fn test_peer_id_check_result_duplicate_at_first_position() {
    let our_id = [0u8; 20];
    let received = [0x11; 20];
    let connected: [[u8; 20]; 3] = [[0x11; 20], [0x22; 20], [0x33; 20]];

    let result = BtPeerInteractive::check_duplicate_peer_id(&received, &our_id, &connected);
    assert_eq!(result, PeerIdCheckResult::DuplicatePeer);
}

#[test]
fn test_peer_id_check_result_duplicate_at_last_position() {
    let our_id = [0u8; 20];
    let received = [0x33; 20];
    let connected: [[u8; 20]; 3] = [[0x11; 20], [0x22; 20], [0x33; 20]];

    let result = BtPeerInteractive::check_duplicate_peer_id(&received, &our_id, &connected);
    assert_eq!(result, PeerIdCheckResult::DuplicatePeer);
}

#[test]
fn test_peer_id_check_result_with_empty_connected_list() {
    // No connected peers — only self-connection check matters.
    let our_id = [1u8; 20];
    let received = [2u8; 20];

    let result = BtPeerInteractive::check_duplicate_peer_id(&received, &our_id, &[]);
    assert_eq!(result, PeerIdCheckResult::Ok);
}

#[test]
fn test_peer_id_check_result_with_empty_connected_list_self() {
    let our_id = [1u8; 20];
    let received = our_id;

    let result = BtPeerInteractive::check_duplicate_peer_id(&received, &our_id, &[]);
    assert_eq!(result, PeerIdCheckResult::SelfConnection);
}

#[test]
fn test_validate_handshake_peer_id_ok() {
    let info_hash = [0u8; 20];
    let our_id = [2u8; 20];
    let handshake = Handshake::new(&info_hash, &[1u8; 20]);
    let connected: Vec<[u8; 20]> = Vec::new();

    let result = BtPeerInteractive::validate_handshake_peer_id(&handshake, &our_id, &connected);
    assert!(result.is_ok());
}

#[test]
fn test_validate_handshake_peer_id_self_connection() {
    let info_hash = [0u8; 20];
    let our_id = [0xAA; 20];
    // Remote peer sends our own ID — self-connection
    let handshake = Handshake::new(&info_hash, &our_id);
    let connected: Vec<[u8; 20]> = Vec::new();

    let result = BtPeerInteractive::validate_handshake_peer_id(&handshake, &our_id, &connected);
    assert!(result.is_err());
    match result.unwrap_err() {
        Aria2Error::Recoverable(RecoverableError::HandshakeRejection { reason }) => {
            assert!(reason.contains("Self-connection"));
        }
        other => panic!("Expected HandshakeRejection, got {:?}", other),
    }
}

#[test]
fn test_validate_handshake_peer_id_duplicate() {
    let info_hash = [0u8; 20];
    let our_id = [2u8; 20];
    let remote_id = [0xBB; 20];
    let handshake = Handshake::new(&info_hash, &remote_id);
    // The remote ID is already present in the connected list
    let connected: [[u8; 20]; 2] = [[0xAA; 20], [0xBB; 20]];

    let result = BtPeerInteractive::validate_handshake_peer_id(&handshake, &our_id, &connected);
    assert!(result.is_err());
    match result.unwrap_err() {
        Aria2Error::Recoverable(RecoverableError::HandshakeRejection { reason }) => {
            assert!(reason.contains("Duplicate"));
        }
        other => panic!("Expected HandshakeRejection, got {:?}", other),
    }
}

#[test]
fn test_peer_id_check_result_enum_variants() {
    // Verify all variants exist and are distinct
    assert_ne!(
        PeerIdCheckResult::SelfConnection,
        PeerIdCheckResult::DuplicatePeer
    );
    assert_ne!(PeerIdCheckResult::DuplicatePeer, PeerIdCheckResult::Ok);
    assert_ne!(PeerIdCheckResult::SelfConnection, PeerIdCheckResult::Ok);
}

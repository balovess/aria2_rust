//! Tests for `bt_message_receiver` — moved from the main module to
//! keep the source file under 600 lines.

use crate::engine::bt_message_receiver::{BtMessageReceiver, HandshakeResult};
use aria2_protocol::bittorrent::message::handshake::Handshake;
use aria2_protocol::bittorrent::message::types::HANDSHAKE_LENGTH;

/// Helper: create a valid handshake byte buffer with the given info_hash and peer_id.
fn make_handshake_bytes(info_hash: &[u8; 20], peer_id: &[u8; 20]) -> [u8; HANDSHAKE_LENGTH] {
    Handshake::new(info_hash, peer_id).to_bytes()
}

// -----------------------------------------------------------------------
// Test 1: New receiver initial state
// -----------------------------------------------------------------------
#[test]
fn test_new_receiver_initial_state() {
    let info_hash = [0xAA; 20];
    let receiver = BtMessageReceiver::new(info_hash);
    assert!(!receiver.is_handshake_sent());
}

// -----------------------------------------------------------------------
// Test 2: Full handshake success
// -----------------------------------------------------------------------
#[test]
fn test_full_handshake_success() {
    let info_hash = [0x11; 20];
    let peer_id = [0x22; 20];
    let mut receiver = BtMessageReceiver::new(info_hash);
    let data = make_handshake_bytes(&info_hash, &peer_id);

    let result = receiver.receive_handshake(&data);
    match result {
        HandshakeResult::Completed {
            peer_id: pid,
            reserved_bytes,
        } => {
            assert_eq!(pid, peer_id);
            assert_ne!(reserved_bytes, [0u8; 8]);
        }
        _ => panic!("Expected Completed, got {:?}", result),
    }
    assert!(receiver.is_handshake_sent());
}

// -----------------------------------------------------------------------
// Test 3: Info hash mismatch
// -----------------------------------------------------------------------
#[test]
fn test_info_hash_mismatch() {
    let expected_hash = [0x11; 20];
    let wrong_hash = [0xFF; 20];
    let peer_id = [0x22; 20];
    let mut receiver = BtMessageReceiver::new(expected_hash);

    let data = make_handshake_bytes(&wrong_hash, &peer_id);
    let result = receiver.receive_handshake(&data);

    match result {
        HandshakeResult::InfoHashMismatch { received } => {
            assert_eq!(received, wrong_hash);
        }
        _ => panic!("Expected InfoHashMismatch, got {:?}", result),
    }
    assert!(!receiver.is_handshake_sent());
}

// -----------------------------------------------------------------------
// Test 4: Incomplete data (too short)
// -----------------------------------------------------------------------
#[test]
fn test_incomplete_data_too_short() {
    let info_hash = [0x11; 20];
    let mut receiver = BtMessageReceiver::new(info_hash);

    let result = receiver.receive_handshake(&[]);
    assert_eq!(result, HandshakeResult::NeedMoreData);

    let partial = vec![0u8; 67];
    let result = receiver.receive_handshake(&partial);
    assert_eq!(result, HandshakeResult::NeedMoreData);
    assert!(!receiver.is_handshake_sent());
}

// -----------------------------------------------------------------------
// Test 5: Quick reply with valid info_hash (48 bytes, partial)
// -----------------------------------------------------------------------
#[test]
fn test_quick_reply_valid_info_hash_partial() {
    let info_hash = [0x33; 20];
    let peer_id = [0x44; 20];
    let mut receiver = BtMessageReceiver::new(info_hash);

    let full_data = make_handshake_bytes(&info_hash, &peer_id);
    let partial = &full_data[..48];

    let result = receiver.receive_handshake_with_quick_reply(partial);
    assert_eq!(result, HandshakeResult::NeedMoreData);
    assert!(receiver.is_handshake_sent());
}

// -----------------------------------------------------------------------
// Test 6: Quick reply with invalid info_hash
// -----------------------------------------------------------------------
#[test]
fn test_quick_reply_invalid_info_hash() {
    let expected_hash = [0x33; 20];
    let wrong_hash = [0xFF; 20];
    let peer_id = [0x44; 20];
    let mut receiver = BtMessageReceiver::new(expected_hash);

    let full_data = make_handshake_bytes(&wrong_hash, &peer_id);
    let partial = &full_data[..48];

    let result = receiver.receive_handshake_with_quick_reply(partial);
    match result {
        HandshakeResult::InfoHashMismatch { received } => {
            assert_eq!(received, wrong_hash);
        }
        _ => panic!("Expected InfoHashMismatch, got {:?}", result),
    }
    assert!(!receiver.is_handshake_sent());
}

// -----------------------------------------------------------------------
// Test 7: handshake_sent flag transitions
// -----------------------------------------------------------------------
#[test]
fn test_handshake_sent_flag_transitions() {
    let info_hash = [0x11; 20];
    let mut receiver = BtMessageReceiver::new(info_hash);

    assert!(!receiver.is_handshake_sent());
    receiver.set_handshake_sent(true);
    assert!(receiver.is_handshake_sent());
    receiver.set_handshake_sent(false);
    assert!(!receiver.is_handshake_sent());
}

// -----------------------------------------------------------------------
// Test 8: Roundtrip with Handshake::parse()
// -----------------------------------------------------------------------
#[test]
fn test_roundtrip_with_handshake_parse() {
    let info_hash = [0xAB; 20];
    let peer_id = [0xCD; 20];

    let handshake = Handshake::new(&info_hash, &peer_id);
    let bytes = handshake.to_bytes();

    let parsed = Handshake::parse(&bytes).unwrap();
    assert_eq!(parsed.info_hash, info_hash);
    assert_eq!(parsed.peer_id, peer_id);

    let mut receiver = BtMessageReceiver::new(info_hash);
    let result = receiver.receive_handshake(&bytes);

    match result {
        HandshakeResult::Completed {
            peer_id: pid,
            reserved_bytes,
        } => {
            assert_eq!(pid, peer_id);
            assert_eq!(reserved_bytes, handshake.reserved);
        }
        _ => panic!("Expected Completed, got {:?}", result),
    }
}

// -----------------------------------------------------------------------
// Test 9: Quick reply with full 68 bytes
// -----------------------------------------------------------------------
#[test]
fn test_quick_reply_full_68_bytes() {
    let info_hash = [0x55; 20];
    let peer_id = [0x66; 20];
    let mut receiver = BtMessageReceiver::new(info_hash);

    let data = make_handshake_bytes(&info_hash, &peer_id);
    let result = receiver.receive_handshake_with_quick_reply(&data);

    match result {
        HandshakeResult::Completed { peer_id: pid, .. } => {
            assert_eq!(pid, peer_id);
        }
        _ => panic!("Expected Completed, got {:?}", result),
    }
    assert!(receiver.is_handshake_sent());
}

// -----------------------------------------------------------------------
// Test 10: Quick reply not triggered when handshake already sent
// -----------------------------------------------------------------------
#[test]
fn test_quick_reply_not_triggered_when_already_sent() {
    let info_hash = [0x55; 20];
    let peer_id = [0x66; 20];
    let mut receiver = BtMessageReceiver::new(info_hash);
    receiver.set_handshake_sent(true);

    let data = make_handshake_bytes(&info_hash, &peer_id);
    let result = receiver.receive_handshake_with_quick_reply(&data);

    match result {
        HandshakeResult::Completed { peer_id: pid, .. } => {
            assert_eq!(pid, peer_id);
        }
        _ => panic!("Expected Completed, got {:?}", result),
    }
}

// -----------------------------------------------------------------------
// Test 11: Quick reply not triggered with less than 48 bytes
// -----------------------------------------------------------------------
#[test]
fn test_quick_reply_not_triggered_with_less_than_48_bytes() {
    let info_hash = [0x55; 20];
    let mut receiver = BtMessageReceiver::new(info_hash);

    let data = vec![0u8; 47];
    let result = receiver.receive_handshake_with_quick_reply(&data);
    assert_eq!(result, HandshakeResult::NeedMoreData);
    assert!(!receiver.is_handshake_sent());
}

// -----------------------------------------------------------------------
// Test 12: Parse error with bad protocol
// -----------------------------------------------------------------------
#[test]
fn test_parse_error_bad_protocol() {
    let info_hash = [0x11; 20];
    let mut receiver = BtMessageReceiver::new(info_hash);

    let mut bad_data = [0u8; HANDSHAKE_LENGTH];
    bad_data[0] = 19;
    bad_data[1..20].copy_from_slice(b"Invalid protocol!!!");

    let result = receiver.receive_handshake(&bad_data);
    match result {
        HandshakeResult::ParseError { reason } => {
            assert!(!reason.is_empty());
        }
        _ => panic!("Expected ParseError, got {:?}", result),
    }
    assert!(!receiver.is_handshake_sent());
}

// -----------------------------------------------------------------------
// Test 13: Quick reply then full data (two-step handshake)
// -----------------------------------------------------------------------
#[test]
fn test_quick_reply_then_full_data() {
    let info_hash = [0x77; 20];
    let peer_id = [0x88; 20];
    let mut receiver = BtMessageReceiver::new(info_hash);

    let full_data = make_handshake_bytes(&info_hash, &peer_id);

    let partial = &full_data[..48];
    let result1 = receiver.receive_handshake_with_quick_reply(partial);
    assert_eq!(result1, HandshakeResult::NeedMoreData);
    assert!(receiver.is_handshake_sent());

    let result2 = receiver.receive_handshake_with_quick_reply(&full_data);
    match result2 {
        HandshakeResult::Completed { peer_id: pid, .. } => {
            assert_eq!(pid, peer_id);
        }
        _ => panic!("Expected Completed, got {:?}", result2),
    }
}

// -----------------------------------------------------------------------
// Test 14: Reserved bytes preserved in Completed result
// -----------------------------------------------------------------------
#[test]
fn test_reserved_bytes_preserved() {
    let info_hash = [0x99; 20];
    let peer_id = [0xAA; 20];

    let handshake = Handshake::new(&info_hash, &peer_id).with_dht(true);
    let bytes = handshake.to_bytes();

    let mut receiver = BtMessageReceiver::new(info_hash);
    let result = receiver.receive_handshake(&bytes);

    match result {
        HandshakeResult::Completed { reserved_bytes, .. } => {
            assert_ne!(reserved_bytes[7] & 0x04, 0);
            assert_ne!(reserved_bytes[5] & 0x10, 0);
            assert_ne!(reserved_bytes[7] & 0x01, 0);
        }
        _ => panic!("Expected Completed, got {:?}", result),
    }
}

// -----------------------------------------------------------------------
// Test 15: handshake_sent set after successful receive_handshake
// -----------------------------------------------------------------------
#[test]
fn test_handshake_sent_set_after_successful_receive() {
    let info_hash = [0xBB; 20];
    let peer_id = [0xCC; 20];
    let mut receiver = BtMessageReceiver::new(info_hash);

    assert!(!receiver.is_handshake_sent());

    let data = make_handshake_bytes(&info_hash, &peer_id);
    let _ = receiver.receive_handshake(&data);

    assert!(receiver.is_handshake_sent());

    let _ = receiver.receive_handshake(&data);
    assert!(receiver.is_handshake_sent());
}

// -----------------------------------------------------------------------
// Test 16: Quick reply with data between 48 and 67 bytes
// -----------------------------------------------------------------------
#[test]
fn test_quick_reply_data_between_48_and_67() {
    let info_hash = [0xDD; 20];
    let peer_id = [0xEE; 20];
    let full_data = make_handshake_bytes(&info_hash, &peer_id);

    let mut receiver = BtMessageReceiver::new(info_hash);
    let result48 = receiver.receive_handshake_with_quick_reply(&full_data[..48]);
    assert_eq!(result48, HandshakeResult::NeedMoreData);
    assert!(receiver.is_handshake_sent());

    let mut receiver = BtMessageReceiver::new(info_hash);
    let result60 = receiver.receive_handshake_with_quick_reply(&full_data[..60]);
    assert_eq!(result60, HandshakeResult::NeedMoreData);
    assert!(receiver.is_handshake_sent());

    let mut receiver = BtMessageReceiver::new(info_hash);
    let result67 = receiver.receive_handshake_with_quick_reply(&full_data[..67]);
    assert_eq!(result67, HandshakeResult::NeedMoreData);
    assert!(receiver.is_handshake_sent());
}

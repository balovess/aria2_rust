//! Tests for dispatch_message and handler access.

use crate::constants;
use aria2_protocol::bittorrent::message::types::BtMessage;

use super::super::BtPeerInteractive;
use super::make_test_conn;

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
    let transition = update.bitfield_update.expect("Have transition");
    assert_eq!(transition.old, vec![0]);
    assert_eq!(transition.new, vec![0x80]);
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

    let update = interactive.dispatch_message(BtMessage::HaveAll, &mut conn, |_| false);

    let transition = update.bitfield_update.expect("HaveAll transition");
    assert_eq!(transition.old, vec![0]);
    assert_eq!(transition.new, vec![0xf0]);
    assert!(conn.seeder);
}

#[test]
fn test_dispatch_have_none_emits_clearing_transition() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();
    conn.allocate_session_resource(256 * 1024, 1024 * 1024);
    interactive.dispatch_message(BtMessage::HaveAll, &mut conn, |_| false);

    let update = interactive.dispatch_message(BtMessage::HaveNone, &mut conn, |_| false);
    let transition = update.bitfield_update.expect("HaveNone transition");
    assert_eq!(transition.old, vec![0xf0]);
    assert_eq!(transition.new, vec![0]);
    assert!(!conn.seeder);
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

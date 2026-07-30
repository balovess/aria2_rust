//! Tests for IP blocklist integration.

use std::sync::Arc;

use crate::engine::bt_peer_blocklist::BtPeerBlocklist;
use crate::engine::bt_peer_storage::storage::DefaultPeerStorage;
use super::basic::make_peer;

#[test]
fn test_add_peer_rejected_by_blocklist() {
    let mut bl = BtPeerBlocklist::new();
    bl.add_rule("10.0.0.0/8").unwrap();

    let mut storage = DefaultPeerStorage::new();
    storage.set_peer_blocklist(Arc::new(bl));

    // Blocked peer should be rejected.
    assert!(!storage.add_peer(make_peer("10.0.0.1", 6881)));
    assert_eq!(storage.blocklist_reject_count(), 1);
    assert_eq!(
        storage.count_all_peers(),
        0,
        "blocked peer should not be added"
    );
    storage.verify_invariant();
}

#[test]
fn test_add_peer_non_blocked_succeeds() {
    let mut bl = BtPeerBlocklist::new();
    bl.add_rule("10.0.0.0/8").unwrap();

    let mut storage = DefaultPeerStorage::new();
    storage.set_peer_blocklist(Arc::new(bl));

    // Non-blocked peer should succeed.
    assert!(storage.add_peer(make_peer("192.168.1.1", 6881)));
    assert_eq!(storage.count_all_peers(), 1);
    assert_eq!(storage.blocklist_reject_count(), 0);
    storage.verify_invariant();
}

#[test]
fn test_add_peers_batch_blocklist_filtering() {
    let mut bl = BtPeerBlocklist::new();
    bl.add_rule("10.0.0.0/8").unwrap();

    let mut storage = DefaultPeerStorage::new();
    storage.set_peer_blocklist(Arc::new(bl));

    let peers = vec![
        make_peer("10.0.0.1", 6881),    // blocked
        make_peer("192.168.1.1", 6881), // allowed
        make_peer("10.0.0.2", 6881),    // blocked
        make_peer("8.8.8.8", 6881),     // allowed
    ];

    storage.add_peers(peers);
    assert_eq!(
        storage.count_all_peers(),
        2,
        "only non-blocked peers should be added"
    );
    assert_eq!(storage.blocklist_reject_count(), 2);
    storage.verify_invariant();
}

#[test]
fn test_add_and_checkout_peer_blocked_by_blocklist() {
    let mut bl = BtPeerBlocklist::new();
    bl.add_rule("172.16.0.0/12").unwrap();

    let mut storage = DefaultPeerStorage::new();
    storage.set_peer_blocklist(Arc::new(bl));

    let peer = make_peer("172.16.0.1", 6881);
    let result = storage.add_and_checkout_peer(peer, 1);
    assert!(result.is_none());
    assert_eq!(storage.blocklist_reject_count(), 1);
}

#[test]
fn test_no_blocklist_allows_all_peers() {
    let mut storage = DefaultPeerStorage::new();
    // No blocklist configured -- all peers should be accepted.
    assert!(storage.add_peer(make_peer("10.0.0.1", 6881)));
    assert!(storage.add_peer(make_peer("192.168.1.1", 6881)));
    assert_eq!(storage.count_all_peers(), 2);
    assert_eq!(storage.blocklist_reject_count(), 0);
}

#[test]
fn test_blocklist_reject_count_increments() {
    let mut bl = BtPeerBlocklist::new();
    bl.add_rule("10.0.0.0/8").unwrap();

    let mut storage = DefaultPeerStorage::new();
    storage.set_peer_blocklist(Arc::new(bl));

    assert!(!storage.add_peer(make_peer("10.0.0.1", 6881)));
    assert!(!storage.add_peer(make_peer("10.0.0.2", 6881)));
    assert!(!storage.add_peer(make_peer("10.0.0.3", 6881)));
    assert_eq!(storage.blocklist_reject_count(), 3);

    // Non-blocked peer does not increment counter.
    assert!(storage.add_peer(make_peer("192.168.1.1", 6881)));
    assert_eq!(storage.blocklist_reject_count(), 3);
}

//! Tests for get_peer lookup and invariant verification.

use super::basic::make_peer;
use crate::engine::bt_peer_storage::storage::DefaultPeerStorage;

// ------------------------------------------------------------------
// get_peer (C++ DefaultPeerStorage::getPeer)
// ------------------------------------------------------------------

#[test]
fn test_get_peer_from_unused() {
    let mut storage = DefaultPeerStorage::new();
    storage.add_peer(make_peer("192.168.1.1", 6881));

    let found = storage.get_peer("192.168.1.1", 6881);
    assert!(found.is_some(), "should find peer in unused list");
    assert_eq!(found.unwrap().ip, "192.168.1.1");
}

#[test]
fn test_get_peer_from_used() {
    let mut storage = DefaultPeerStorage::new();
    storage.add_peer(make_peer("192.168.1.1", 6881));
    storage.checkout_peer(1);

    let found = storage.get_peer("192.168.1.1", 6881);
    assert!(found.is_some(), "should find peer in used set");
    assert_eq!(found.unwrap().used_by, 1);
}

#[test]
fn test_get_peer_not_found() {
    let storage = DefaultPeerStorage::new();
    assert!(storage.get_peer("192.168.1.1", 6881).is_none());
}

// ------------------------------------------------------------------
// Invariant check after various operations
// ------------------------------------------------------------------

#[test]
fn test_invariant_after_mixed_operations() {
    let mut storage = DefaultPeerStorage::new();

    // Add several peers.
    for i in 0..10u16 {
        assert!(storage.add_peer(make_peer(&format!("10.0.0.{}", i), 6881 + i)));
    }
    storage.verify_invariant();

    // Checkout some.
    let p1 = storage.checkout_peer(1).unwrap();
    let p2 = storage.checkout_peer(2).unwrap();
    storage.verify_invariant();

    // Return one.
    storage.return_peer(&p1);
    storage.verify_invariant();

    // Try to add a returned peer again (should succeed since it was
    // removed from uniq_peers on return).
    let same_peer = make_peer(&p1.ip, p1.port);
    assert!(storage.add_peer(same_peer));
    storage.verify_invariant();

    // Return the other.
    storage.return_peer(&p2);
    storage.verify_invariant();
}

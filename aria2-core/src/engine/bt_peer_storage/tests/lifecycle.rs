//! Tests for peer checkout/return lifecycle and add_and_checkout_peer.

use crate::engine::bt_peer_storage::storage::DefaultPeerStorage;
use super::basic::make_peer;

// ------------------------------------------------------------------
// checkout_peer / return_peer lifecycle
// ------------------------------------------------------------------

#[test]
fn test_checkout_peer() {
    let mut storage = DefaultPeerStorage::new();
    storage.add_peer(make_peer("192.168.1.1", 6881));

    let peer = storage.checkout_peer(42);
    assert!(peer.is_some());
    let p = peer.unwrap();
    assert_eq!(p.ip, "192.168.1.1");
    assert_eq!(p.port, 6881);
    assert_eq!(p.used_by, 42);

    assert!(storage.unused_peers.is_empty());
    assert_eq!(storage.used_peers.len(), 1);
    assert!(!storage.is_peer_available());
    storage.verify_invariant();
}

#[test]
fn test_checkout_peer_empty_returns_none() {
    let mut storage = DefaultPeerStorage::new();
    assert!(storage.checkout_peer(1).is_none());
}

#[test]
fn test_return_peer() {
    let mut storage = DefaultPeerStorage::new();
    storage.add_peer(make_peer("192.168.1.1", 6881));
    let peer = storage.checkout_peer(42).unwrap();

    storage.return_peer(&peer);
    assert!(storage.used_peers.is_empty());
    assert!(storage.uniq_peers.is_empty());
    storage.verify_invariant();
}

#[test]
fn test_return_peer_not_in_used_warns() {
    let mut storage = DefaultPeerStorage::new();
    let peer = make_peer("192.168.1.1", 6881);
    // Return a peer that was never checked out -- should warn, not panic.
    storage.return_peer(&peer);
    assert!(storage.used_peers.is_empty());
}

#[test]
fn test_checkout_and_return_multiple() {
    let mut storage = DefaultPeerStorage::new();
    storage.add_peer(make_peer("10.0.0.1", 6881));
    storage.add_peer(make_peer("10.0.0.2", 6881));

    let p1 = storage.checkout_peer(1).unwrap();
    let p2 = storage.checkout_peer(2).unwrap();
    assert_eq!(storage.used_peers.len(), 2);

    storage.return_peer(&p1);
    assert_eq!(storage.used_peers.len(), 1);

    storage.return_peer(&p2);
    assert_eq!(storage.used_peers.len(), 0);
    storage.verify_invariant();
}

// ------------------------------------------------------------------
// add_and_checkout_peer
// ------------------------------------------------------------------

#[test]
fn test_add_and_checkout_peer_new() {
    let mut storage = DefaultPeerStorage::new();
    let peer = make_peer("192.168.1.1", 6881);

    let result = storage.add_and_checkout_peer(peer, 42);
    assert!(result.is_some());
    let p = result.unwrap();
    assert_eq!(p.ip, "192.168.1.1");
    assert_eq!(p.port, 6881);
    assert_eq!(p.used_by, 42);

    assert!(storage.unused_peers.is_empty());
    assert_eq!(storage.used_peers.len(), 1);
    storage.verify_invariant();
}

#[test]
fn test_add_and_checkout_peer_already_in_unused() {
    let mut storage = DefaultPeerStorage::new();
    storage.add_peer(make_peer("192.168.1.1", 6881));

    // The peer is in the unused list. addAndCheckout should move it
    // to front and check it out.
    let peer = make_peer("192.168.1.1", 6881);
    let result = storage.add_and_checkout_peer(peer, 99);
    assert!(result.is_some());
    let p = result.unwrap();
    assert_eq!(p.used_by, 99);

    assert!(storage.unused_peers.is_empty());
    assert_eq!(storage.used_peers.len(), 1);
    storage.verify_invariant();
}

#[test]
fn test_add_and_checkout_peer_already_in_used() {
    let mut storage = DefaultPeerStorage::new();
    storage.add_peer(make_peer("192.168.1.1", 6881));
    let _checked = storage.checkout_peer(1).unwrap();

    // The peer is already in used_peers -- cannot checkout again.
    let peer = make_peer("192.168.1.1", 6881);
    let result = storage.add_and_checkout_peer(peer, 2);
    assert!(result.is_none());
    storage.verify_invariant();
}

#[test]
fn test_add_and_checkout_peer_temporarily_rejected() {
    let mut storage = DefaultPeerStorage::new();
    storage.reject_peer_temporarily("192.168.1.1");

    let peer = make_peer("192.168.1.1", 6881);
    let result = storage.add_and_checkout_peer(peer, 1);
    assert!(result.is_none());
}

// ------------------------------------------------------------------
// delete_unused_peers
// ------------------------------------------------------------------

#[test]
fn test_delete_unused_peers() {
    let mut storage = DefaultPeerStorage::new();
    storage.add_peer(make_peer("10.0.0.1", 6881));
    storage.add_peer(make_peer("10.0.0.2", 6881));
    storage.add_peer(make_peer("10.0.0.3", 6881));

    storage.delete_unused_peers(2);
    assert_eq!(storage.unused_peers.len(), 1);
    assert_eq!(
        storage.unused_peers[0].ip, "10.0.0.1",
        "should keep front peers"
    );
    storage.verify_invariant();
}

#[test]
fn test_delete_unused_peers_excess() {
    let mut storage = DefaultPeerStorage::new();
    storage.add_peer(make_peer("10.0.0.1", 6881));

    // Deleting more than available should just empty the list.
    storage.delete_unused_peers(5);
    assert!(storage.unused_peers.is_empty());
    storage.verify_invariant();
}

// ------------------------------------------------------------------
// on_erasing_peer / on_returning_peer (public lifecycle callbacks)
// ------------------------------------------------------------------

#[test]
fn test_on_erasing_peer_removes_from_uniq() {
    let mut storage = DefaultPeerStorage::new();
    let peer = make_peer("192.168.1.1", 6881);
    storage.add_peer(peer.clone());
    assert!(
        storage
            .uniq_peers
            .contains(&("192.168.1.1".to_string(), 6881))
    );

    storage.on_erasing_peer(&peer);
    assert!(
        !storage
            .uniq_peers
            .contains(&("192.168.1.1".to_string(), 6881))
    );
}

#[test]
fn test_on_returning_peer_adds_to_dropped() {
    let mut storage = DefaultPeerStorage::new();
    let mut peer = make_peer("192.168.1.1", 6881);
    peer.is_active = true;
    peer.disconnected_gracefully = true;
    peer.is_incoming = false;

    storage.on_returning_peer(&peer);
    assert_eq!(
        storage.dropped_peers.len(),
        1,
        "graceful outgoing peer should be added to dropped list"
    );
}

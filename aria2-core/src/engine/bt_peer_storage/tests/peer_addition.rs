//! Tests for peer addition and duplicate/max-size rejection.

use super::basic::make_peer;
use crate::engine::bt_peer_storage::storage::DefaultPeerStorage;

// ------------------------------------------------------------------
// add_peer / add_peers
// ------------------------------------------------------------------

#[test]
fn test_add_peer_single() {
    let mut storage = DefaultPeerStorage::new();
    let peer = make_peer("192.168.1.1", 6881);

    assert!(storage.add_peer(peer));
    assert!(storage.is_peer_available());
    assert_eq!(storage.unused_peers.len(), 1);
    assert_eq!(storage.count_all_peers(), 1);
    assert!(
        storage
            .uniq_peers
            .contains(&("192.168.1.1".to_string(), 6881))
    );
    storage.verify_invariant();
}

#[test]
fn test_add_peers_batch() {
    let mut storage = DefaultPeerStorage::new();
    let peers = vec![
        make_peer("10.0.0.1", 6881),
        make_peer("10.0.0.2", 6881),
        make_peer("10.0.0.3", 6881),
    ];

    storage.add_peers(peers);
    assert_eq!(storage.unused_peers.len(), 3);
    assert_eq!(storage.count_all_peers(), 3);
    storage.verify_invariant();
}

#[test]
fn test_add_peers_batch_with_duplicates() {
    let mut storage = DefaultPeerStorage::new();
    let peers = vec![
        make_peer("10.0.0.1", 6881),
        make_peer("10.0.0.1", 6881), // duplicate
        make_peer("10.0.0.2", 6881),
    ];

    storage.add_peers(peers);
    assert_eq!(storage.unused_peers.len(), 2, "duplicate should be skipped");
    storage.verify_invariant();
}

// ------------------------------------------------------------------
// Duplicate peer rejection
// ------------------------------------------------------------------

#[test]
fn test_add_peer_rejects_duplicate() {
    let mut storage = DefaultPeerStorage::new();
    let peer1 = make_peer("192.168.1.1", 6881);
    let peer2 = make_peer("192.168.1.1", 6881); // same ip:port

    assert!(storage.add_peer(peer1));
    assert!(!storage.add_peer(peer2), "duplicate should be rejected");
    assert_eq!(storage.unused_peers.len(), 1);
    storage.verify_invariant();
}

#[test]
fn test_add_peer_rejects_duplicate_even_if_checked_out() {
    let mut storage = DefaultPeerStorage::new();
    let peer = make_peer("192.168.1.1", 6881);

    assert!(storage.add_peer(peer));
    let checked = storage.checkout_peer(1).unwrap();
    assert!(storage.used_peers.contains(&checked));

    // Trying to add the same ip:port while it's in used_peers should fail.
    let peer2 = make_peer("192.168.1.1", 6881);
    assert!(!storage.add_peer(peer2));
    storage.verify_invariant();
}

// ------------------------------------------------------------------
// max_peer_list_size limit
// ------------------------------------------------------------------

#[test]
fn test_add_peer_respects_max_list_size() {
    let mut storage = DefaultPeerStorage::new();
    storage.set_max_peer_list_size(3);

    assert!(storage.add_peer(make_peer("10.0.0.1", 6881)));
    assert!(storage.add_peer(make_peer("10.0.0.2", 6881)));
    assert!(storage.add_peer(make_peer("10.0.0.3", 6881)));
    assert!(
        !storage.add_peer(make_peer("10.0.0.4", 6881)),
        "should reject when full"
    );

    assert_eq!(storage.unused_peers.len(), 3);
    storage.verify_invariant();
}

#[test]
fn test_add_peers_evicts_excess() {
    let mut storage = DefaultPeerStorage::new();
    storage.set_max_peer_list_size(2);

    // Add 3 peers at once -- only 2 should remain.
    let peers = vec![
        make_peer("10.0.0.1", 6881),
        make_peer("10.0.0.2", 6881),
        make_peer("10.0.0.3", 6881),
    ];
    storage.add_peers(peers);

    assert_eq!(
        storage.unused_peers.len(),
        2,
        "excess peers should be evicted"
    );
    storage.verify_invariant();
}

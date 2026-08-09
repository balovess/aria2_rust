//! Tests for dropped peer tracking.

use crate::engine::bt_peer_storage::constants::MAX_DROPPED_PEERS;
use crate::engine::bt_peer_storage::peer_entry::PeerEntry;
use crate::engine::bt_peer_storage::storage::DefaultPeerStorage;

#[test]
fn test_return_peer_adds_to_dropped_on_graceful_disconnect() {
    let mut storage = DefaultPeerStorage::new();
    let peer = PeerEntry {
        ip: "192.168.1.1".to_string(),
        port: 6881,
        used_by: 0,
        is_active: true,
        am_choking: true,
        peer_interested: false,
        is_incoming: false,
        disconnected_gracefully: true,
    };

    storage.add_peer(peer);
    let checked = storage.checkout_peer(1).unwrap();
    storage.return_peer(&checked);

    assert_eq!(storage.dropped_peers.len(), 1);
    assert_eq!(storage.dropped_peers[0].ip, "192.168.1.1");
    assert_eq!(storage.dropped_peers[0].used_by, 0);
}

#[test]
fn test_return_peer_no_drop_for_incoming() {
    let mut storage = DefaultPeerStorage::new();
    let peer = PeerEntry {
        ip: "192.168.1.1".to_string(),
        port: 6881,
        used_by: 0,
        is_active: true,
        am_choking: true,
        peer_interested: false,
        is_incoming: true, // incoming -- should not be dropped
        disconnected_gracefully: true,
    };

    storage.add_peer(peer);
    let checked = storage.checkout_peer(1).unwrap();
    storage.return_peer(&checked);

    assert!(
        storage.dropped_peers.is_empty(),
        "incoming peers should not be dropped"
    );
}

#[test]
fn test_return_peer_no_drop_for_not_graceful() {
    let mut storage = DefaultPeerStorage::new();
    let peer = PeerEntry {
        ip: "192.168.1.1".to_string(),
        port: 6881,
        used_by: 0,
        is_active: true,
        am_choking: true,
        peer_interested: false,
        is_incoming: false,
        disconnected_gracefully: false, // not graceful
    };

    storage.add_peer(peer);
    let checked = storage.checkout_peer(1).unwrap();
    storage.return_peer(&checked);

    assert!(
        storage.dropped_peers.is_empty(),
        "non-graceful disconnect should not be dropped"
    );
}

#[test]
fn test_dropped_peers_max_50() {
    let mut storage = DefaultPeerStorage::new();

    for i in 0..60u16 {
        let peer = PeerEntry {
            ip: format!("10.0.0.{}", i),
            port: 6881,
            used_by: 0,
            is_active: true,
            am_choking: true,
            peer_interested: false,
            is_incoming: false,
            disconnected_gracefully: true,
        };
        storage.add_peer(peer);
        let checked = storage.checkout_peer(i as u64 + 1).unwrap();
        storage.return_peer(&checked);
    }

    assert_eq!(
        storage.dropped_peers.len(),
        MAX_DROPPED_PEERS,
        "dropped list should be capped at {}",
        MAX_DROPPED_PEERS
    );
}

#[test]
fn test_dropped_peers_dedup() {
    let mut storage = DefaultPeerStorage::new();

    // Add, checkout, return the same peer twice.
    for _ in 0..2 {
        let peer = PeerEntry {
            ip: "192.168.1.1".to_string(),
            port: 6881,
            used_by: 0,
            is_active: true,
            am_choking: true,
            peer_interested: false,
            is_incoming: false,
            disconnected_gracefully: true,
        };
        storage.add_peer(peer);
        let checked = storage.checkout_peer(1).unwrap();
        storage.return_peer(&checked);
    }

    // Only one dropped entry should exist (dedup by ip:port).
    assert_eq!(storage.dropped_peers.len(), 1);
}

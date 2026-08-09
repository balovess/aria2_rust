//! Basic tests: empty state, helpers, compile-time trait bounds.

use crate::engine::bt_peer_storage::constants::MAX_PEER_LIST_SIZE;
use crate::engine::bt_peer_storage::peer_entry::PeerEntry;
use crate::engine::bt_peer_storage::peer_storage_trait::PeerStorage;
use crate::engine::bt_peer_storage::storage::DefaultPeerStorage;

// Compile-time verification: DefaultPeerStorage satisfies PeerStorage's
// Send + Sync bounds, so Arc<dyn PeerStorage> is constructible.
const _: () = {
    fn _assert_send_sync() {
        fn _check<T: PeerStorage>() {}
        _check::<DefaultPeerStorage>();
    }
};

/// Helper to create a simple PeerEntry for tests.
pub(super) fn make_peer(ip: &str, port: u16) -> PeerEntry {
    PeerEntry::new(ip.to_string(), port)
}

// ------------------------------------------------------------------
// new() / empty state
// ------------------------------------------------------------------

#[test]
fn test_new_is_empty() {
    let storage = DefaultPeerStorage::new();
    assert!(storage.unused_peers.is_empty());
    assert!(storage.used_peers.is_empty());
    assert!(storage.dropped_peers.is_empty());
    assert!(storage.uniq_peers.is_empty());
    assert!(!storage.is_peer_available());
    assert_eq!(storage.count_all_peers(), 0);
    assert_eq!(storage.max_peer_list_size, MAX_PEER_LIST_SIZE);
    assert!(!storage.download_finished);
    storage.verify_invariant();
}

// ------------------------------------------------------------------
// count_all_peers
// ------------------------------------------------------------------

#[test]
fn test_count_all_peers() {
    let mut storage = DefaultPeerStorage::new();
    assert_eq!(storage.count_all_peers(), 0);

    storage.add_peer(make_peer("10.0.0.1", 6881));
    storage.add_peer(make_peer("10.0.0.2", 6881));
    assert_eq!(storage.count_all_peers(), 2);

    let _p1 = storage.checkout_peer(1);
    assert_eq!(
        storage.count_all_peers(),
        2,
        "checkout moves from unused to used, total unchanged"
    );
}

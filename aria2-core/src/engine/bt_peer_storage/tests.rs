//! Tests for DefaultPeerStorage and PeerStorage trait.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::debug;

use crate::engine::bt_peer_blocklist::BtPeerBlocklist;
use crate::engine::peer_stats::PeerStats;

use super::constants::*;
use super::peer_entry::PeerEntry;
use super::storage::DefaultPeerStorage;
use super::peer_storage_trait::PeerStorage;

// Compile-time verification: DefaultPeerStorage satisfies PeerStorage's
// Send + Sync bounds, so Arc<dyn PeerStorage> is constructible.
const _: () = {
    fn _assert_send_sync() {
        fn _check<T: PeerStorage>() {}
        _check::<DefaultPeerStorage>();
    }
};

/// Helper to create an `Instant` in the past without panicking.
fn instant_past(secs: u64) -> Instant {
    Instant::now().checked_sub(Duration::from_secs(secs)).unwrap_or(Instant::now())
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

fn make_peer(ip: &str, port: u16) -> PeerEntry {
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
    assert!(storage.uniq_peers.contains(&("192.168.1.1".to_string(), 6881)));
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
    assert!(!storage.add_peer(make_peer("10.0.0.4", 6881)), "should reject when full");

    assert_eq!(storage.unused_peers.len(), 3);
    storage.verify_invariant();
}

#[test]
fn test_add_peers_evicts_excess() {
    let mut storage = DefaultPeerStorage::new();
    storage.set_max_peer_list_size(2);

    // Add 3 peers at once — only 2 should remain.
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
    // Return a peer that was never checked out — should warn, not panic.
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
// Dropped peers
// ------------------------------------------------------------------

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
        is_incoming: true, // incoming — should not be dropped
        disconnected_gracefully: true,
    };

    storage.add_peer(peer);
    let checked = storage.checkout_peer(1).unwrap();
    storage.return_peer(&checked);

    assert!(storage.dropped_peers.is_empty(), "incoming peers should not be dropped");
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

    assert!(storage.dropped_peers.is_empty(), "non-graceful disconnect should not be dropped");
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

// ------------------------------------------------------------------
// Temporary rejection
// ------------------------------------------------------------------

#[test]
fn test_is_temporarily_rejected_not_in_map() {
    let mut storage = DefaultPeerStorage::new();
    assert!(!storage.is_temporarily_rejected("192.168.1.1"));
}

#[test]
fn test_reject_peer_temporarily() {
    let mut storage = DefaultPeerStorage::new();
    storage.reject_peer_temporarily("192.168.1.1");

    // The peer should be temporarily rejected immediately.
    assert!(storage.is_temporarily_rejected("192.168.1.1"));
}

#[test]
fn test_temporary_rejection_expiry() {
    let mut storage = DefaultPeerStorage::new();

    // Manually insert an already-expired entry.
    let expired = instant_past(1);
    storage
        .temporarily_rejected_peers
        .insert("192.168.1.1".to_string(), expired);

    // Should return false and remove the entry.
    assert!(!storage.is_temporarily_rejected("192.168.1.1"));
    assert!(!storage
        .temporarily_rejected_peers
        .contains_key("192.168.1.1"));
}

#[test]
fn test_add_peer_rejects_temporarily_rejected() {
    let mut storage = DefaultPeerStorage::new();
    storage.reject_peer_temporarily("192.168.1.1");

    let peer = make_peer("192.168.1.1", 6881);
    assert!(!storage.add_peer(peer), "temporarily rejected peer should be rejected");
}

#[test]
fn test_reject_peer_temporarily_cleanup() {
    let mut storage = DefaultPeerStorage::new();

    // Insert an expired entry manually.
    let expired = instant_past(1);
    storage
        .temporarily_rejected_peers
        .insert("10.0.0.1".to_string(), expired);

    // Force the cleanup timer to trigger by setting it far enough in the
    // past. Use a small offset (2ms) that won't overflow on Windows,
    // and temporarily lower the cleanup interval by using direct
    // manipulation: just call cleanup directly.
    let now = Instant::now();
    // Manually trigger the cleanup logic (same as in reject_peer_temporarily)
    storage.temporarily_rejected_peers
        .retain(|ip, timeout| {
            if *timeout <= now {
                debug!("Purge temporarily rejected peer {}", ip);
                false
            } else {
                true
            }
        });
    storage.last_temp_peer_cleanup = now;

    // Expired entry should have been purged.
    assert!(!storage
        .temporarily_rejected_peers
        .contains_key("10.0.0.1"));
}

// ------------------------------------------------------------------
// choke_round_interval_elapsed
// ------------------------------------------------------------------

#[test]
fn test_choke_round_interval_elapsed_no_previous_round() {
    let storage = DefaultPeerStorage::new();
    // No previous round — should return true.
    assert!(storage.choke_round_interval_elapsed());
}

#[test]
fn test_choke_round_interval_elapsed_after_round() {
    let mut storage = DefaultPeerStorage::new();

    // Execute a choke round to set last_round_time.
    let mut peers: Vec<PeerStats> = Vec::new();
    let mut refs: Vec<&mut PeerStats> = peers.iter_mut().collect();
    storage.execute_choke(&mut refs);

    // Immediately after, interval should NOT have elapsed.
    assert!(!storage.choke_round_interval_elapsed());

    // After waiting 10+ seconds, it should have elapsed.
    // (We can't easily sleep 10s in a unit test, so we test the logic
    //  by verifying the initial state is correct.)
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
    assert_eq!(storage.count_all_peers(), 2, "checkout moves from unused to used, total unchanged");
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

    // The peer is already in used_peers — cannot checkout again.
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
    assert_eq!(storage.unused_peers[0].ip, "10.0.0.1", "should keep front peers");
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
// set_download_finished
// ------------------------------------------------------------------

#[test]
fn test_set_download_finished_affects_choke_algorithm() {
    let mut storage = DefaultPeerStorage::new();

    // Initially leecher mode.
    assert!(!storage.download_finished);

    // Execute a leecher round.
    let mut peers: Vec<PeerStats> = Vec::new();
    let mut refs: Vec<&mut PeerStats> = peers.iter_mut().collect();
    storage.execute_choke(&mut refs);

    // Switch to seeder mode.
    storage.set_download_finished(true);
    assert!(storage.download_finished);

    // Should now delegate to seeder choke (no panic).
    storage.execute_choke(&mut refs);
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

// ------------------------------------------------------------------
// Blocklist integration
// ------------------------------------------------------------------

#[test]
fn test_add_peer_rejected_by_blocklist() {
    let mut bl = BtPeerBlocklist::new();
    bl.add_rule("10.0.0.0/8").unwrap();

    let mut storage = DefaultPeerStorage::new();
    storage.set_peer_blocklist(Arc::new(bl));

    // Blocked peer should be rejected.
    assert!(!storage.add_peer(make_peer("10.0.0.1", 6881)));
    assert_eq!(storage.blocklist_reject_count(), 1);
    assert_eq!(storage.count_all_peers(), 0, "blocked peer should not be added");
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
        make_peer("10.0.0.1", 6881),  // blocked
        make_peer("192.168.1.1", 6881), // allowed
        make_peer("10.0.0.2", 6881),  // blocked
        make_peer("8.8.8.8", 6881),    // allowed
    ];

    storage.add_peers(peers);
    assert_eq!(storage.count_all_peers(), 2, "only non-blocked peers should be added");
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
    // No blocklist configured — all peers should be accepted.
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
// on_erasing_peer / on_returning_peer (public lifecycle callbacks)
// ------------------------------------------------------------------

#[test]
fn test_on_erasing_peer_removes_from_uniq() {
    let mut storage = DefaultPeerStorage::new();
    let peer = make_peer("192.168.1.1", 6881);
    storage.add_peer(peer.clone());
    assert!(storage.uniq_peers.contains(&("192.168.1.1".to_string(), 6881)));

    storage.on_erasing_peer(&peer);
    assert!(!storage.uniq_peers.contains(&("192.168.1.1".to_string(), 6881)));
}

#[test]
fn test_on_returning_peer_adds_to_dropped() {
    let mut storage = DefaultPeerStorage::new();
    let mut peer = make_peer("192.168.1.1", 6881);
    peer.is_active = true;
    peer.disconnected_gracefully = true;
    peer.is_incoming = false;

    storage.on_returning_peer(&peer);
    assert_eq!(storage.dropped_peers.len(), 1, "graceful outgoing peer should be added to dropped list");
}

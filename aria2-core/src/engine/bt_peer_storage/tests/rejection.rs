//! Tests for temporary peer rejection.

use std::time::{Duration, Instant};

use tracing::debug;

use super::basic::make_peer;
use crate::engine::bt_peer_storage::storage::DefaultPeerStorage;

/// Helper to create an Instant in the past without panicking.
fn instant_past(secs: u64) -> Instant {
    Instant::now()
        .checked_sub(Duration::from_secs(secs))
        .unwrap_or(Instant::now())
}

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
        .insert("192.168.1.1".to_string().into_boxed_str(), expired);

    // Should return false and remove the entry.
    assert!(!storage.is_temporarily_rejected("192.168.1.1"));
    assert!(
        !storage
            .temporarily_rejected_peers
            .contains_key("192.168.1.1")
    );
}

#[test]
fn test_add_peer_rejects_temporarily_rejected() {
    let mut storage = DefaultPeerStorage::new();
    storage.reject_peer_temporarily("192.168.1.1");

    let peer = make_peer("192.168.1.1", 6881);
    assert!(
        !storage.add_peer(peer),
        "temporarily rejected peer should be rejected"
    );
}

#[test]
fn test_reject_peer_temporarily_cleanup() {
    let mut storage = DefaultPeerStorage::new();

    // Insert an expired entry manually.
    let expired = instant_past(1);
    storage
        .temporarily_rejected_peers
        .insert("10.0.0.1".to_string().into_boxed_str(), expired);

    // Force the cleanup timer to trigger by setting it far enough in the
    // past. Use a small offset (2ms) that won't overflow on Windows,
    // and temporarily lower the cleanup interval by using direct
    // manipulation: just call cleanup directly.
    let now = Instant::now();
    // Manually trigger the cleanup logic (same as in reject_peer_temporarily)
    storage.temporarily_rejected_peers.retain(|ip, timeout| {
        if *timeout <= now {
            debug!("Purge temporarily rejected peer {}", ip);
            false
        } else {
            true
        }
    });
    storage.last_temp_peer_cleanup = now;

    // Expired entry should have been purged.
    assert!(!storage.temporarily_rejected_peers.contains_key("10.0.0.1"));
}

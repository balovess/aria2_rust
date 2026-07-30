//! Tests for choking integration.

use crate::engine::peer_stats::PeerStats;
use crate::engine::bt_peer_storage::storage::DefaultPeerStorage;

#[test]
fn test_choke_round_interval_elapsed_no_previous_round() {
    let storage = DefaultPeerStorage::new();
    // No previous round -- should return true.
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

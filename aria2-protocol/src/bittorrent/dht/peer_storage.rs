use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{debug, trace};

/// Peers expire after 30 minutes per BEP 0005 recommendation.
const PEER_TTL: Duration = Duration::from_secs(30 * 60);

/// Maximum peers to store per info_hash to prevent unbounded growth.
const MAX_PEERS_PER_INFO_HASH: usize = 50;

/// A single announced peer with the timestamp of its last announcement.
struct PeerEntry {
    addr: SocketAddr,
    last_seen: Instant,
}

/// Stores peers announced via DHT `announce_peer` queries, keyed by info_hash.
///
/// Peers expire after 30 minutes (BEP 0005). Used to respond to `get_peers`
/// queries. Thread-safe via an internal `std::sync::Mutex`; operations are
/// brief and never span `.await` points, so a blocking mutex is appropriate.
pub struct DhtPeerStorage {
    inner: Mutex<HashMap<[u8; 20], Vec<PeerEntry>>>,
}

impl Default for DhtPeerStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl DhtPeerStorage {
    /// Create an empty peer storage.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Add or refresh a peer for the given info_hash using the current time.
    pub fn add_peer(&self, info_hash: [u8; 20], addr: SocketAddr) {
        self.add_peer_inner(info_hash, addr, Instant::now());
    }

    /// Get all non-expired peers for the given info_hash.
    ///
    /// Expired entries are opportunistically purged from the vector when they
    /// represent at least half of the stored peers, keeping memory bounded
    /// without waiting for the periodic cleanup pass.
    pub fn get_peers(&self, info_hash: &[u8; 20]) -> Vec<SocketAddr> {
        let mut map = self.inner.lock().expect("DhtPeerStorage mutex poisoned");
        let Some(entries) = map.get_mut(info_hash) else {
            return Vec::new();
        };
        let now = Instant::now();
        let expired_count = entries
            .iter()
            .filter(|e| now.duration_since(e.last_seen) >= PEER_TTL)
            .count();
        if expired_count > 0 && expired_count * 2 >= entries.len() {
            entries.retain(|e| now.duration_since(e.last_seen) < PEER_TTL);
            trace!(
                expired_count,
                "opportunistically purged expired peers during get_peers"
            );
        }
        entries
            .iter()
            .filter(|e| now.duration_since(e.last_seen) < PEER_TTL)
            .map(|e| e.addr)
            .collect()
    }

    /// Remove expired peers from all info_hashes. Called periodically by the
    /// maintenance loop. Empty info_hash entries are also removed.
    pub fn cleanup_expired(&self) {
        let mut map = self.inner.lock().expect("DhtPeerStorage mutex poisoned");
        let now = Instant::now();
        let mut total_removed = 0usize;
        map.retain(|_, entries| {
            let before = entries.len();
            entries.retain(|e| now.duration_since(e.last_seen) < PEER_TTL);
            let removed = before - entries.len();
            total_removed += removed;
            !entries.is_empty()
        });
        if total_removed > 0 {
            debug!(total_removed, "DhtPeerStorage cleanup complete");
        }
    }

    /// Get total peer count across all info_hashes (for stats/debugging).
    ///
    /// Includes entries that may have expired but not yet been purged; call
    /// [`cleanup_expired`](Self::cleanup_expired) first for an exact live count.
    pub fn total_peer_count(&self) -> usize {
        let map = self.inner.lock().expect("DhtPeerStorage mutex poisoned");
        map.values().map(Vec::len).sum()
    }

    /// Internal helper: insert or refresh a peer with an explicit timestamp.
    ///
    /// If the peer already exists for this info_hash its `last_seen` is updated.
    /// Otherwise a new entry is appended; when the vector exceeds
    /// `MAX_PEERS_PER_INFO_HASH` the oldest entry (smallest `last_seen`) is
    /// evicted.
    fn add_peer_inner(&self, info_hash: [u8; 20], addr: SocketAddr, last_seen: Instant) {
        let mut map = self.inner.lock().expect("DhtPeerStorage mutex poisoned");
        let entries = map.entry(info_hash).or_default();
        if let Some(entry) = entries.iter_mut().find(|e| e.addr == addr) {
            entry.last_seen = last_seen;
            trace!("refreshed existing DHT peer last_seen");
            return;
        }
        entries.push(PeerEntry { addr, last_seen });
        if entries.len() > MAX_PEERS_PER_INFO_HASH {
            // Evict the oldest entry to stay within the cap. The freshly added
            // entry has the newest timestamp so it is never the victim here.
            let oldest_idx = entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_seen)
                .map(|(i, _)| i);
            if let Some(idx) = oldest_idx {
                entries.remove(idx);
                debug!(
                    limit = MAX_PEERS_PER_INFO_HASH,
                    "evicted oldest DHT peer to enforce per-info_hash cap"
                );
            }
        }
    }

    /// Test-only helper to insert a peer with an explicit `last_seen` so expiry
    /// behavior can be exercised without waiting for real time to elapse.
    #[cfg(test)]
    fn add_peer_with_timestamp(&self, info_hash: [u8; 20], addr: SocketAddr, last_seen: Instant) {
        self.add_peer_inner(info_hash, addr, last_seen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_get_peer() {
        let storage = DhtPeerStorage::new();
        let info_hash = [0x11u8; 20];
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();

        storage.add_peer(info_hash, addr);

        let peers = storage.get_peers(&info_hash);
        assert_eq!(peers, vec![addr]);
    }

    #[test]
    fn test_get_peers_empty() {
        let storage = DhtPeerStorage::new();
        let info_hash = [0x22u8; 20];

        let peers = storage.get_peers(&info_hash);

        assert!(peers.is_empty(), "unknown info_hash should yield no peers");
    }

    #[test]
    fn test_add_peer_updates_existing() {
        let storage = DhtPeerStorage::new();
        let info_hash = [0x33u8; 20];
        let addr: SocketAddr = "127.0.0.1:9000".parse().unwrap();

        storage.add_peer(info_hash, addr);
        // Re-announcing the same peer must refresh, not duplicate.
        storage.add_peer(info_hash, addr);

        let peers = storage.get_peers(&info_hash);
        assert_eq!(peers.len(), 1, "duplicate add should not create a new entry");
        assert_eq!(peers, vec![addr]);
        assert_eq!(storage.total_peer_count(), 1);
    }

    #[test]
    fn test_peer_expiry() {
        let storage = DhtPeerStorage::new();
        let info_hash = [0xBBu8; 20];
        let addr: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        // 31 minutes ago: past the 30-minute TTL.
        let expired_ts = Instant::now() - Duration::from_secs(60 * 31);

        storage.add_peer_with_timestamp(info_hash, addr, expired_ts);

        let peers = storage.get_peers(&info_hash);
        assert!(
            peers.is_empty(),
            "peer older than TTL must not be returned"
        );
    }

    #[test]
    fn test_max_peers_limit() {
        let storage = DhtPeerStorage::new();
        let info_hash = [0xAAu8; 20];
        // First peer gets an explicitly older timestamp so it is the eviction
        // target regardless of clock resolution.
        let first_addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let old_ts = Instant::now() - Duration::from_secs(60);
        storage.add_peer_with_timestamp(info_hash, first_addr, old_ts);
        // Add enough peers to exceed the cap by one.
        for port in 5001u16..=(5000 + MAX_PEERS_PER_INFO_HASH as u16) {
            let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
            storage.add_peer(info_hash, addr);
        }

        let peers = storage.get_peers(&info_hash);

        assert_eq!(
            peers.len(),
            MAX_PEERS_PER_INFO_HASH,
            "peer count must be capped at MAX_PEERS_PER_INFO_HASH"
        );
        assert!(
            !peers.contains(&first_addr),
            "oldest peer should have been evicted"
        );
    }

    #[test]
    fn test_cleanup_expired() {
        let storage = DhtPeerStorage::new();
        let info_hash = [0xCCu8; 20];
        let expired_addr: SocketAddr = "127.0.0.1:7000".parse().unwrap();
        let fresh_addr: SocketAddr = "127.0.0.1:7001".parse().unwrap();
        let expired_ts = Instant::now() - Duration::from_secs(60 * 31);

        storage.add_peer_with_timestamp(info_hash, expired_addr, expired_ts);
        storage.add_peer(info_hash, fresh_addr);

        assert_eq!(
            storage.total_peer_count(),
            2,
            "before cleanup both peers are stored"
        );

        storage.cleanup_expired();

        assert_eq!(
            storage.total_peer_count(),
            1,
            "after cleanup only the fresh peer remains"
        );
        let peers = storage.get_peers(&info_hash);
        assert_eq!(peers, vec![fresh_addr]);
    }

    #[test]
    fn test_cleanup_removes_empty_info_hash() {
        let storage = DhtPeerStorage::new();
        let info_hash = [0xDDu8; 20];
        let addr: SocketAddr = "127.0.0.1:7100".parse().unwrap();
        let expired_ts = Instant::now() - Duration::from_secs(60 * 31);

        storage.add_peer_with_timestamp(info_hash, addr, expired_ts);
        assert_eq!(storage.total_peer_count(), 1);

        storage.cleanup_expired();

        assert_eq!(storage.total_peer_count(), 0);
        // The info_hash entry itself should be gone.
        assert!(storage.get_peers(&info_hash).is_empty());
    }

    #[test]
    fn test_multiple_info_hashes() {
        let storage = DhtPeerStorage::new();
        let hash_a = [0x01u8; 20];
        let hash_b = [0x02u8; 20];
        let addr_a: SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:8001".parse().unwrap();

        storage.add_peer(hash_a, addr_a);
        storage.add_peer(hash_b, addr_b);

        let peers_a = storage.get_peers(&hash_a);
        let peers_b = storage.get_peers(&hash_b);

        assert_eq!(peers_a, vec![addr_a], "hash_a should only contain addr_a");
        assert_eq!(peers_b, vec![addr_b], "hash_b should only contain addr_b");
        assert_eq!(storage.total_peer_count(), 2);
    }

    #[test]
    fn test_default_impl() {
        let storage = DhtPeerStorage::default();
        assert_eq!(storage.total_peer_count(), 0);
    }
}

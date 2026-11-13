use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{debug, trace};

/// Peers expire after 30 minutes per BEP 0005 recommendation.
const PEER_TTL: Duration = Duration::from_secs(30 * 60);

/// Maximum peers to store per info_hash to prevent unbounded growth.
const DEFAULT_MAX_PEERS_PER_INFO_HASH: usize = 50;

/// Maximum swarms retained by one DHT engine.
const DEFAULT_MAX_INFO_HASHES: usize = 4_096;

/// Resource limits for locally announced peers held by one DHT engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DhtPeerStorageConfig {
    /// Maximum distinct info-hashes retained by one address-family engine.
    pub max_info_hashes: usize,
    /// Maximum peer endpoints retained for each info-hash.
    pub max_peers_per_info_hash: usize,
}

impl Default for DhtPeerStorageConfig {
    fn default() -> Self {
        Self {
            max_info_hashes: DEFAULT_MAX_INFO_HASHES,
            max_peers_per_info_hash: DEFAULT_MAX_PEERS_PER_INFO_HASH,
        }
    }
}

fn normalize_config(config: DhtPeerStorageConfig) -> DhtPeerStorageConfig {
    DhtPeerStorageConfig {
        max_info_hashes: config.max_info_hashes.max(1),
        max_peers_per_info_hash: config.max_peers_per_info_hash.max(1),
    }
}

/// A single announced peer with the timestamp of its last announcement.
struct PeerEntry {
    addr: SocketAddr,
    last_seen: Instant,
}

struct SwarmEntry {
    peers: Vec<PeerEntry>,
    last_updated: Instant,
}

struct PeerStore {
    swarms: HashMap<[u8; 20], SwarmEntry>,
    least_recent: BTreeSet<(Instant, [u8; 20])>,
    evictions: u64,
    config: DhtPeerStorageConfig,
}

impl Default for PeerStore {
    fn default() -> Self {
        Self::with_config(DhtPeerStorageConfig::default())
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DhtPeerStorageStats {
    pub info_hashes: usize,
    pub peers: usize,
    pub evictions: u64,
    pub max_info_hashes: usize,
}

impl PeerStore {
    fn with_config(config: DhtPeerStorageConfig) -> Self {
        Self {
            swarms: HashMap::new(),
            least_recent: BTreeSet::new(),
            evictions: 0,
            config: normalize_config(config),
        }
    }

    fn set_config(&mut self, config: DhtPeerStorageConfig) {
        self.config = normalize_config(config);

        for swarm in self.swarms.values_mut() {
            while swarm.peers.len() > self.config.max_peers_per_info_hash {
                let oldest_idx = swarm
                    .peers
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, peer)| peer.last_seen)
                    .map(|(index, _)| index)
                    .expect("over-limit swarm must contain a peer");
                swarm.peers.remove(oldest_idx);
            }
        }
        self.swarms.retain(|_, swarm| !swarm.peers.is_empty());
        self.least_recent = self
            .swarms
            .iter()
            .map(|(info_hash, swarm)| (swarm.last_updated, *info_hash))
            .collect();

        while self.swarms.len() > self.config.max_info_hashes {
            let Some((_, oldest_hash)) = self.least_recent.pop_first() else {
                break;
            };
            self.swarms.remove(&oldest_hash);
            self.evictions = self.evictions.saturating_add(1);
        }
    }

    fn remove_swarm(&mut self, info_hash: &[u8; 20]) -> bool {
        let Some(entry) = self.swarms.remove(info_hash) else {
            return false;
        };
        self.least_recent.remove(&(entry.last_updated, *info_hash));
        true
    }

    fn remove_expired(&mut self, now: Instant) -> usize {
        let mut removed_peers = 0;
        let mut expired_hashes = Vec::new();
        for (info_hash, swarm) in &mut self.swarms {
            let before = swarm.peers.len();
            swarm
                .peers
                .retain(|peer| now.duration_since(peer.last_seen) < PEER_TTL);
            removed_peers += before - swarm.peers.len();
            if swarm.peers.is_empty() {
                expired_hashes.push(*info_hash);
            }
        }
        for info_hash in expired_hashes {
            self.remove_swarm(&info_hash);
        }
        removed_peers
    }
}

/// Stores peers announced via DHT `announce_peer` queries, keyed by info_hash.
///
/// Peers expire after 30 minutes (BEP 0005). Used to respond to `get_peers`
/// queries. Thread-safe via an internal `std::sync::Mutex`; operations are
/// brief and never span `.await` points, so a blocking mutex is appropriate.
pub struct DhtPeerStorage {
    inner: Mutex<PeerStore>,
}

impl Default for DhtPeerStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl DhtPeerStorage {
    /// Create an empty peer storage.
    pub fn new() -> Self {
        Self::with_config(DhtPeerStorageConfig::default())
    }

    /// Create peer storage with explicit memory bounds.
    ///
    /// Zero limits are normalized to one so the storage always has useful
    /// behavior and cannot silently disable peer discovery.
    pub fn with_config(config: DhtPeerStorageConfig) -> Self {
        Self {
            inner: Mutex::new(PeerStore::with_config(config)),
        }
    }

    /// Update storage bounds immediately, evicting least-recent data as needed.
    pub fn set_config(&self, config: DhtPeerStorageConfig) {
        self.inner
            .lock()
            .expect("DhtPeerStorage mutex poisoned")
            .set_config(config);
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
        let Some(swarm) = map.swarms.get_mut(info_hash) else {
            return Vec::new();
        };
        let now = Instant::now();
        let expired_count = swarm
            .peers
            .iter()
            .filter(|e| now.duration_since(e.last_seen) >= PEER_TTL)
            .count();
        if expired_count > 0 && expired_count * 2 >= swarm.peers.len() {
            swarm
                .peers
                .retain(|e| now.duration_since(e.last_seen) < PEER_TTL);
            trace!(
                expired_count,
                "opportunistically purged expired peers during get_peers"
            );
        }
        let peers = swarm
            .peers
            .iter()
            .filter(|e| now.duration_since(e.last_seen) < PEER_TTL)
            .map(|e| e.addr)
            .collect::<Vec<_>>();
        let is_empty = swarm.peers.is_empty();
        if is_empty {
            map.remove_swarm(info_hash);
        }
        peers
    }

    /// Remove expired peers from all info_hashes. Called periodically by the
    /// maintenance loop. Empty info_hash entries are also removed.
    pub fn cleanup_expired(&self) {
        let mut map = self.inner.lock().expect("DhtPeerStorage mutex poisoned");
        let total_removed = map.remove_expired(Instant::now());
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
        map.swarms.values().map(|swarm| swarm.peers.len()).sum()
    }

    pub(crate) fn stats(&self) -> DhtPeerStorageStats {
        let mut map = self.inner.lock().expect("DhtPeerStorage mutex poisoned");
        map.remove_expired(Instant::now());
        DhtPeerStorageStats {
            info_hashes: map.swarms.len(),
            peers: map.swarms.values().map(|swarm| swarm.peers.len()).sum(),
            evictions: map.evictions,
            max_info_hashes: map.config.max_info_hashes,
        }
    }

    /// Return the info-hash keys currently represented in the peer store.
    pub fn info_hashes(&self) -> Vec<[u8; 20]> {
        let mut map = self.inner.lock().expect("DhtPeerStorage mutex poisoned");
        map.remove_expired(Instant::now());
        map.swarms.keys().copied().collect()
    }

    /// Return a bounded random sample of stored info-hash keys.
    pub fn sample_info_hashes(&self, limit: usize) -> Vec<[u8; 20]> {
        self.sample_info_hashes_with_count(limit).1
    }

    /// Return the live swarm count and a uniform bounded sample in one pass.
    ///
    /// BEP 51 needs both values. Reservoir sampling keeps temporary storage
    /// proportional to the response sample size instead of all stored swarms.
    pub(crate) fn sample_info_hashes_with_count(&self, limit: usize) -> (usize, Vec<[u8; 20]>) {
        use rand::RngExt;

        let mut store = self.inner.lock().expect("DhtPeerStorage mutex poisoned");
        store.remove_expired(Instant::now());
        let count = store.swarms.len();
        let sample_size = limit.min(count);
        if sample_size == 0 {
            return (count, Vec::new());
        }
        let mut sample = Vec::with_capacity(sample_size);
        let mut rng = rand::rng();

        for (index, info_hash) in store.swarms.keys().copied().enumerate() {
            if index < sample_size {
                sample.push(info_hash);
            } else {
                let selected = rng.random_range(0..=index);
                if selected < sample_size {
                    sample[selected] = info_hash;
                }
            }
        }

        (count, sample)
    }

    /// Internal helper: insert or refresh a peer with an explicit timestamp.
    ///
    /// If the peer already exists for this info_hash its `last_seen` is updated.
    /// Otherwise a new entry is appended; when the vector exceeds
    /// the configured per-info-hash limit the oldest entry (smallest
    /// `last_seen`) is evicted.
    fn add_peer_inner(&self, info_hash: [u8; 20], addr: SocketAddr, last_seen: Instant) {
        let mut map = self.inner.lock().expect("DhtPeerStorage mutex poisoned");
        let config = map.config;
        if let Some(swarm) = map.swarms.get_mut(&info_hash) {
            let previous_update = swarm.last_updated;
            if let Some(entry) = swarm.peers.iter_mut().find(|e| e.addr == addr) {
                entry.last_seen = last_seen;
            } else {
                swarm.peers.push(PeerEntry { addr, last_seen });
                if swarm.peers.len() > config.max_peers_per_info_hash {
                    let oldest_idx = swarm
                        .peers
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, e)| e.last_seen)
                        .map(|(i, _)| i);
                    if let Some(idx) = oldest_idx {
                        swarm.peers.remove(idx);
                        debug!(
                            limit = config.max_peers_per_info_hash,
                            "evicted oldest DHT peer to enforce per-info_hash cap"
                        );
                    }
                }
            }
            swarm.last_updated = last_seen;
            map.least_recent.remove(&(previous_update, info_hash));
            map.least_recent.insert((last_seen, info_hash));
            trace!("refreshed existing DHT peer last_seen");
            return;
        }

        if map.swarms.len() == config.max_info_hashes
            && let Some((_, oldest_hash)) = map.least_recent.pop_first()
        {
            let removed = map.swarms.remove(&oldest_hash).is_some();
            debug_assert!(removed, "DHT swarm recency index must match peer map");
            map.evictions = map.evictions.saturating_add(1);
            debug!(
                limit = config.max_info_hashes,
                evicted_info_hash = %hex::encode(oldest_hash),
                "evicted least recently updated DHT swarm to enforce global cap"
            );
        }
        map.swarms.insert(
            info_hash,
            SwarmEntry {
                peers: vec![PeerEntry { addr, last_seen }],
                last_updated: last_seen,
            },
        );
        map.least_recent.insert((last_seen, info_hash));
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
        assert_eq!(
            peers.len(),
            1,
            "duplicate add should not create a new entry"
        );
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
        assert!(peers.is_empty(), "peer older than TTL must not be returned");
    }

    #[test]
    fn info_hashes_excludes_expired_peer_entries() {
        let storage = DhtPeerStorage::new();
        let info_hash = [0x42u8; 20];
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        storage.add_peer_with_timestamp(
            info_hash,
            addr,
            Instant::now() - PEER_TTL - Duration::from_secs(1),
        );

        assert!(storage.info_hashes().is_empty());
        assert!(storage.get_peers(&info_hash).is_empty());
    }

    #[test]
    fn test_max_peers_limit() {
        let storage = DhtPeerStorage::with_config(DhtPeerStorageConfig {
            max_info_hashes: 3,
            max_peers_per_info_hash: 3,
        });
        let info_hash = [0xAAu8; 20];
        // First peer gets an explicitly older timestamp so it is the eviction
        // target regardless of clock resolution.
        let first_addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let old_ts = Instant::now() - Duration::from_secs(60);
        storage.add_peer_with_timestamp(info_hash, first_addr, old_ts);
        // Add enough peers to exceed the cap by one.
        for port in 5001u16..=5003 {
            let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
            storage.add_peer(info_hash, addr);
        }

        let peers = storage.get_peers(&info_hash);

        assert_eq!(
            peers.len(),
            3,
            "peer count must respect the configured per-hash limit"
        );
        assert!(
            !peers.contains(&first_addr),
            "oldest peer should have been evicted"
        );
    }

    #[test]
    fn global_hash_limit_evicts_the_least_recently_updated_swarm() {
        let storage = DhtPeerStorage::with_config(DhtPeerStorageConfig {
            max_info_hashes: 3,
            max_peers_per_info_hash: 50,
        });
        let hash_for = |value: u64| {
            let mut hash = [0u8; 20];
            hash[..8].copy_from_slice(&value.to_be_bytes());
            hash
        };

        storage.add_peer_with_timestamp(
            hash_for(0),
            "127.0.0.1:9999"
                .parse()
                .expect("expired peer address should parse"),
            Instant::now() - PEER_TTL - Duration::from_secs(1),
        );
        storage.cleanup_expired();

        for value in 1..=3 {
            let addr: SocketAddr = format!("127.0.0.1:{}", 10_000 + value)
                .parse()
                .expect("fixture peer address should parse");
            storage.add_peer(hash_for(value), addr);
        }

        storage.add_peer(
            hash_for(1),
            "127.0.0.1:20000"
                .parse()
                .expect("refreshed peer address should parse"),
        );
        storage.add_peer(
            hash_for(4),
            "127.0.0.1:20001"
                .parse()
                .expect("new peer address should parse"),
        );

        let hashes = storage.info_hashes();
        assert_eq!(hashes.len(), 3);
        assert!(hashes.contains(&hash_for(1)), "refreshed swarm must remain");
        assert!(
            !hashes.contains(&hash_for(2)),
            "least recently updated swarm must be evicted"
        );
        assert!(hashes.contains(&hash_for(4)));
    }

    #[test]
    fn updating_limits_immediately_trims_oldest_peers_and_swarms() {
        let storage = DhtPeerStorage::with_config(DhtPeerStorageConfig {
            max_info_hashes: 4,
            max_peers_per_info_hash: 3,
        });
        let hash_for = |value: u8| [value; 20];
        let old = Instant::now() - Duration::from_secs(10);
        for value in 1..=3 {
            storage.add_peer_with_timestamp(
                hash_for(value),
                format!("127.0.0.1:{}", 6_000 + u16::from(value))
                    .parse()
                    .unwrap(),
                old + Duration::from_secs(u64::from(value)),
            );
        }
        let hash = hash_for(3);
        for port in 6_010..=6_011 {
            storage.add_peer(hash, format!("127.0.0.1:{port}").parse().unwrap());
        }

        storage.set_config(DhtPeerStorageConfig {
            max_info_hashes: 2,
            max_peers_per_info_hash: 2,
        });

        let hashes = storage.info_hashes();
        assert_eq!(hashes.len(), 2);
        assert!(!hashes.contains(&hash_for(1)));
        assert!(hashes.contains(&hash_for(2)));
        assert_eq!(storage.get_peers(&hash).len(), 2);
        assert_eq!(storage.stats().max_info_hashes, 2);
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

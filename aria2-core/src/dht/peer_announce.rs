//! DHT peer announce storage.
//!
//! Stores peer announcements received from the DHT network.
//! When a node responds to a get_peers query with peers,
//! those peers are stored here, keyed by info hash.
//!
//! Also manages periodic re-announcement of local torrents
//! to keep them visible on the DHT network.
//!
//! # Relationship to `DhtPeerStorage` (protocol layer)
//!
//! The protocol-layer `DhtPeerStorage` handles the *inbound* side:
//! storing peers from `announce_peer` queries to respond to `get_peers`.
//!
//! This module handles the *outbound* side: tracking which info hashes
//! the local node should announce, storing discovered peers from
//! `get_peers` lookups, and managing the re-announcement lifecycle.
//!
//! C++ reference: `DHTPeerAnnounceStorage.h/cc`, `DHTPeerAnnounceEntry.h/cc`

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::time::Instant;

use tracing::{debug, trace};

use super::constants::PEER_ANNOUNCE_PURGE_INTERVAL_SECS;
use super::node_id::NodeId;

// ---------------------------------------------------------------------------
// AnnouncedPeer
// ---------------------------------------------------------------------------

/// A peer that has been announced for an info hash.
///
/// Ordered by `addr` for use in `BTreeSet` (deduplication by address).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AnnouncedPeer {
    /// The peer's IP address and port.
    pub addr: SocketAddr,
    /// When this peer was last seen/announced.
    pub last_seen: Instant,
}

impl AnnouncedPeer {
    /// Create a new peer with `last_seen` set to now.
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            last_seen: Instant::now(),
        }
    }

    /// Update the last-seen timestamp to now.
    pub fn touch(&mut self) {
        self.last_seen = Instant::now();
    }

    /// Check if this peer entry has expired.
    pub fn is_expired(&self, timeout_secs: u64) -> bool {
        self.last_seen.elapsed() > std::time::Duration::from_secs(timeout_secs)
    }
}

/// Partial ordering by address for `BTreeSet`.
impl Ord for AnnouncedPeer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.addr.cmp(&other.addr)
    }
}

impl PartialOrd for AnnouncedPeer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------
// PeerAnnounceEntry
// ---------------------------------------------------------------------------

/// Entry for a single info hash, containing its announced peers.
///
/// C++ reference: `DHTPeerAnnounceEntry`
pub struct PeerAnnounceEntry {
    /// The info hash (20-byte key).
    info_hash: NodeId,
    /// Peers announced for this info hash, ordered by address.
    peers: BTreeSet<AnnouncedPeer>,
    /// When the last peer was added or the entry was touched.
    last_updated: Instant,
}

impl PeerAnnounceEntry {
    /// Create a new empty entry for the given info hash.
    pub fn new(info_hash: NodeId) -> Self {
        Self {
            info_hash,
            peers: BTreeSet::new(),
            last_updated: Instant::now(),
        }
    }

    /// Add a peer to this entry.
    ///
    /// If the peer already exists (same address), updates its last-seen timestamp.
    /// Returns `true` if a **new** peer was added, `false` if an existing one was refreshed.
    pub fn add_peer(&mut self, addr: SocketAddr) -> bool {
        // BTreeSet::take removes the element matching by Ord (i.e. by addr),
        // ignoring last_seen in the comparison.
        if let Some(mut existing) = self.peers.take(&AnnouncedPeer::new(addr)) {
            existing.touch();
            self.peers.insert(existing);
            self.last_updated = Instant::now();
            false
        } else {
            self.peers.insert(AnnouncedPeer::new(addr));
            self.last_updated = Instant::now();
            true
        }
    }

    /// Get all peer addresses for this entry.
    pub fn peer_addrs(&self) -> Vec<SocketAddr> {
        self.peers.iter().map(|p| p.addr).collect()
    }

    /// Get the number of peers.
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Check if this entry is empty (no peers).
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Remove expired peers. Returns the number of peers removed.
    pub fn purge_expired(&mut self, timeout_secs: u64) -> usize {
        let before = self.peers.len();
        self.peers.retain(|p| !p.is_expired(timeout_secs));
        before - self.peers.len()
    }

    /// Check if the entry itself is stale (no updates for `timeout_secs`).
    pub fn is_stale(&self, timeout_secs: u64) -> bool {
        self.last_updated.elapsed() > std::time::Duration::from_secs(timeout_secs)
    }

    /// Get the info hash.
    pub fn info_hash(&self) -> &NodeId {
        &self.info_hash
    }
}

// ---------------------------------------------------------------------------
// DhtPeerAnnounceStorage
// ---------------------------------------------------------------------------

/// Storage for DHT peer announcements, keyed by info hash.
///
/// Maintains a `BTreeMap` of info hash -> peer entries, supporting:
/// - Adding peers received from `get_peers` responses
/// - Looking up peers by info hash
/// - Periodic purging of stale entries
/// - Tracking which info hashes need re-announcement
///
/// C++ reference: `DHTPeerAnnounceStorage`
pub struct DhtPeerAnnounceStorage {
    /// Info hash -> peer announce entry.
    entries: BTreeMap<NodeId, PeerAnnounceEntry>,
    /// Info hashes that the local node wants to announce.
    /// These are the torrents we are actively downloading/seeding.
    local_info_hashes: BTreeSet<NodeId>,
    /// Timeout for purging stale entries (seconds).
    purge_timeout_secs: u64,
}

impl DhtPeerAnnounceStorage {
    /// Create a new peer announce storage with default purge timeout.
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            local_info_hashes: BTreeSet::new(),
            purge_timeout_secs: PEER_ANNOUNCE_PURGE_INTERVAL_SECS,
        }
    }

    /// Create a new peer announce storage with a custom purge timeout.
    pub fn with_purge_timeout(purge_timeout_secs: u64) -> Self {
        Self {
            entries: BTreeMap::new(),
            local_info_hashes: BTreeSet::new(),
            purge_timeout_secs,
        }
    }

    /// Add a peer announcement for an info hash.
    ///
    /// Called when a DHT node responds with peers for a `get_peers` query.
    pub fn add_peer_announce(&mut self, info_hash: &NodeId, addr: SocketAddr) {
        let entry = self
            .entries
            .entry(*info_hash)
            .or_insert_with(|| PeerAnnounceEntry::new(*info_hash));
        let added = entry.add_peer(addr);
        trace!(
            info_hash = %info_hash,
            addr = %addr,
            peer_count = entry.peer_count(),
            added,
            "Peer announced for info hash"
        );
    }

    /// Check if any peers have been announced for the given info hash.
    pub fn contains(&self, info_hash: &NodeId) -> bool {
        self.entries.contains_key(info_hash)
    }

    /// Get all peer addresses for an info hash.
    ///
    /// Returns an empty vector if no peers are known.
    pub fn get_peers(&self, info_hash: &NodeId) -> Vec<SocketAddr> {
        self.entries
            .get(info_hash)
            .map(|e| e.peer_addrs())
            .unwrap_or_default()
    }

    /// Register a local info hash that should be announced.
    ///
    /// These are torrents the local node is participating in
    /// and should announce itself as a peer for.
    pub fn add_local_info_hash(&mut self, info_hash: NodeId) {
        self.local_info_hashes.insert(info_hash);
        debug!(info_hash = %info_hash, "Registered local info hash for DHT announcement");
    }

    /// Remove a local info hash.
    pub fn remove_local_info_hash(&mut self, info_hash: &NodeId) {
        self.local_info_hashes.remove(info_hash);
    }

    /// Get all local info hashes that need to be announced.
    pub fn local_info_hashes(&self) -> &BTreeSet<NodeId> {
        &self.local_info_hashes
    }

    /// Purge stale entries and expired peers.
    ///
    /// Called periodically to clean up old data.
    /// Returns the number of empty entries removed after purging.
    pub fn handle_timeout(&mut self) -> usize {
        // Purge expired peers from each entry
        for entry in self.entries.values_mut() {
            let removed = entry.purge_expired(self.purge_timeout_secs);
            if removed > 0 {
                debug!(
                    info_hash = %entry.info_hash,
                    removed,
                    "Purged expired peers from DHT announce entry"
                );
            }
        }

        // Remove entries with no remaining peers
        let before = self.entries.len();
        self.entries.retain(|_, e| !e.is_empty());
        let removed = before - self.entries.len();
        if removed > 0 {
            debug!(removed, "Purged empty DHT peer announce entries");
        }
        removed
    }

    /// Get the total number of entries.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Get the total number of peers across all entries.
    pub fn total_peer_count(&self) -> usize {
        self.entries.values().map(|e| e.peer_count()).sum()
    }

    /// Get the number of local info hashes registered for announcement.
    pub fn local_info_hash_count(&self) -> usize {
        self.local_info_hashes.len()
    }
}

impl Default for DhtPeerAnnounceStorage {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::constants::ID_LENGTH;
    use super::*;

    fn test_info_hash(byte: u8) -> NodeId {
        NodeId([byte; ID_LENGTH])
    }

    fn test_addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{}", port).parse().unwrap()
    }

    #[test]
    fn announced_peer_new_sets_last_seen() {
        let addr = test_addr(6881);
        let peer = AnnouncedPeer::new(addr);
        assert_eq!(peer.addr, addr);
        assert!(peer.last_seen.elapsed().as_secs() < 1);
    }

    #[test]
    fn announced_peer_touch_updates_timestamp() {
        let addr = test_addr(6881);
        let mut peer = AnnouncedPeer::new(addr);
        let before = peer.last_seen;
        peer.touch();
        assert!(peer.last_seen >= before);
    }

    #[test]
    fn announced_peer_not_expired_initially() {
        let peer = AnnouncedPeer::new(test_addr(6881));
        assert!(!peer.is_expired(1800));
    }

    #[test]
    fn announced_peer_ordering_by_addr() {
        let a = AnnouncedPeer::new(test_addr(6881));
        let b = AnnouncedPeer::new(test_addr(6882));
        assert!(a < b);
        assert!(b > a);
    }

    #[test]
    fn announced_peer_ord_by_addr_ignores_timestamp() {
        let addr = test_addr(6881);
        let p1 = AnnouncedPeer::new(addr);
        let p2 = AnnouncedPeer::new(addr);
        assert_eq!(p1.cmp(&p2), std::cmp::Ordering::Equal);
    }

    #[test]
    fn entry_new_is_empty() {
        let entry = PeerAnnounceEntry::new(test_info_hash(0x01));
        assert!(entry.is_empty());
        assert_eq!(entry.peer_count(), 0);
        assert_eq!(entry.peer_addrs(), Vec::<SocketAddr>::new());
    }

    #[test]
    fn entry_add_peer_new() {
        let mut entry = PeerAnnounceEntry::new(test_info_hash(0x01));
        let added = entry.add_peer(test_addr(6881));
        assert!(added);
        assert!(!entry.is_empty());
        assert_eq!(entry.peer_count(), 1);
    }

    #[test]
    fn entry_add_peer_duplicate_refreshes() {
        let mut entry = PeerAnnounceEntry::new(test_info_hash(0x01));
        let addr = test_addr(6881);
        assert!(entry.add_peer(addr));
        assert!(!entry.add_peer(addr));
        assert_eq!(entry.peer_count(), 1);
    }

    #[test]
    fn entry_add_multiple_peers() {
        let mut entry = PeerAnnounceEntry::new(test_info_hash(0x01));
        entry.add_peer(test_addr(6881));
        entry.add_peer(test_addr(6882));
        entry.add_peer(test_addr(6883));
        assert_eq!(entry.peer_count(), 3);
    }

    #[test]
    fn entry_peer_addrs_returns_all() {
        let mut entry = PeerAnnounceEntry::new(test_info_hash(0x01));
        let a1 = test_addr(6881);
        let a2 = test_addr(6882);
        entry.add_peer(a1);
        entry.add_peer(a2);
        let addrs = entry.peer_addrs();
        assert_eq!(addrs.len(), 2);
        assert!(addrs.contains(&a1));
        assert!(addrs.contains(&a2));
    }

    #[test]
    fn entry_purge_expired_removes_nothing_when_fresh() {
        let mut entry = PeerAnnounceEntry::new(test_info_hash(0x01));
        entry.add_peer(test_addr(6881));
        let removed = entry.purge_expired(1800);
        assert_eq!(removed, 0);
        assert_eq!(entry.peer_count(), 1);
    }

    #[test]
    fn entry_info_hash_accessor() {
        let ih = test_info_hash(0xAB);
        let entry = PeerAnnounceEntry::new(ih);
        assert_eq!(*entry.info_hash(), ih);
    }

    #[test]
    fn storage_new_is_empty() {
        let storage = DhtPeerAnnounceStorage::new();
        assert_eq!(storage.entry_count(), 0);
        assert_eq!(storage.total_peer_count(), 0);
        assert_eq!(storage.local_info_hash_count(), 0);
    }

    #[test]
    fn storage_default_matches_new() {
        let s1 = DhtPeerAnnounceStorage::new();
        let s2 = DhtPeerAnnounceStorage::default();
        assert_eq!(s1.entry_count(), s2.entry_count());
        assert_eq!(s1.total_peer_count(), s2.total_peer_count());
    }

    #[test]
    fn storage_add_peer_announce_creates_entry() {
        let mut storage = DhtPeerAnnounceStorage::new();
        let ih = test_info_hash(0x01);
        storage.add_peer_announce(&ih, test_addr(6881));
        assert!(storage.contains(&ih));
        assert_eq!(storage.entry_count(), 1);
        assert_eq!(storage.total_peer_count(), 1);
    }

    #[test]
    fn storage_add_peer_announce_multiple_peers_same_hash() {
        let mut storage = DhtPeerAnnounceStorage::new();
        let ih = test_info_hash(0x01);
        storage.add_peer_announce(&ih, test_addr(6881));
        storage.add_peer_announce(&ih, test_addr(6882));
        assert_eq!(storage.entry_count(), 1);
        assert_eq!(storage.total_peer_count(), 2);
    }

    #[test]
    fn storage_add_peer_announce_different_hashes() {
        let mut storage = DhtPeerAnnounceStorage::new();
        let ih1 = test_info_hash(0x01);
        let ih2 = test_info_hash(0x02);
        storage.add_peer_announce(&ih1, test_addr(6881));
        storage.add_peer_announce(&ih2, test_addr(6882));
        assert_eq!(storage.entry_count(), 2);
        assert_eq!(storage.total_peer_count(), 2);
    }

    #[test]
    fn storage_contains_unknown_hash() {
        let storage = DhtPeerAnnounceStorage::new();
        assert!(!storage.contains(&test_info_hash(0xFF)));
    }

    #[test]
    fn storage_get_peers_returns_addresses() {
        let mut storage = DhtPeerAnnounceStorage::new();
        let ih = test_info_hash(0x01);
        let a1 = test_addr(6881);
        let a2 = test_addr(6882);
        storage.add_peer_announce(&ih, a1);
        storage.add_peer_announce(&ih, a2);
        let peers = storage.get_peers(&ih);
        assert_eq!(peers.len(), 2);
        assert!(peers.contains(&a1));
        assert!(peers.contains(&a2));
    }

    #[test]
    fn storage_get_peers_unknown_hash_returns_empty() {
        let storage = DhtPeerAnnounceStorage::new();
        let peers = storage.get_peers(&test_info_hash(0xFF));
        assert!(peers.is_empty());
    }

    #[test]
    fn storage_dedup_by_address() {
        let mut storage = DhtPeerAnnounceStorage::new();
        let ih = test_info_hash(0x01);
        let addr = test_addr(6881);
        storage.add_peer_announce(&ih, addr);
        storage.add_peer_announce(&ih, addr);
        assert_eq!(storage.total_peer_count(), 1);
    }

    #[test]
    fn storage_local_info_hash_add_and_remove() {
        let mut storage = DhtPeerAnnounceStorage::new();
        let ih = test_info_hash(0x01);
        storage.add_local_info_hash(ih);
        assert_eq!(storage.local_info_hash_count(), 1);
        assert!(storage.local_info_hashes().contains(&ih));
        storage.remove_local_info_hash(&ih);
        assert_eq!(storage.local_info_hash_count(), 0);
        assert!(!storage.local_info_hashes().contains(&ih));
    }

    #[test]
    fn storage_local_info_hash_dedup() {
        let mut storage = DhtPeerAnnounceStorage::new();
        let ih = test_info_hash(0x01);
        storage.add_local_info_hash(ih);
        storage.add_local_info_hash(ih);
        assert_eq!(storage.local_info_hash_count(), 1);
    }

    #[test]
    fn storage_handle_timeout_removes_empty_entries() {
        let mut storage = DhtPeerAnnounceStorage::with_purge_timeout(0);
        let ih = test_info_hash(0x01);
        storage.add_peer_announce(&ih, test_addr(6881));
        std::thread::sleep(std::time::Duration::from_millis(1));
        let removed = storage.handle_timeout();
        assert_eq!(removed, 1);
        assert_eq!(storage.entry_count(), 0);
        assert_eq!(storage.total_peer_count(), 0);
    }

    #[test]
    fn storage_handle_timeout_preserves_fresh_entries() {
        let mut storage = DhtPeerAnnounceStorage::new();
        let ih = test_info_hash(0x01);
        storage.add_peer_announce(&ih, test_addr(6881));
        let removed = storage.handle_timeout();
        assert_eq!(removed, 0);
        assert_eq!(storage.entry_count(), 1);
    }

    #[test]
    fn storage_with_custom_purge_timeout() {
        let storage = DhtPeerAnnounceStorage::with_purge_timeout(600);
        assert_eq!(storage.entry_count(), 0);
        assert_eq!(storage.purge_timeout_secs, 600);
    }

    #[test]
    fn storage_multiple_hashes_partial_expiry() {
        let mut storage = DhtPeerAnnounceStorage::with_purge_timeout(0);
        let ih1 = test_info_hash(0x01);
        let ih2 = test_info_hash(0x02);
        storage.add_peer_announce(&ih1, test_addr(6881));
        storage.add_peer_announce(&ih2, test_addr(6882));
        std::thread::sleep(std::time::Duration::from_millis(1));
        let removed = storage.handle_timeout();
        assert_eq!(removed, 2);
        assert_eq!(storage.entry_count(), 0);
    }

    #[test]
    fn storage_get_peers_isolation_between_hashes() {
        let mut storage = DhtPeerAnnounceStorage::new();
        let ih1 = test_info_hash(0x01);
        let ih2 = test_info_hash(0x02);
        storage.add_peer_announce(&ih1, test_addr(6881));
        storage.add_peer_announce(&ih2, test_addr(6882));
        let peers1 = storage.get_peers(&ih1);
        let peers2 = storage.get_peers(&ih2);
        assert_eq!(peers1, vec![test_addr(6881)]);
        assert_eq!(peers2, vec![test_addr(6882)]);
    }
}

//! DefaultPeerStorage — Peer lifecycle management for BitTorrent.
//!
//! Manages the complete peer lifecycle: unused (discovered) peers → used
//! (connected) peers → dropped (recently disconnected) peers.
//!
//! # C++ Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/DefaultPeerStorage.h` / `src/DefaultPeerStorage.cc`
//!
//! # Key Data Structures
//!
//! | C++ aria2 | Rust | Rationale |
//! |---|---|---|
//! | `set<pair<string, uint16_t>>` | `HashSet<(String, u16)>` | Same dedup by (ip, port) |
//! | `deque<shared_ptr<Peer>>` | `VecDeque<PeerEntry>` | Same FIFO ordering |
//! | `PeerSet` (sorted by ptr) | `HashSet<PeerEntry>` | Identity by (ip, port) suffices |
//! | `map<string, Timer>` | `HashMap<String, Instant>` | Same ip → timeout mapping |
//! | `unique_ptr<BtSeederStateChoke>` | `BtSeederStateChoke` | Inline ownership |
//! | `unique_ptr<BtLeecherStateChoke>` | `BtLeecherStateChoke` | Inline ownership |

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::Rng;
use tracing::{debug, warn};

use crate::engine::bt_choke_manager::{BtLeecherStateChoke, BtSeederStateChoke};
use crate::engine::bt_peer_blocklist::BtPeerBlocklist;
use crate::engine::peer_stats::PeerStats;

use super::constants::*;
use super::peer_entry::PeerEntry;
use super::peer_storage_trait::PeerStorage;

/// Peer lifecycle storage, matching C++ `DefaultPeerStorage`.
///
/// Tracks peers through three stages:
/// 1. **Unused** — discovered but not yet connected (FIFO queue)
/// 2. **Used** — currently connected (set for O(1) lookup)
/// 3. **Dropped** — recently disconnected gracefully (bounded deque)
///
/// Additionally provides:
/// - Deduplication by `(ip, port)` via `uniq_peers`
/// - Temporary peer rejection with variable timeout
/// - Choking algorithm integration (seeder vs leecher)
///
/// # Invariant
///
/// `uniq_peers` always equals the union of keys in `unused_peers` and
/// `used_peers`. This is verified in the C++ destructor assertion.
pub struct DefaultPeerStorage {
    /// Maximum number of unused peers before rejection.
    pub(super) max_peer_list_size: usize,

    /// Set of (ip, port) pairs currently tracked (unused + used).
    /// Ensures no duplicate peers across both lists.
    pub(super) uniq_peers: HashSet<(String, u16)>,

    /// Unused (not connected) peers, sorted by last-added (FIFO).
    pub(super) unused_peers: VecDeque<PeerEntry>,

    /// Currently connected (used) peers.
    pub(super) used_peers: HashSet<PeerEntry>,

    /// Recently disconnected peers, bounded to MAX_DROPPED_PEERS.
    pub(super) dropped_peers: VecDeque<PeerEntry>,

    /// Choking algorithm for seeder state (when download is complete).
    seeder_state_choke: BtSeederStateChoke,

    /// Choking algorithm for leecher state (when download is in progress).
    leecher_state_choke: BtLeecherStateChoke,

    /// Temporarily rejected peers: ip → timeout instant.
    pub(super) temporarily_rejected_peers: HashMap<String, Instant>,

    /// Last time we cleaned up expired entries from `temporarily_rejected_peers`.
    pub(super) last_temp_peer_cleanup: Instant,

    /// Whether piece storage has been configured.
    piece_storage_available: bool,

    /// Whether the download has finished (determines seeder vs leecher choke).
    pub(super) download_finished: bool,

    /// IP range-based blocklist for rejecting peers by address.
    ///
    /// In C++ aria2, this is `shared_ptr<BtPeerBlocklist> peerBlocklist_`,
    /// always non-null (constructed with `make_shared`). Here we use
    /// `Option<Arc<>>` to allow construction without a blocklist and to
    /// support shared ownership with `BtRegistry`.
    peer_blocklist: Option<Arc<BtPeerBlocklist>>,

    /// Counter of peers rejected by the blocklist (for diagnostics).
    blocklist_reject_count: u64,
}

impl DefaultPeerStorage {
    /// Create a new `DefaultPeerStorage` with default settings.
    ///
    /// No blocklist is configured; use `set_peer_blocklist()` to attach one.
    pub fn new() -> Self {
        Self {
            max_peer_list_size: MAX_PEER_LIST_SIZE,
            uniq_peers: HashSet::new(),
            unused_peers: VecDeque::new(),
            used_peers: HashSet::new(),
            dropped_peers: VecDeque::new(),
            seeder_state_choke: BtSeederStateChoke::new(),
            leecher_state_choke: BtLeecherStateChoke::new(),
            temporarily_rejected_peers: HashMap::new(),
            last_temp_peer_cleanup: Instant::now(),
            piece_storage_available: false,
            download_finished: false,
            peer_blocklist: None,
            blocklist_reject_count: 0,
        }
    }

    // ==================================================================
    // Peer addition
    // ==================================================================

    /// Add a single peer to the unused list.
    ///
    /// Returns `true` if the peer was added, `false` if rejected.
    ///
    /// A peer is rejected if:
    /// - The unused list is full (`unused_peers.len() >= max_peer_list_size`)
    /// - The peer is already tracked (duplicate ip:port)
    /// - The peer IP is in the blocklist
    /// - The peer is temporarily rejected
    ///
    /// Matches C++ `DefaultPeerStorage::addPeer(shared_ptr<Peer>)`.
    pub fn add_peer(&mut self, peer: PeerEntry) -> bool {
        let key = (peer.ip.clone(), peer.port);

        if self.unused_peers.len() >= self.max_peer_list_size {
            debug!(
                "Adding {}:{} rejected: unused list full ({}/{}",
                peer.ip, peer.port, self.unused_peers.len(), self.max_peer_list_size
            );
            return false;
        }

        if self.uniq_peers.contains(&key) {
            debug!(
                "Adding {}:{} rejected: already tracked",
                peer.ip, peer.port
            );
            return false;
        }

        if self.is_blocked_by_blocklist(&peer.ip) {
            debug!(
                "Adding {}:{} rejected: blocklisted",
                peer.ip, peer.port
            );
            self.blocklist_reject_count += 1;
            return false;
        }

        if self.is_temporarily_rejected(&peer.ip) {
            debug!("Adding {}:{} rejected: temporarily rejected", peer.ip, peer.port);
            return false;
        }

        // If list would overflow, evict from the back first.
        if self.unused_peers.len() >= self.max_peer_list_size {
            let excess = self.unused_peers.len() - self.max_peer_list_size + 1;
            self.delete_unused_peers(excess);
        }

        self.unused_peers.push_back(peer);
        self.uniq_peers.insert(key);
        debug!(
            "Added peer, unused list now has {} peers",
            self.unused_peers.len()
        );
        true
    }

    /// Add multiple peers to the unused list.
    ///
    /// If the unused list is already full before this call, all peers
    /// are rejected. Otherwise, each peer is individually checked for
    /// duplicates, blocklist membership, and temporary rejection before
    /// being added. After all additions, excess peers are evicted from
    /// the back.
    ///
    /// Matches C++ `DefaultPeerStorage::addPeer(vector<shared_ptr<Peer>>)`.
    pub fn add_peers(&mut self, peers: Vec<PeerEntry>) {
        if self.unused_peers.len() < self.max_peer_list_size {
            for peer in peers {
                let key = (peer.ip.clone(), peer.port);

                if self.uniq_peers.contains(&key) {
                    debug!(
                        "Adding {}:{} rejected: already tracked",
                        peer.ip, peer.port
                    );
                    continue;
                }

                if self.is_blocked_by_blocklist(&peer.ip) {
                    debug!(
                        "Adding {}:{} rejected: blocklisted",
                        peer.ip, peer.port
                    );
                    self.blocklist_reject_count += 1;
                    continue;
                }

                if self.is_temporarily_rejected(&peer.ip) {
                    debug!("Adding {}:{} rejected: temporarily rejected", peer.ip, peer.port);
                    continue;
                }

                debug!("Adding peer {}:{}", peer.ip, peer.port);
                self.unused_peers.push_back(peer);
                self.uniq_peers.insert(key);
            }
        } else {
            for peer in &peers {
                debug!(
                    "Adding {}:{} rejected: unused list full ({}/{}",
                    peer.ip, peer.port, self.unused_peers.len(), self.max_peer_list_size
                );
            }
        }

        // Evict excess peers from the back.
        if self.unused_peers.len() > self.max_peer_list_size {
            let excess = self.unused_peers.len() - self.max_peer_list_size;
            self.delete_unused_peers(excess);
        }

        debug!(
            "After batch add, unused list has {} peers",
            self.unused_peers.len()
        );
    }

    /// Atomically add a peer and check it out.
    ///
    /// If the peer is blocked by the blocklist or temporarily rejected,
    /// returns `None`. If the peer is already tracked and in the unused
    /// list, it is moved to the front for immediate checkout. If already
    /// in the used list, returns `None`.
    /// If the peer is new, it is added to the front of the unused list
    /// and then checked out.
    ///
    /// Matches C++ `DefaultPeerStorage::addAndCheckoutPeer`.
    pub fn add_and_checkout_peer(&mut self, peer: PeerEntry, cuid: u64) -> Option<PeerEntry> {
        let key = (peer.ip.clone(), peer.port);

        if self.is_blocked_by_blocklist(&peer.ip) {
            debug!("addAndCheckout: {}:{} rejected: blocklisted", peer.ip, peer.port);
            self.blocklist_reject_count += 1;
            return None;
        }

        if self.is_temporarily_rejected(&peer.ip) {
            debug!("addAndCheckout: {}:{} rejected: temporarily rejected", peer.ip, peer.port);
            return None;
        }

        if self.uniq_peers.contains(&key) {
            // Peer already tracked. Try to find in unused list.
            let pos = self
                .unused_peers
                .iter()
                .position(|p| p.ip == peer.ip && p.port == peer.port);

            if let Some(idx) = pos {
                // Remove from unused list; we'll push to front below.
                self.unused_peers.remove(idx);
            } else {
                // Peer is in used_peers — cannot checkout.
                return None;
            }
        } else {
            // New peer — register in uniq set.
            self.uniq_peers.insert(key);
        }

        // Push to front for immediate checkout (C++ uses push_front).
        self.unused_peers.push_front(peer);

        self.checkout_peer(cuid)
    }

    // ==================================================================
    // Peer checkout / return lifecycle
    // ==================================================================

    /// Check out the next available unused peer for the given caretaker.
    ///
    /// Moves the peer from the unused list to the used set, setting
    /// `used_by` to `cuid`. Returns `None` if no peers are available.
    ///
    /// Matches C++ `DefaultPeerStorage::checkoutPeer`.
    pub fn checkout_peer(&mut self, cuid: u64) -> Option<PeerEntry> {
        if !self.is_peer_available() {
            return None;
        }

        let mut peer = self.unused_peers.pop_front().expect("is_peer_available guarantees non-empty");

        if peer.used_by != 0 {
            warn!(
                "CUID#{} is already set for peer {}:{}",
                peer.used_by, peer.ip, peer.port
            );
        }

        peer.used_by = cuid;
        self.used_peers.insert(peer.clone());
        debug!("Checkout peer {}:{} to CUID#{}", peer.ip, peer.port, cuid);
        Some(peer)
    }

    /// Return a peer from the used set.
    ///
    /// Handles the peer's disconnect lifecycle:
    /// - If the peer was active and disconnected gracefully and is not
    ///   incoming, add it to the dropped list.
    /// - If the peer was not choking and the peer was interested, trigger
    ///   a choke round.
    /// - Remove from `uniq_peers`.
    ///
    /// Matches C++ `DefaultPeerStorage::returnPeer`.
    pub fn return_peer(&mut self, peer: &PeerEntry) {
        debug!(
            "Peer {}:{} returned from CUID#{}",
            peer.ip, peer.port, peer.used_by
        );

        if self.used_peers.remove(peer) {
            self.on_returning_peer(peer);
            self.on_erasing_peer(peer);
        } else {
            warn!(
                "Cannot find peer {}:{} in used_peers",
                peer.ip, peer.port
            );
        }
    }

    /// Check whether any unused peer is available for checkout.
    pub fn is_peer_available(&self) -> bool {
        !self.unused_peers.is_empty()
    }

    // ==================================================================
    // Temporary rejection
    // ==================================================================

    /// Check whether a peer IP is temporarily rejected.
    ///
    /// If the timeout has expired, the entry is removed and `false` is returned.
    /// Matches C++ `DefaultPeerStorage::isTemporarilyRejectedPeer`.
    pub fn is_temporarily_rejected(&mut self, ipaddr: &str) -> bool {
        let Some(timeout) = self.temporarily_rejected_peers.get(ipaddr) else {
            return false;
        };

        if *timeout <= Instant::now() {
            // Timeout has expired — remove entry.
            self.temporarily_rejected_peers.remove(ipaddr);
            return false;
        }

        true
    }

    /// Temporarily reject a peer IP with a variable timeout.
    ///
    /// The timeout is randomly chosen in [120, 720] seconds to avoid
    /// thundering herd effects when many bad peers wake up simultaneously.
    /// Expired entries are cleaned up once per hour.
    ///
    /// Matches C++ `DefaultPeerStorage::rejectPeerTemporarily`.
    pub fn reject_peer_temporarily(&mut self, ipaddr: &str) {
        let now = Instant::now();

        // Periodic cleanup of expired entries (C++ checks every 1 hour).
        if now.duration_since(self.last_temp_peer_cleanup)
            >= Duration::from_secs(TEMP_PEER_CLEANUP_INTERVAL_SECS)
        {
            self.temporarily_rejected_peers
                .retain(|ip, timeout| {
                    if *timeout <= now {
                        debug!("Purge temporarily rejected peer {}", ip);
                        false
                    } else {
                        true
                    }
                });
            self.last_temp_peer_cleanup = now;
        }

        // Variable timeout: [120, 720] seconds (C++: 120 + getRandomNumber(601)).
        let mut rng = rand::thread_rng();
        let extra_secs: u64 = rng.gen_range(0..TEMP_REJECT_TIMEOUT_RANGE_SECS);
        let timeout_secs = TEMP_REJECT_TIMEOUT_MIN_SECS + extra_secs;

        debug!(
            "Temporarily rejected peer {} for {}s",
            ipaddr, timeout_secs
        );

        self.temporarily_rejected_peers
            .insert(ipaddr.to_string(), now + Duration::from_secs(timeout_secs));
    }

    // ==================================================================
    // Peer eviction
    // ==================================================================

    /// Delete peers from the back of the unused list.
    ///
    /// Each removed peer is also removed from `uniq_peers`.
    /// Matches C++ `DefaultPeerStorage::deleteUnusedPeer`.
    pub fn delete_unused_peers(&mut self, del_size: usize) {
        for _ in 0..del_size {
            if let Some(peer) = self.unused_peers.pop_back() {
                self.on_erasing_peer(&peer);
                debug!("Removed peer {}:{}", peer.ip, peer.port);
            }
        }
    }

    // ==================================================================
    // Choking integration
    // ==================================================================

    /// Check whether a choke round interval (10s) has elapsed.
    ///
    /// Delegates to the appropriate choke algorithm (seeder or leecher)
    /// based on whether the download is finished.
    ///
    /// Matches C++ `DefaultPeerStorage::chokeRoundIntervalElapsed`.
    pub fn choke_round_interval_elapsed(&self) -> bool {
        let choke_interval = Duration::from_secs(CHOKE_ROUND_INTERVAL_SECS);

        let last_round = if self.download_finished {
            self.seeder_state_choke.last_round_time()
        } else {
            self.leecher_state_choke.last_round_time()
        };

        match last_round {
            None => true, // No round has been executed yet — interval is elapsed.
            Some(t) => t.elapsed() >= choke_interval,
        }
    }

    /// Execute a choke round on the given peers.
    ///
    /// If the download is finished, delegates to the seeder choke algorithm.
    /// Otherwise, delegates to the leecher choke algorithm.
    ///
    /// Matches C++ `DefaultPeerStorage::executeChoke`.
    pub fn execute_choke(&mut self, peers: &mut [&mut PeerStats]) {
        if self.download_finished {
            self.seeder_state_choke.execute_choke(peers);
        } else {
            self.leecher_state_choke.execute_choke(peers);
        }
    }

    // ==================================================================
    // Accessors
    // ==================================================================

    /// Total count of tracked peers (unused + used).
    ///
    /// Matches C++ `DefaultPeerStorage::countAllPeer`.
    pub fn count_all_peers(&self) -> usize {
        self.unused_peers.len() + self.used_peers.len()
    }

    /// Get a reference to the unused peers list.
    pub fn unused_peers(&self) -> &VecDeque<PeerEntry> {
        &self.unused_peers
    }

    /// Get a reference to the used peers set.
    pub fn used_peers(&self) -> &HashSet<PeerEntry> {
        &self.used_peers
    }

    /// Get a reference to the dropped peers list.
    pub fn dropped_peers(&self) -> &VecDeque<PeerEntry> {
        &self.dropped_peers
    }

    // ==================================================================
    // Configuration setters
    // ==================================================================

    /// Set the maximum peer list size.
    pub fn set_max_peer_list_size(&mut self, size: usize) {
        self.max_peer_list_size = size;
    }

    /// Set whether the download has finished (affects choke algorithm).
    pub fn set_download_finished(&mut self, finished: bool) {
        self.download_finished = finished;
    }

    /// Set whether piece storage is available.
    pub fn set_piece_storage_available(&mut self, available: bool) {
        self.piece_storage_available = available;
    }

    /// Set the peer blocklist for IP-based rejection.
    ///
    /// In C++ aria2, the blocklist is passed to the constructor. Here we
    /// use a setter to allow flexible construction order.
    pub fn set_peer_blocklist(&mut self, blocklist: Arc<BtPeerBlocklist>) {
        self.peer_blocklist = Some(blocklist);
    }

    /// Get the number of peers rejected by the blocklist.
    pub fn blocklist_reject_count(&self) -> u64 {
        self.blocklist_reject_count
    }

    // ==================================================================
    // Peer lookup (C++ DefaultPeerStorage::getPeer)
    // ==================================================================

    /// Find a peer by IP address and port.
    ///
    /// Searches both `used_peers` and `unused_peers`. Returns a clone
    /// of the matching `PeerEntry` if found, `None` otherwise.
    ///
    /// Matches C++ `DefaultPeerStorage::getPeer(ipaddr, port)`.
    pub fn get_peer(&self, ipaddr: &str, port: u16) -> Option<PeerEntry> {
        // Check used_peers first (active connections are more likely targets)
        let key = PeerEntry::new(ipaddr.to_string(), port);
        if let Some(peer) = self.used_peers.get(&key) {
            return Some(peer.clone());
        }
        // Then check unused_peers
        for peer in &self.unused_peers {
            if peer.ip == ipaddr && peer.port == port {
                return Some(peer.clone());
            }
        }
        None
    }

    // ==================================================================
    // Lifecycle callbacks (C++ DefaultPeerStorage::onErasingPeer, onReturningPeer)
    // ==================================================================

    /// Handle peer removal from the used set: remove from uniq_peers.
    ///
    /// In C++ this is a public method called when a peer is removed from
    /// `usedPeers_`. Here it is also public so that external callers
    /// (e.g. BtInteractive) can trigger it directly.
    ///
    /// Matches C++ `DefaultPeerStorage::onErasingPeer`.
    pub fn on_erasing_peer(&mut self, peer: &PeerEntry) {
        self.uniq_peers.remove(&(peer.ip.clone(), peer.port));
    }

    /// Handle peer return: drop tracking and choke triggering.
    ///
    /// In C++ this is a public method. It adds gracefully-disconnected
    /// outgoing peers to the dropped list, and triggers a choke round
    /// if an unchoked+interested peer disconnects.
    ///
    /// Matches C++ `DefaultPeerStorage::onReturningPeer`.
    pub fn on_returning_peer(&mut self, peer: &PeerEntry) {
        if peer.is_active {
            if peer.disconnected_gracefully && !peer.is_incoming {
                self.add_dropped_peer(peer);
            }

            if !peer.am_choking && peer.peer_interested {
                debug!(
                    "Unchoked+interested peer {}:{} disconnected, choke round needed",
                    peer.ip, peer.port
                );
            }
        }
    }

    // ==================================================================
    // Internal helpers
    // ==================================================================

    /// Check whether a peer IP is blocked by the blocklist.
    ///
    /// Returns `false` if no blocklist is configured.
    fn is_blocked_by_blocklist(&self, ipaddr: &str) -> bool {
        match &self.peer_blocklist {
            Some(bl) => bl.contains(ipaddr),
            None => false,
        }
    }

    /// Add a peer to the dropped list, evicting duplicates and capping at
    /// MAX_DROPPED_PEERS.
    ///
    /// Matches C++ `DefaultPeerStorage::addDroppedPeer`.
    fn add_dropped_peer(&mut self, peer: &PeerEntry) {
        // Remove any existing entry with the same (ip, port) to avoid
        // duplicates — the new entry replaces the old one.
        if let Some(pos) = self
            .dropped_peers
            .iter()
            .position(|p| p.ip == peer.ip && p.port == peer.port)
        {
            self.dropped_peers.remove(pos);
        }

        self.dropped_peers.push_front(peer.clone());

        // Cap at MAX_DROPPED_PEERS (C++ hardcodes 50).
        while self.dropped_peers.len() > MAX_DROPPED_PEERS {
            self.dropped_peers.pop_back();
        }
    }

    /// Verify internal invariant: `uniq_peers == keys(unused) ∪ keys(used)`.
    ///
    /// This mirrors the C++ destructor assertion:
    /// `assert(uniqPeers_.size() == unusedPeers_.size() + usedPeers_.size())`.
    #[cfg(test)]
    pub(super) fn verify_invariant(&self) {
        assert_eq!(
            self.uniq_peers.len(),
            self.unused_peers.len() + self.used_peers.len(),
            "Invariant violated: uniq_peers.len() != unused_peers.len() + used_peers.len()"
        );
    }
}

impl Default for DefaultPeerStorage {
    fn default() -> Self {
        Self::new()
    }
}

// ======================================================================
// PeerStorage trait impl for DefaultPeerStorage
// ======================================================================

impl PeerStorage for DefaultPeerStorage {
    fn add_peer(&mut self, peer: PeerEntry) -> bool {
        self.add_peer(peer)
    }

    fn add_peers(&mut self, peers: Vec<PeerEntry>) {
        self.add_peers(peers)
    }

    fn add_and_checkout_peer(&mut self, peer: PeerEntry, cuid: u64) -> Option<PeerEntry> {
        self.add_and_checkout_peer(peer, cuid)
    }

    fn count_all_peers(&self) -> usize {
        self.count_all_peers()
    }

    fn dropped_peers(&self) -> &VecDeque<PeerEntry> {
        self.dropped_peers()
    }

    fn is_peer_available(&self) -> bool {
        self.is_peer_available()
    }

    fn used_peers(&self) -> &HashSet<PeerEntry> {
        self.used_peers()
    }

    fn is_temporarily_rejected(&mut self, ipaddr: &str) -> bool {
        self.is_temporarily_rejected(ipaddr)
    }

    fn reject_peer_temporarily(&mut self, ipaddr: &str) {
        self.reject_peer_temporarily(ipaddr)
    }

    fn checkout_peer(&mut self, cuid: u64) -> Option<PeerEntry> {
        self.checkout_peer(cuid)
    }

    fn return_peer(&mut self, peer: &PeerEntry) {
        self.return_peer(peer)
    }

    fn choke_round_interval_elapsed(&self) -> bool {
        self.choke_round_interval_elapsed()
    }

    fn execute_choke(&mut self, peers: &mut [&mut PeerStats]) {
        self.execute_choke(peers)
    }
}

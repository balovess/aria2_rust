//! DefaultPeerStorage - Peer lifecycle management for BitTorrent.
//!
//! Manages the complete peer lifecycle: unused (discovered) peers -> used
//! (connected) peers -> dropped (recently disconnected) peers.
//!
//! # C++ Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - src/DefaultPeerStorage.h / src/DefaultPeerStorage.cc
//!
//! # Key Data Structures
//!
//! | C++ aria2 | Rust | Rationale |
//! |---|---|---|
//! | set<pair<string, uint16_t>> | HashSet<(String, u16)> | Same dedup by (ip, port) |
//! | deque<shared_ptr<Peer>> | VecDeque<PeerEntry> | Same FIFO ordering |
//! | PeerSet (sorted by ptr) | HashSet<PeerEntry> | Identity by (ip, port) suffices |
//! | map<string, Timer> | HashMap<String, Instant> | Same ip -> timeout mapping |
//! | unique_ptr<BtSeederStateChoke> | BtSeederStateChoke | Inline ownership |
//! | unique_ptr<BtLeecherStateChoke> | BtLeecherStateChoke | Inline ownership |

mod choke_and_config;
mod peer_ops;
mod rejection;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use crate::engine::bt_choke_manager::{BtLeecherStateChoke, BtSeederStateChoke};
use crate::engine::bt_peer_blocklist::BtPeerBlocklist;
use crate::engine::peer_stats::PeerStats;

use super::constants::*;
use super::peer_entry::PeerEntry;
use super::peer_storage_trait::PeerStorage;

/// Peer lifecycle storage, matching C++ DefaultPeerStorage.
///
/// Tracks peers through three stages:
/// 1. **Unused** - discovered but not yet connected (FIFO queue)
/// 2. **Used** - currently connected (set for O(1) lookup)
/// 3. **Dropped** - recently disconnected gracefully (bounded deque)
///
/// Additionally provides:
/// - Deduplication by (ip, port) via uniq_peers
/// - Temporary peer rejection with variable timeout
/// - Choking algorithm integration (seeder vs leecher)
///
/// # Invariant
///
/// uniq_peers always equals the union of keys in unused_peers and
/// used_peers. This is verified in the C++ destructor assertion.
pub struct DefaultPeerStorage {
    /// Maximum number of unused peers before rejection.
    pub(super) max_peer_list_size: usize,

    /// Set of (ip, port) pairs currently tracked (unused + used).
    /// Ensures no duplicate peers across both lists.
    pub(super) uniq_peers: HashSet<(Arc<str>, u16)>,

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

    /// Temporarily rejected peers: ip -> timeout instant.
    pub(super) temporarily_rejected_peers: HashMap<String, Instant>,

    /// Last time we cleaned up expired entries from temporarily_rejected_peers.
    pub(super) last_temp_peer_cleanup: Instant,

    /// Whether piece storage has been configured.
    piece_storage_available: bool,

    /// Whether the download has finished (determines seeder vs leecher choke).
    pub(super) download_finished: bool,

    /// IP range-based blocklist for rejecting peers by address.
    ///
    /// In C++ aria2, this is shared_ptr<BtPeerBlocklist> peerBlocklist_,
    /// always non-null (constructed with make_shared). Here we use
    /// Option<Arc<>> to allow construction without a blocklist and to
    /// support shared ownership with BtRegistry.
    peer_blocklist: Option<Arc<BtPeerBlocklist>>,

    /// Counter of peers rejected by the blocklist (for diagnostics).
    blocklist_reject_count: u64,
}

impl DefaultPeerStorage {
    /// Create a new DefaultPeerStorage with default settings.
    ///
    /// No blocklist is configured; use set_peer_blocklist() to attach one.
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

    fn execute_choke_by_identity(&mut self, peers: &mut [&mut PeerStats]) {
        self.execute_choke_by_identity(peers)
    }

    fn execute_choke(&mut self, peers: &mut [&mut PeerStats]) {
        self.execute_choke(peers)
    }
}

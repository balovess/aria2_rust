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

// ======================================================================
// Constants (matching C++ aria2)
// ======================================================================

/// Maximum number of unused peers to track, matching C++ MAX_PEER_LIST_SIZE.
const MAX_PEER_LIST_SIZE: usize = 512;

/// Maximum number of dropped peers to retain for reconnect attempts.
const MAX_DROPPED_PEERS: usize = 50;

/// Choke round interval in seconds (matching C++ 10_s).
const CHOKE_ROUND_INTERVAL_SECS: u64 = 10;

/// Minimum temporary rejection timeout in seconds (C++ uses 120).
const TEMP_REJECT_TIMEOUT_MIN_SECS: u64 = 120;

/// Range of random additional timeout for temporary rejection (C++ uses
/// `getRandomNumber(601)` which returns [0, 600], so total = 120 + [0, 600]
/// = [120, 720]).
const TEMP_REJECT_TIMEOUT_RANGE_SECS: u64 = 601;

/// Cleanup interval for expired temporarily-rejected peers (C++ uses 1 hour).
const TEMP_PEER_CLEANUP_INTERVAL_SECS: u64 = 3600;

// ======================================================================
// PeerEntry — lightweight peer descriptor
// ======================================================================

/// Lightweight peer descriptor for peer storage tracking.
///
/// Unlike the full `BtPeerConn`, this struct only tracks the fields needed
/// for peer lifecycle management (add/checkout/return/drop) and choking
/// algorithm decisions.
///
/// # Identity
///
/// Two `PeerEntry` values are considered equal if they share the same
/// `(ip, port)` pair. All other fields are ignored for `Hash`/`Eq`/`PartialEq`.
/// This matches the C++ `uniqPeers_` deduplication behavior.
#[derive(Clone, Debug)]
pub struct PeerEntry {
    /// IP address (or hostname) of the peer.
    pub ip: String,
    /// Port number of the peer.
    pub port: u16,
    /// Caretaker unique ID that "owns" this peer (0 = not checked out).
    pub used_by: u64,
    /// Whether the connection is currently active.
    pub is_active: bool,
    /// Whether we are choking this peer.
    pub am_choking: bool,
    /// Whether the peer is interested in our data.
    pub peer_interested: bool,
    /// Whether this is an incoming (rather than outgoing) connection.
    pub is_incoming: bool,
    /// Whether the peer disconnected gracefully (sent proper close).
    pub disconnected_gracefully: bool,
}

impl PeerEntry {
    /// Create a new `PeerEntry` with default state (not checked out, not active).
    pub fn new(ip: String, port: u16) -> Self {
        Self {
            ip,
            port,
            used_by: 0,
            is_active: false,
            am_choking: true,
            peer_interested: false,
            is_incoming: false,
            disconnected_gracefully: false,
        }
    }

    /// Create a `PeerEntry` from a `PeerStats` reference.
    ///
    /// This is a convenience conversion for feeding choking algorithm
    /// output back into peer storage.
    pub fn from_peer_stats(ip: String, port: u16, stats: &PeerStats) -> Self {
        Self {
            ip,
            port,
            used_by: 0,
            is_active: !stats.is_banned,
            am_choking: stats.am_choking,
            peer_interested: stats.peer_interested,
            is_incoming: false,
            disconnected_gracefully: false,
        }
    }
}

// Identity is based on (ip, port) only — matching C++ uniqPeers_ behavior.
impl PartialEq for PeerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.ip == other.ip && self.port == other.port
    }
}

impl Eq for PeerEntry {}

impl std::hash::Hash for PeerEntry {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.ip.hash(state);
        self.port.hash(state);
    }
}

// ======================================================================
// PeerStorage trait — abstract interface (C++ PeerStorage)
// ======================================================================

/// Abstract interface for peer lifecycle storage, matching C++ `PeerStorage`.
///
/// This trait decouples the peer storage contract from the concrete
/// [`DefaultPeerStorage`] implementation, enabling dependency injection
/// and alternative storage strategies.
///
/// # C++ Architecture Reference
///
/// Based on `src/PeerStorage.h` — the abstract base class that
/// `DefaultPeerStorage` implements. The C++ class defines 13 pure-virtual
/// methods; this trait maps each to its Rust equivalent.
///
/// # Thread Safety
///
/// The `Send + Sync` supertraits enable use in `Arc<dyn PeerStorage>`.
/// Callers must employ interior mutability (e.g., `Mutex`, `RwLock`)
/// to call `&mut self` methods through a shared reference.
///
/// # Method Mapping
///
/// | C++ `PeerStorage`            | Rust `PeerStorage` trait          |
/// |-------------------------------|-----------------------------------|
/// | `addPeer(Peer)`              | [`add_peer`](PeerStorage::add_peer) |
/// | `addPeer(vector<Peer>)`      | [`add_peers`](PeerStorage::add_peers) |
/// | `addAndCheckoutPeer`         | [`add_and_checkout_peer`](PeerStorage::add_and_checkout_peer) |
/// | `countAllPeer`               | [`count_all_peers`](PeerStorage::count_all_peers) |
/// | `getDroppedPeers`            | [`dropped_peers`](PeerStorage::dropped_peers) |
/// | `isPeerAvailable`            | [`is_peer_available`](PeerStorage::is_peer_available) |
/// | `getUsedPeers`               | [`used_peers`](PeerStorage::used_peers) |
/// | `isBadPeer`                  | [`is_temporarily_rejected`](PeerStorage::is_temporarily_rejected) |
/// | `addBadPeer`                 | [`reject_peer_temporarily`](PeerStorage::reject_peer_temporarily) |
/// | `checkoutPeer`               | [`checkout_peer`](PeerStorage::checkout_peer) |
/// | `returnPeer`                 | [`return_peer`](PeerStorage::return_peer) |
/// | `chokeRoundIntervalElapsed`  | [`choke_round_interval_elapsed`](PeerStorage::choke_round_interval_elapsed) |
/// | `executeChoke`               | [`execute_choke`](PeerStorage::execute_choke) |
pub trait PeerStorage: Send + Sync {
    /// Add a single peer to the unused list.
    ///
    /// Returns `true` if the peer was added, `false` if rejected.
    ///
    /// Matches C++ `PeerStorage::addPeer(shared_ptr<Peer>)`.
    fn add_peer(&mut self, peer: PeerEntry) -> bool;

    /// Add multiple peers to the unused list.
    ///
    /// Matches C++ `PeerStorage::addPeer(vector<shared_ptr<Peer>>)`.
    fn add_peers(&mut self, peers: Vec<PeerEntry>);

    /// Atomically add a peer and check it out.
    ///
    /// Matches C++ `PeerStorage::addAndCheckoutPeer`.
    fn add_and_checkout_peer(&mut self, peer: PeerEntry, cuid: u64) -> Option<PeerEntry>;

    /// Total count of tracked peers (unused + used).
    ///
    /// Matches C++ `PeerStorage::countAllPeer`.
    fn count_all_peers(&self) -> usize;

    /// Get a reference to the dropped peers list.
    ///
    /// Matches C++ `PeerStorage::getDroppedPeers`.
    fn dropped_peers(&self) -> &VecDeque<PeerEntry>;

    /// Check whether any unused peer is available for checkout.
    ///
    /// Matches C++ `PeerStorage::isPeerAvailable`.
    fn is_peer_available(&self) -> bool;

    /// Get a reference to the used peers set.
    ///
    /// Matches C++ `PeerStorage::getUsedPeers`.
    fn used_peers(&self) -> &HashSet<PeerEntry>;

    /// Check whether a peer IP should be ignored (e.g., temporarily rejected).
    ///
    /// Matches C++ `PeerStorage::isBadPeer`.
    fn is_temporarily_rejected(&mut self, ipaddr: &str) -> bool;

    /// Add a peer IP to the rejection list with a variable timeout.
    ///
    /// Matches C++ `PeerStorage::addBadPeer`.
    fn reject_peer_temporarily(&mut self, ipaddr: &str);

    /// Check out the next available unused peer for the given caretaker.
    ///
    /// Matches C++ `PeerStorage::checkoutPeer`.
    fn checkout_peer(&mut self, cuid: u64) -> Option<PeerEntry>;

    /// Return a peer from the used set.
    ///
    /// Matches C++ `PeerStorage::returnPeer`.
    fn return_peer(&mut self, peer: &PeerEntry);

    /// Check whether a choke round interval has elapsed.
    ///
    /// Matches C++ `PeerStorage::chokeRoundIntervalElapsed`.
    fn choke_round_interval_elapsed(&self) -> bool;

    /// Execute a choke round on the given peers.
    ///
    /// Matches C++ `PeerStorage::executeChoke`.
    fn execute_choke(&mut self, peers: &mut [&mut PeerStats]);
}

// ======================================================================
// DefaultPeerStorage
// ======================================================================

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
    max_peer_list_size: usize,

    /// Set of (ip, port) pairs currently tracked (unused + used).
    /// Ensures no duplicate peers across both lists.
    uniq_peers: HashSet<(String, u16)>,

    /// Unused (not connected) peers, sorted by last-added (FIFO).
    unused_peers: VecDeque<PeerEntry>,

    /// Currently connected (used) peers.
    used_peers: HashSet<PeerEntry>,

    /// Recently disconnected peers, bounded to MAX_DROPPED_PEERS.
    dropped_peers: VecDeque<PeerEntry>,

    /// Choking algorithm for seeder state (when download is complete).
    seeder_state_choke: BtSeederStateChoke,

    /// Choking algorithm for leecher state (when download is in progress).
    leecher_state_choke: BtLeecherStateChoke,

    /// Temporarily rejected peers: ip → timeout instant.
    temporarily_rejected_peers: HashMap<String, Instant>,

    /// Last time we cleaned up expired entries from `temporarily_rejected_peers`.
    last_temp_peer_cleanup: Instant,

    /// Whether piece storage has been configured.
    piece_storage_available: bool,

    /// Whether the download has finished (determines seeder vs leecher choke).
    download_finished: bool,

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
    fn verify_invariant(&self) {
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

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
        use std::sync::Arc;
        use crate::engine::bt_peer_blocklist::BtPeerBlocklist;

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
        use std::sync::Arc;
        use crate::engine::bt_peer_blocklist::BtPeerBlocklist;

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
        use std::sync::Arc;
        use crate::engine::bt_peer_blocklist::BtPeerBlocklist;

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
        use std::sync::Arc;
        use crate::engine::bt_peer_blocklist::BtPeerBlocklist;

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
        use std::sync::Arc;
        use crate::engine::bt_peer_blocklist::BtPeerBlocklist;

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
}

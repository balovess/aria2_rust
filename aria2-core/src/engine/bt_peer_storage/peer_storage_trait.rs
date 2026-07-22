//! PeerStorage trait — abstract interface for peer lifecycle storage.
//!
//! Matches C++ `PeerStorage.h` — the abstract base class that
//! `DefaultPeerStorage` implements.

use std::collections::{HashSet, VecDeque};

use crate::engine::peer_stats::PeerStats;

use super::peer_entry::PeerEntry;

/// Abstract interface for peer lifecycle storage, matching C++ `PeerStorage`.
///
/// This trait decouples the peer storage contract from the concrete
/// [`DefaultPeerStorage`](super::DefaultPeerStorage) implementation, enabling
/// dependency injection and alternative storage strategies.
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

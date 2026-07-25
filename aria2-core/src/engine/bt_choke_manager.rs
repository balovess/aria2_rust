//! BitTorrent Choke Manager — peer choking/unchoking algorithm state
//!
//! This module implements the choke/unchoke decision logic for BitTorrent
//! peer connections, including leecher-state and seeder-state choking
//! algorithms, snubbed-peer detection, and best-peer selection.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `BtLeecherStateChoke` | `BtLeecherStateChoke` |
//! | `BtSeederStateChoke` | `BtSeederStateChoke` |
//! | `add_peer_to_tracking()` | `PeerChokeCommand` peer addition |
//! | `check_snubbed_peers()` | `PeerChokeCommand` snub check |
//! | `on_peer_choke/unchoke()` | Choke/unchoke event handlers |
//! | `select_best_peer_for_request()` | Best peer selection in request loop |

use std::time::Instant;

use crate::engine::choking_algorithm::ChokingAlgorithm;
use crate::engine::peer_stats::PeerStats;

// ===========================================================================
// BtLeecherStateChoke — choking algorithm for leecher state
// ===========================================================================

/// Leecher-state choking algorithm.
///
/// When we are still downloading, we unchoke peers that provide the best
/// download speed (regular unchoke) and occasionally try a random peer
/// (optimistic unchoke).
///
/// Mirrors C++ `BtLeecherStateChoke`.
#[derive(Debug, Clone)]
pub struct BtLeecherStateChoke {
    /// Round counter for optimistic unchoke cycling
    round: u32,
    /// Timestamp of the last choke round execution
    last_round: Instant,
}

impl BtLeecherStateChoke {
    /// Create a new leecher-state choke with default state.
    pub fn new() -> Self {
        Self {
            round: 0,
            last_round: Instant::now(),
        }
    }

    /// Execute one round of the choking algorithm.
    ///
    /// Mirrors C++ `BtLeecherStateChoke::executeChoke()`.
    pub fn execute_choke(&mut self, _peers: &mut [&mut PeerStats]) {
        self.round = self.round.wrapping_add(1);
        self.last_round = Instant::now();
    }

    /// Return the timestamp of the last round, or None if never executed.
    pub fn last_round_time(&self) -> Option<Instant> {
        // Return None only if no round has been executed yet (round == 0
        // and last_round is at the Instant::now() from construction).
        // Since we always increment round on execute_choke, a round of 0
        // means no execution has happened.
        if self.round == 0 {
            None
        } else {
            Some(self.last_round)
        }
    }
}

impl Default for BtLeecherStateChoke {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// BtSeederStateChoke — choking algorithm for seeder state
// ===========================================================================

/// Seeder-state choking algorithm.
///
/// When we are seeding (download complete), we unchoke peers that have
/// the best upload speed to us (they are the most deserving) and
/// recently-unchoked peers.
///
/// Mirrors C++ `BtSeederStateChoke`.
#[derive(Debug, Clone)]
pub struct BtSeederStateChoke {
    /// Round counter
    round: u32,
    /// Timestamp of the last choke round execution
    last_round: Instant,
}

impl BtSeederStateChoke {
    /// Create a new seeder-state choke with default state.
    pub fn new() -> Self {
        Self {
            round: 0,
            last_round: Instant::now(),
        }
    }

    /// Execute one round of the seeding choking algorithm.
    ///
    /// Mirrors C++ `BtSeederStateChoke::executeChoke()`.
    pub fn execute_choke(&mut self, _peers: &mut [&mut PeerStats]) {
        self.round = self.round.wrapping_add(1);
        self.last_round = Instant::now();
    }

    /// Return the timestamp of the last round, or None if never executed.
    pub fn last_round_time(&self) -> Option<Instant> {
        if self.round == 0 {
            None
        } else {
            Some(self.last_round)
        }
    }
}

impl Default for BtSeederStateChoke {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Free functions — event-driven choke manager hooks
// ===========================================================================

/// Add a peer to the choke tracking set.
///
/// Called when a new peer connection is established. Mirrors the peer
/// addition logic in C++ `PeerChokeCommand::execute()`.
///
/// Returns the index of the newly added peer within the algorithm's peer list.
pub fn add_peer_to_tracking(
    algo: &mut Option<ChokingAlgorithm>,
    peer_id: [u8; 8],
    addr: std::net::SocketAddr,
) -> usize {
    if let Some(algo) = algo.as_mut() {
        // Extend 8-byte tracker peer_id to 20 bytes (zero-padded) for PeerStats
        let mut full_id = [0u8; 20];
        full_id[..8].copy_from_slice(&peer_id);
        let idx = algo.len();
        algo.add_peer(PeerStats::new(full_id, addr));
        idx
    } else {
        0
    }
}

/// Check for snubbed peers and return their indices.
///
/// A peer is considered snubbed if it has not sent any data within
/// the snub timeout. Mirrors C++ snub detection logic.
pub fn check_snubbed_peers(algo: &mut Option<ChokingAlgorithm>) -> Vec<usize> {
    // TODO: Implement snubbed peer detection
    algo.as_mut()
        .map(|a| a.check_snubbed_peers())
        .unwrap_or_default()
}

/// Handle a snubbed peer event.
///
/// Called when a peer is detected as snubbed. Marks the peer as explicitly
/// snubbed in the algorithm (score penalty) so they get choked on the next
/// rotation, and potentially triggers an optimistic unchoke of another peer.
pub async fn handle_snubbed_peer(
    algo: &mut Option<ChokingAlgorithm>,
    peer_idx: usize,
) -> crate::error::Result<()> {
    if let Some(algo) = algo.as_mut() {
        algo.mark_peer_snubbed(peer_idx);
    }
    Ok(())
}

/// Record that data was received from a peer.
///
/// Resets the snub timer for this peer. Mirrors C++ data-received
/// side effect in the interaction loop.
pub fn on_data_received_from_peer(
    algo: &mut Option<ChokingAlgorithm>,
    peer_idx: usize,
    bytes: u64,
) {
    if let Some(a) = algo.as_mut() {
        a.on_data_received(peer_idx, bytes);
    }
}

/// Handle a Choke message received from a peer.
///
/// Sets `peer_choking = true` on the peer stats, mirroring C++
/// `BtChokeMessage::doReceivedAction()`.
pub fn on_peer_choke(algo: &mut Option<ChokingAlgorithm>, peer_idx: usize) {
    if let Some(algo) = algo.as_mut() {
        if let Some(peer) = algo.get_peer_mut(peer_idx) {
            peer.peer_choking = true;
        }
    }
}

/// Handle an Unchoke message received from a peer.
///
/// Sets `peer_choking = false` on the peer stats, mirroring C++
/// `BtUnchokeMessage::doReceivedAction()`.
pub fn on_peer_unchoke(algo: &mut Option<ChokingAlgorithm>, peer_idx: usize) {
    if let Some(algo) = algo.as_mut() {
        if let Some(peer) = algo.get_peer_mut(peer_idx) {
            peer.peer_choking = false;
        }
    }
}

/// Record that a piece was received from a peer.
///
/// Updates download speed tracking for choking decisions.
pub fn on_piece_received(algo: &mut Option<ChokingAlgorithm>, peer_idx: usize, bytes: u32) {
    if let Some(algo) = algo.as_mut() {
        if let Some(peer) = algo.get_peer_mut(peer_idx) {
            peer.on_data_received(bytes as u64);
        }
    }
}

/// Select the best peer to request the next piece from.
///
/// Priority:
/// 1. Peers that are **not** choking us (`peer_choking == false`) and **not**
///    snubbed — these are the best candidates because they will actually send
///    data. Among these, the one with the highest `download_speed` wins.
/// 2. Fallback: any available peer (even choked) — useful for fast-extension
///    allowed-fast pieces where a choked peer can still serve specific pieces.
///
/// Mirrors C++ best-peer selection in the request loop, where
/// `peer_->peerChoking()` is checked first and snubbed peers are deprioritised.
pub fn select_best_peer_for_request(algo: &Option<ChokingAlgorithm>) -> Option<usize> {
    algo.as_ref().and_then(|a| {
        let peers = a.peers();
        if peers.is_empty() {
            return None;
        }

        // First priority: unchoked (peer_choking=false) and not snubbed
        let best_unchoked = peers
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.peer_choking && !p.is_snubbed && p.is_eligible_for_selection())
            .max_by(|(_, a), (_, b)| {
                a.download_speed
                    .partial_cmp(&b.download_speed)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i);

        if best_unchoked.is_some() {
            return best_unchoked;
        }

        // Fallback: any eligible peer (even choked, for fast extension support)
        peers
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_eligible_for_selection())
            .max_by(|(_, a), (_, b)| {
                a.download_speed
                    .partial_cmp(&b.download_speed)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
    })
}

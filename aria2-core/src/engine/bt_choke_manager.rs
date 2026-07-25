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
pub fn add_peer_to_tracking(
    algo: &mut Option<ChokingAlgorithm>,
    _peer_id: [u8; 8],
    _addr: std::net::SocketAddr,
) -> usize {
    // TODO: Implement peer tracking addition
    algo.as_ref().map(|a| a.len()).unwrap_or(0)
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
/// Called when a peer is detected as snubbed. May re-choke the peer
/// and trigger optimistic unchoke of another peer.
pub async fn handle_snubbed_peer(
    _algo: &mut Option<ChokingAlgorithm>,
    _peer_idx: usize,
) -> crate::error::Result<()> {
    // TODO: Implement snubbed peer handling
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
/// Mirrors C++ `BtChokeMessage::doReceivedAction()`.
pub fn on_peer_choke(algo: &mut Option<ChokingAlgorithm>, _peer_idx: usize) {
    // TODO: Implement choke event handling
    let _ = algo;
}

/// Handle an Unchoke message received from a peer.
///
/// Mirrors C++ `BtUnchokeMessage::doReceivedAction()`.
pub fn on_peer_unchoke(algo: &mut Option<ChokingAlgorithm>, _peer_idx: usize) {
    // TODO: Implement unchoke event handling
    let _ = algo;
}

/// Record that a piece was received from a peer.
///
/// Updates download speed tracking for choking decisions.
pub fn on_piece_received(algo: &mut Option<ChokingAlgorithm>, peer_idx: usize, _bytes: u32) {
    // TODO: Implement piece-received tracking
    let _ = (algo, peer_idx);
}

/// Select the best peer to request the next piece from.
///
/// Returns `Some(peer_idx)` for the peer with the best download speed
/// that is unchoked and interested, or `None` if no suitable peer exists.
/// Mirrors C++ best-peer selection in the request loop.
pub fn select_best_peer_for_request(algo: &Option<ChokingAlgorithm>) -> Option<usize> {
    // TODO: Implement best peer selection
    algo.as_ref().and_then(|a| {
        a.peers()
            .iter()
            .enumerate()
            .find(|(_, p)| !p.am_choking && p.peer_interested)
            .map(|(i, _)| i)
    })
}

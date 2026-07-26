//! Event-driven choke manager hooks — thin wrappers around ChokingAlgorithm.
//!
//! These free functions provide a convenient API for the download/interaction
//! loop to update the choking algorithm's peer state in response to protocol
//! events (choke/unchoke messages, data received, piece received, etc.).
//!
//! All functions accept `&mut Option<ChokingAlgorithm>` so that the caller
//! does not need to unwrap the option each time.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `add_peer_to_tracking()` | `PeerChokeCommand` peer addition |
//! | `check_snubbed_peers()` | `PeerChokeCommand` snub check |
//! | `on_peer_choke/unchoke()` | Choke/unchoke event handlers |
//! | `select_best_peer_for_request()` | Best peer selection in request loop |

use crate::engine::choking_algorithm::ChokingAlgorithm;
use crate::engine::peer_stats::PeerStats;

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

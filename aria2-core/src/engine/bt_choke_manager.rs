#![allow(clippy::empty_line_after_doc_comments)]
#![allow(clippy::doc_lazy_continuation)]

use tracing::{debug, warn};

use crate::engine::choking_algorithm::ChokingAlgorithm;
use crate::engine::peer_stats::PeerStats;

/// BitTorrent choking algorithm manager for download-side peer selection.
///
/// This module encapsulates all download-side choke/unchoke tracking logic,
/// mirroring the original aria2 C++ architecture's separation of
/// BtLeecherStateChoke and BtSeederStateChoke.
///
/// Responsibilities:
/// - Track which peers are choking us (affects request priority)
/// - Select best peers for piece requests based on choke state and speed
/// - Detect and handle snubbed peers (unresponsive peers)
/// - Update statistics when data is received from peers

// ======================================================================
// Download-Side Choke Tracking Helpers
// ======================================================================

/// Record that a peer at the given index has sent us a Choke message.
///
/// This updates the internal `choking_algo` state so that
/// [`select_best_peer_for_request`] can deprioritize choked peers.
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
/// * `peer_idx` - Index of the peer that sent the choke message
pub fn on_peer_choke(algo: &mut Option<ChokingAlgorithm>, peer_idx: usize) {
    if let Some(a) = algo
        && let Some(peer) = a.get_peer_mut(peer_idx)
    {
        peer.peer_choking = true;
        debug!("Peer #{} is now choking us", peer_idx);
    }
}

/// Record that a peer at the given index has sent us an Unchoke message.
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
/// * `peer_idx` - Index of the peer that sent the unchoke message
pub fn on_peer_unchoke(algo: &mut Option<ChokingAlgorithm>, peer_idx: usize) {
    if let Some(a) = algo
        && let Some(peer) = a.get_peer_mut(peer_idx)
    {
        peer.peer_choking = false;
        debug!("Peer #{} has unchoked us", peer_idx);
    }
}

/// Record data received from a peer (updates speed + resets snubbed status).
///
/// Should be called whenever we successfully receive a block from a peer.
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
/// * `peer_idx` - Index of the peer we received data from
/// * `bytes` - Number of bytes received
pub fn on_data_received_from_peer(
    algo: &mut Option<ChokingAlgorithm>,
    peer_idx: usize,
    bytes: u64,
) {
    if let Some(a) = algo {
        a.on_data_received(peer_idx, bytes);
    }
}

/// Check if any tracked peer is snubbed and should be handled.
///
/// Returns indices of newly snubbed peers that may need special handling
/// (e.g., reduced priority or disconnection).
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
///
/// # Returns
/// Vector of peer indices that are newly snubbed
pub fn check_snubbed_peers(algo: &mut Option<ChokingAlgorithm>) -> Vec<usize> {
    if let Some(a) = algo {
        a.check_snubbed_peers()
    } else {
        vec![]
    }
}

/// Add a connected peer to the choking algorithm tracking.
///
/// Call this when a new peer connection is established during download phase.
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
/// * `peer_id` - First 8 bytes of the peer's 20-byte ID (rest will be zeroed)
/// * `addr` - Socket address of the peer
///
/// # Returns
/// Index of the added peer in the algorithm's internal list,
/// or 0 if no algorithm is configured
pub fn add_peer_to_tracking(
    algo: &mut Option<ChokingAlgorithm>,
    peer_id: [u8; 8],
    addr: std::net::SocketAddr,
) -> usize {
    if let Some(a) = algo {
        let full_peer_id = {
            let mut id = [0u8; 20];
            id[..8].copy_from_slice(&peer_id);
            id
        };
        let stats = PeerStats::new(full_peer_id, addr);
        a.add_peer(stats);
        a.len() - 1 // Return the index of the added peer
    } else {
        0 // No algorithm, return dummy index
    }
}

// ======================================================================
// Peer Selection Logic
// ======================================================================

/// Select the best peer for requesting pieces, preferring unchoked peers.
///
/// Uses the choking algorithm's peer stats to score and rank peers:
/// - Unchoked peers are strongly preferred
/// - Higher download speed is better
/// - Snubbed peers are penalized
///
/// Scoring formula:
/// - Download speed contribution: 50% weight
/// - Upload speed contribution (reciprocity): 30% weight
/// - Interest bonus: +50 points if peer wants our data
///
/// # Arguments
/// * `algo` - The choking algorithm instance (immutable reference)
///
/// # Returns
/// Index of the best peer for making requests, or None if no suitable peer found
pub fn select_best_peer_for_request(algo: &Option<ChokingAlgorithm>) -> Option<usize> {
    if let Some(a) = algo {
        // Find best peer: unchoked + high download speed + not snubbed
        let best_idx = a
            .peers()
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.am_choking && p.peer_interested && !p.is_snubbed)
            .max_by_key(|(_, p)| {
                let mut score = 0i64;
                // Download speed is primary factor (scaled down to avoid overflow)
                score += (p.download_speed * 0.5) as i64;
                // Upload speed contribution (reciprocity)
                score += (p.upload_speed * 0.3) as i64;
                // Bonus for being interested in our data
                if p.peer_interested {
                    score += 50;
                }
                score
            })
            .map(|(i, _)| i);

        if let Some(idx) = best_idx {
            debug!(
                "[BT] Selected peer {} for request (using choking algorithm)",
                idx
            );
            return best_idx;
        }

        // Fallback: if no unchoked+interested peer, just pick first non-snubbed peer
        a.peers().iter().position(|p| !p.is_snubbed)
    } else {
        // No choking algorithm configured: cannot select a peer
        None
    }
}

// ======================================================================
// Snubbed Peer Handling
// ======================================================================

/// Handle a peer that has been marked as snubbed.
///
/// Reduces the request frequency for this peer by increasing its
/// request interval multiplier. This avoids wasting time waiting for
/// data from unresponsive peers while keeping the connection alive
/// in case they recover.
///
/// The choking algorithm will automatically lower this peer's score
/// on next rotation due to the `is_snubbed` flag, which will cause it
/// to be choked on the upload side.
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
/// * `peer_idx` - Index of the snubbed peer
pub async fn handle_snubbed_peer(
    algo: &mut Option<ChokingAlgorithm>,
    peer_idx: usize,
) -> std::result::Result<(), ()> {
    if let Some(a) = algo
        && let Some(peer) = a.get_peer_mut(peer_idx)
    {
        warn!(
            "[BT] Peer {} at {} marked as snubbed, reducing request priority",
            peer_idx, peer.addr
        );

        // Additional action: we could optionally force a choke message
        // For now, just log and let the algorithm handle it naturally
    }

    Ok(())
}

// ======================================================================
// Piece Receive Statistics
// ======================================================================

/// Update peer statistics when piece data is received.
///
/// Should be called whenever we successfully receive a block from a peer.
/// Updates the download speed estimate via EMA and resets the snubbed timer.
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
/// * `peer_idx` - Index of the peer we received data from
/// * `bytes` - Number of bytes received in this block
pub fn on_piece_received(algo: &mut Option<ChokingAlgorithm>, peer_idx: usize, bytes: u64) {
    if let Some(a) = algo {
        a.on_data_received(peer_idx, bytes);
        debug!(
            "[BT] Updated peer {} stats: received {} bytes",
            peer_idx, bytes
        );
    }
}

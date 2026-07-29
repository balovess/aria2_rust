use tracing::{debug, warn};

use crate::engine::peer_stats::BAD_DATA_THRESHOLD;

use super::BtDownloadCommand;

/// Bad peer detection and ban system API (H3).
pub trait BtDownloadCommandPeerBanApi {
    /// Record that a peer sent invalid piece data.
    /// Returns Ok(true) if the peer was banned, Ok(false) if not yet, Err(()) on invalid index.
    #[allow(clippy::result_unit_err)]
    fn record_bad_piece_for_peer(
        &mut self,
        peer_idx: usize,
        piece_index: u32,
    ) -> std::result::Result<bool, ()>;

    /// Record that a valid, verified piece was received from a peer (gradual recovery).
    fn record_valid_piece_for_peer(&mut self, peer_idx: usize);

    /// Check if a peer is currently banned.
    fn is_peer_banned(&self, peer_idx: usize) -> bool;

    /// Get a reference to a peer's stats for RPC/display purposes.
    fn get_peer_stats(&self, peer_idx: usize) -> Option<&crate::engine::peer_stats::PeerStats>;
}

impl BtDownloadCommandPeerBanApi for BtDownloadCommand {
    fn record_bad_piece_for_peer(
        &mut self,
        peer_idx: usize,
        piece_index: u32,
    ) -> std::result::Result<bool, ()> {
        if let Some(ref mut algo) = self.choking_algo
            && let Some(peer) = algo.get_peer_mut(peer_idx)
        {
            let should_ban = peer.increment_bad_data();

            warn!(
                "[BT] Peer {} sent invalid data for piece {} (bad count: {}/{})",
                peer_idx, piece_index, peer.bad_data_count, BAD_DATA_THRESHOLD
            );

            if should_ban {
                let reason = format!(
                    "Too many invalid pieces ({} >= {})",
                    peer.bad_data_count, BAD_DATA_THRESHOLD
                );
                warn!("[BT] BANNING peer {}: {}", peer_idx, reason);
                peer.ban_peer(reason);
                return Ok(true); // Peer was banned
            }

            return Ok(false); // Count incremented but not banned yet
        }

        Err(()) // Invalid peer index or no choking algorithm
    }

    fn record_valid_piece_for_peer(&mut self, peer_idx: usize) {
        if let Some(ref mut algo) = self.choking_algo
            && let Some(peer) = algo.get_peer_mut(peer_idx)
        {
            peer.decrement_bad_data();
            debug!(
                "[BT] Peer {} sent valid piece, bad count decremented to {}",
                peer_idx, peer.bad_data_count
            );
        }
    }

    fn is_peer_banned(&self, peer_idx: usize) -> bool {
        self.choking_algo
            .as_ref()
            .and_then(|algo| algo.get_peer(peer_idx))
            .map(|p| p.is_banned)
            .unwrap_or(true) // If not found, treat as banned for safety
    }

    fn get_peer_stats(&self, peer_idx: usize) -> Option<&crate::engine::peer_stats::PeerStats> {
        self.choking_algo.as_ref()?.get_peer(peer_idx)
    }
}

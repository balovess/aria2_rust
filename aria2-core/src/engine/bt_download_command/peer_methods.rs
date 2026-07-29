use tracing::{debug, warn};

use crate::engine::bt_choke_manager::{
    add_peer_to_tracking, check_snubbed_peers, handle_snubbed_peer, on_data_received_from_peer,
    on_peer_choke, on_peer_unchoke, on_piece_received, select_best_peer_for_request,
};
use crate::error::{Aria2Error, FatalError, Result};

use super::BtDownloadCommand;

// Re-export the bad-data threshold so external code can still reach it via
// `crate::engine::bt_download_command::BAD_DATA_THRESHOLD` if needed.
pub use crate::engine::peer_stats::BAD_DATA_THRESHOLD;

impl BtDownloadCommand {
    // ==================== Choking Algorithm Delegation ====================

    pub fn on_peer_choke(&mut self, peer_idx: usize) {
        on_peer_choke(&mut self.choking_algo, peer_idx);
    }

    pub fn on_peer_unchoke(&mut self, peer_idx: usize) {
        on_peer_unchoke(&mut self.choking_algo, peer_idx);
    }

    pub fn on_data_received_from_peer(&mut self, peer_idx: usize, bytes: u64) {
        on_data_received_from_peer(&mut self.choking_algo, peer_idx, bytes);
    }

    pub fn check_snubbed_peers(&mut self) -> Vec<usize> {
        check_snubbed_peers(&mut self.choking_algo)
    }

    pub fn add_peer_to_tracking(&mut self, peer_id: [u8; 8], addr: std::net::SocketAddr) -> usize {
        add_peer_to_tracking(&mut self.choking_algo, peer_id, addr)
    }

    pub fn select_best_peer_for_request(&self) -> Option<usize> {
        select_best_peer_for_request(&self.choking_algo)
    }

    pub async fn handle_snubbed_peer(&mut self, peer_idx: usize) -> Result<()> {
        handle_snubbed_peer(&mut self.choking_algo, peer_idx)
            .await
            .map_err(|_| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "Failed to handle snubbed peer {}", peer_idx
                )))
            })
    }

    pub fn on_piece_received(&mut self, peer_idx: usize, bytes: u64) {
        on_piece_received(&mut self.choking_algo, peer_idx, bytes as u32);
    }

    /// Explicitly mark a peer as snubbed (algorithm-level snubbing).
    ///
    /// This adds the peer to the explicit snubbed set, causing them to receive
    /// a score of -1000 on the next choke rotation, ensuring they are always choked.
    pub fn mark_peer_snubbed(&mut self, peer_idx: usize) {
        if let Some(algo) = &mut self.choking_algo {
            algo.mark_peer_snubbed(peer_idx);
        }
    }

    /// Check if a peer is explicitly snubbed at the algorithm level.
    pub fn is_explicitly_snubbed(&self, peer_idx: usize) -> bool {
        self.choking_algo
            .as_ref()
            .map(|a| a.is_explicitly_snubbed(peer_idx))
            .unwrap_or(false)
    }

    // ==================== H3: Bad Peer Detection / Ban System API ====================

    /// Record that a specific peer sent invalid piece data (hash verification failed).
    ///
    /// This method:
    /// 1. Increments the peer's `bad_data_count` in the choking algorithm's PeerStats
    /// 2. If the count reaches [`BAD_DATA_THRESHOLD`],
    ///    automatically bans the peer with a reason message
    /// 3. Logs the event at WARN level
    ///
    /// # Arguments
    ///
    /// * `peer_idx` - The index of the peer in the choking algorithm's peer list
    /// * `piece_index` - The index of the piece that failed verification
    ///
    /// # Returns
    ///
    /// * `Ok(true)` if the peer was banned as a result of this call
    /// * `Ok(false)` if the peer was not banned (count below threshold)
    /// * `Err(())` if the peer index is invalid or choking algorithm is not configured
    #[allow(clippy::result_unit_err)]
    pub fn record_bad_piece_for_peer(
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
                return Ok(true);
            }

            return Ok(false);
        }

        Err(())
    }

    /// Record that a valid, verified piece was received from a peer.
    ///
    /// This triggers gradual recovery by decrementing the peer's `bad_data_count`.
    /// Call this after successful hash verification to allow peers to recover reputation.
    ///
    /// # Arguments
    ///
    /// * `peer_idx` - The index of the peer in the choking algorithm's peer list
    pub fn record_valid_piece_for_peer(&mut self, peer_idx: usize) {
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

    /// Check if a peer is currently banned.
    ///
    /// # Arguments
    ///
    /// * `peer_idx` - The index of the peer in the choking algorithm's peer list
    ///
    /// # Returns
    ///
    /// * `true` if the peer is banned or peer not found
    /// * `false` if the peer exists and is not banned
    pub fn is_peer_banned(&self, peer_idx: usize) -> bool {
        self.choking_algo
            .as_ref()
            .and_then(|algo| algo.get_peer(peer_idx))
            .map(|p| p.is_banned)
            .unwrap_or(true) // If not found, treat as banned for safety
    }

    /// Get a reference to a peer's stats for RPC/display purposes.
    ///
    /// Returns `None` if the peer doesn't exist or no choking algorithm is configured.
    pub fn get_peer_stats(&self, peer_idx: usize) -> Option<&crate::engine::peer_stats::PeerStats> {
        self.choking_algo.as_ref()?.get_peer(peer_idx)
    }
}
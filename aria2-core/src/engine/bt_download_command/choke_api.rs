use crate::engine::bt_choke_manager::{
    add_peer_to_tracking, check_snubbed_peers, handle_snubbed_peer, on_data_received_from_peer,
    on_peer_choke, on_peer_unchoke, on_piece_received, select_best_peer_for_request,
};
use crate::error::{Aria2Error, FatalError, Result};

use super::BtDownloadCommand;

/// Choke / snub / peer-tracking delegation API.
pub trait BtDownloadCommandChokeApi {
    fn on_peer_choke(&mut self, peer_idx: usize);
    fn on_peer_unchoke(&mut self, peer_idx: usize);
    fn on_data_received_from_peer(&mut self, peer_idx: usize, bytes: u64);
    fn check_snubbed_peers(&mut self) -> Vec<usize>;
    fn add_peer_to_tracking(&mut self, peer_id: [u8; 8], addr: std::net::SocketAddr) -> usize;
    fn select_best_peer_for_request(&self) -> Option<usize>;
    async fn handle_snubbed_peer(&mut self, peer_idx: usize) -> Result<()>;
    fn on_piece_received(&mut self, peer_idx: usize, bytes: u64);

    /// Explicitly mark a peer as snubbed (algorithm-level snubbing).
    fn mark_peer_snubbed(&mut self, peer_idx: usize);

    /// Check if a peer is explicitly snubbed at the algorithm level.
    fn is_explicitly_snubbed(&self, peer_idx: usize) -> bool;
}

impl BtDownloadCommandChokeApi for BtDownloadCommand {
    fn on_peer_choke(&mut self, peer_idx: usize) {
        on_peer_choke(&mut self.choking_algo, peer_idx);
    }

    fn on_peer_unchoke(&mut self, peer_idx: usize) {
        on_peer_unchoke(&mut self.choking_algo, peer_idx);
    }

    fn on_data_received_from_peer(&mut self, peer_idx: usize, bytes: u64) {
        on_data_received_from_peer(&mut self.choking_algo, peer_idx, bytes);
    }

    fn check_snubbed_peers(&mut self) -> Vec<usize> {
        check_snubbed_peers(&mut self.choking_algo)
    }

    fn add_peer_to_tracking(&mut self, peer_id: [u8; 8], addr: std::net::SocketAddr) -> usize {
        add_peer_to_tracking(&mut self.choking_algo, peer_id, addr)
    }

    fn select_best_peer_for_request(&self) -> Option<usize> {
        select_best_peer_for_request(&self.choking_algo)
    }

    async fn handle_snubbed_peer(&mut self, peer_idx: usize) -> Result<()> {
        handle_snubbed_peer(&mut self.choking_algo, peer_idx)
            .await
            .map_err(|_| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "Failed to handle snubbed peer {}",
                    peer_idx
                )))
            })
    }

    fn on_piece_received(&mut self, peer_idx: usize, bytes: u64) {
        on_piece_received(&mut self.choking_algo, peer_idx, bytes as u32);
    }

    fn mark_peer_snubbed(&mut self, peer_idx: usize) {
        if let Some(algo) = &mut self.choking_algo {
            algo.mark_peer_snubbed(peer_idx);
        }
    }

    fn is_explicitly_snubbed(&self, peer_idx: usize) -> bool {
        self.choking_algo
            .as_ref()
            .map(|a| a.is_explicitly_snubbed(peer_idx))
            .unwrap_or(false)
    }
}

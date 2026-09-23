use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::choking_algorithm::{ChokingAlgorithm, IdentityChokeAction, PeerIdentity};
use crate::engine::peer_stats::PeerStats;

use super::BtDownloadCommand;

impl BtDownloadCommand {
    pub(crate) async fn apply_upload_choke_round(&mut self, active_connections: &mut [BtPeerConn]) {
        let Some(algo) = self.choking_algo.as_mut() else {
            return;
        };

        for connection in active_connections.iter() {
            algo.sync_peer_by_identity(&connection.stats);
        }
        let actions = algo.rotate_choke_by_identity();
        let next_optimistic = algo.optimistically_unchoke_by_identity();
        for action in &actions {
            let identity = action.identity();
            let Some(connection) = active_connections
                .iter_mut()
                .find(|connection| PeerIdentity::from(&connection.stats) == identity)
            else {
                continue;
            };
            let result = match action {
                IdentityChokeAction::Choke(_) => connection.choke_upload_peer().await,
                IdentityChokeAction::Unchoke(_) => connection.unchoke_upload_peer().await,
                IdentityChokeAction::NoChange(_) => Ok(()),
            };
            if let Err(error) = result {
                tracing::debug!(%error, peer = %identity.addr, "Failed to apply BT upload choke decision");
            }
        }

        if let Some(next) = next_optimistic
            && let Some(connection) = active_connections
                .iter_mut()
                .find(|connection| PeerIdentity::from(&connection.stats) == next)
            && let Err(error) = connection.unchoke_upload_peer().await
        {
            tracing::debug!(%error, peer = %next.addr, "Failed to apply optimistic unchoke");
        }
    }

    pub fn on_peer_choke(&mut self, peer_idx: usize) {
        if let Some(algo) = self.choking_algo.as_mut()
            && let Some(peer) = algo.get_peer_mut(peer_idx)
        {
            peer.peer_choking = true;
        }
    }

    pub fn on_peer_unchoke(&mut self, peer_idx: usize) {
        if let Some(algo) = self.choking_algo.as_mut()
            && let Some(peer) = algo.get_peer_mut(peer_idx)
        {
            peer.peer_choking = false;
        }
    }

    pub fn on_data_received_from_peer(&mut self, peer_idx: usize, bytes: u64) {
        if let Some(algo) = self.choking_algo.as_mut() {
            algo.on_data_received(peer_idx, bytes);
        }
    }

    pub fn check_snubbed_peers(&mut self) -> Vec<usize> {
        self.choking_algo
            .as_mut()
            .map(ChokingAlgorithm::check_snubbed_peers)
            .unwrap_or_default()
    }

    pub fn add_peer_to_tracking(&mut self, peer_id: [u8; 8], addr: std::net::SocketAddr) -> usize {
        let Some(algo) = self.choking_algo.as_mut() else {
            return 0;
        };

        let mut full_id = [0u8; 20];
        full_id[..8].copy_from_slice(&peer_id);
        let index = algo.len();
        algo.add_peer(PeerStats::new(full_id, addr));
        index
    }

    pub fn select_best_peer_for_request(&self) -> Option<usize> {
        let algo = self.choking_algo.as_ref()?;
        let peers = algo.peers();
        if peers.is_empty() {
            return None;
        }

        let best_unchoked = peers
            .iter()
            .enumerate()
            .filter(|(_, peer)| {
                !peer.peer_choking && !peer.is_snubbed && peer.is_eligible_for_selection()
            })
            .max_by(|(_, a), (_, b)| {
                a.download_speed
                    .partial_cmp(&b.download_speed)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(index, _)| index);

        best_unchoked.or_else(|| {
            peers
                .iter()
                .enumerate()
                .filter(|(_, peer)| peer.is_eligible_for_selection())
                .max_by(|(_, a), (_, b)| {
                    a.download_speed
                        .partial_cmp(&b.download_speed)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(index, _)| index)
        })
    }

    pub fn rotate_choke_by_identity(&mut self) -> Vec<IdentityChokeAction> {
        self.choking_algo
            .as_mut()
            .map(ChokingAlgorithm::rotate_choke_by_identity)
            .unwrap_or_default()
    }

    pub fn optimistically_unchoke_by_identity(&mut self) -> Option<PeerIdentity> {
        self.choking_algo
            .as_mut()
            .and_then(ChokingAlgorithm::optimistically_unchoke_by_identity)
    }

    pub fn select_best_peer_for_request_by_identity(&self) -> Option<PeerIdentity> {
        self.select_best_peer_for_request().and_then(|index| {
            self.choking_algo
                .as_ref()?
                .get_peer(index)
                .map(PeerIdentity::from)
        })
    }

    pub fn on_piece_received(&mut self, peer_idx: usize, bytes: u64) {
        if let Some(algo) = self.choking_algo.as_mut() {
            algo.on_data_received(peer_idx, bytes);
        }
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
}

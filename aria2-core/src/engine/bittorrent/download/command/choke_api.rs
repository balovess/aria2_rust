use std::collections::HashMap;

use crate::engine::bittorrent::peer::choking_algorithm::{
    ChokingAlgorithm, IdentityChokeAction, PeerIdentity,
};
use crate::engine::bittorrent::peer::message_handler::PeerSwarm;
use crate::engine::bittorrent::peer::stats::PeerStats;

use super::BtDownloadCommand;

impl BtDownloadCommand {
    pub(crate) fn apply_upload_choke_round_swarm(&mut self, swarm: &mut PeerSwarm) {
        let Some(algo) = self.choking_algo.as_mut() else {
            return;
        };

        let actor_ids = swarm
            .iter()
            .filter(|actor| !actor.dead)
            .map(|actor| (PeerIdentity::from(&actor.stats), actor.actor_id))
            .collect::<HashMap<_, _>>();
        algo.sync_peers_by_identity(
            swarm
                .iter()
                .filter(|actor| !actor.dead)
                .map(|actor| &actor.stats),
        );
        let mut actions = algo.rotate_choke_by_identity();
        if let Some(identity) = algo.optimistically_unchoke_by_identity() {
            actions.push(IdentityChokeAction::Unchoke(identity));
        }

        let mut disconnected = Vec::new();
        for action in actions {
            let identity = action.identity();
            let Some(actor_id) = actor_ids.get(&identity).copied() else {
                continue;
            };
            let choked = match action {
                IdentityChokeAction::Choke(_) => true,
                IdentityChokeAction::Unchoke(_) => false,
                IdentityChokeAction::NoChange(_) => continue,
            };
            if !swarm.set_upload_choked(actor_id, choked) {
                disconnected.push(actor_id);
            }
        }
        for actor_id in disconnected {
            swarm.mark_dead(actor_id);
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

    pub(crate) fn track_peer_for_upload_choking(&mut self, stats: &PeerStats) {
        let Some(algo) = self.choking_algo.as_mut() else {
            return;
        };
        let identity = PeerIdentity::from(stats);
        if algo
            .peers()
            .iter()
            .any(|peer| PeerIdentity::from(peer) == identity)
        {
            algo.sync_peer_by_identity(stats);
        } else {
            algo.add_peer(stats.clone());
        }
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

//! Peer metadata captured for one piece attempt and updated from actor events.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::OnceLock;

use super::peer_registry::PeerSwarm;
use crate::engine::bt_peer_connection::PeerActorId;
use crate::engine::choking_algorithm::PeerIdentity;

#[derive(Clone, Copy)]
pub(super) struct PeerSchedulingEntry {
    actor_id: PeerActorId,
    pub(super) address: Option<SocketAddr>,
    pub(super) has_piece: bool,
    identity: PeerIdentity,
}

pub(super) struct PeerSchedulingSnapshot {
    piece_index: u32,
    peers: Vec<PeerSchedulingEntry>,
    indices_by_actor_id: HashMap<PeerActorId, usize>,
    indices_by_identity: HashMap<PeerIdentity, usize>,
    indices_by_address: OnceLock<HashMap<SocketAddr, usize>>,
}

impl PeerSchedulingSnapshot {
    pub(super) fn capture(swarm: &PeerSwarm, piece_index: u32) -> Self {
        let mut peers = Vec::with_capacity(swarm.len());
        let mut indices_by_actor_id = HashMap::with_capacity(swarm.len());
        let mut indices_by_identity = HashMap::with_capacity(swarm.len());
        for actor in swarm.iter().filter(|actor| !actor.dead) {
            let index = peers.len();
            let byte = actor
                .bitfield
                .get(piece_index as usize / 8)
                .copied()
                .unwrap_or(0);
            let has_piece =
                actor.seeder || (actor.has_bitfield && byte & (0x80 >> (piece_index % 8)) != 0);
            let identity = PeerIdentity::from(&actor.stats);
            peers.push(PeerSchedulingEntry {
                actor_id: actor.actor_id,
                address: Some(actor.endpoint),
                has_piece,
                identity,
            });
            indices_by_actor_id.insert(actor.actor_id, index);
            indices_by_identity.insert(identity, index);
        }

        Self {
            piece_index,
            peers,
            indices_by_actor_id,
            indices_by_identity,
            indices_by_address: OnceLock::new(),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.peers.len()
    }

    pub(super) fn peers(&self) -> &[PeerSchedulingEntry] {
        &self.peers
    }

    pub(super) fn peer(&self, index: usize) -> Option<&PeerSchedulingEntry> {
        self.peers.get(index)
    }

    pub(super) fn actor_id(&self, index: usize) -> Option<PeerActorId> {
        self.peers.get(index).map(|peer| peer.actor_id)
    }

    pub(super) fn peer_identity(&self, index: usize) -> Option<PeerIdentity> {
        self.peers.get(index).map(|peer| peer.identity)
    }

    pub(super) fn peer_index(&self, identity: PeerIdentity) -> Option<usize> {
        self.indices_by_identity.get(&identity).copied()
    }

    pub(super) fn peer_index_by_actor_id(&self, actor_id: PeerActorId) -> Option<usize> {
        self.indices_by_actor_id.get(&actor_id).copied()
    }

    pub(super) fn update_peer_availability(
        &mut self,
        actor_id: PeerActorId,
        piece_index: u32,
        has_piece: bool,
    ) {
        if piece_index == self.piece_index
            && let Some(index) = self.peer_index_by_actor_id(actor_id)
            && let Some(peer) = self.peers.get_mut(index)
        {
            peer.has_piece = has_piece;
        }
    }

    pub(super) fn update_peer_bitfield(&mut self, actor_id: PeerActorId, bitfield: &[u8]) {
        let piece_index = self.piece_index as usize;
        let has_piece = bitfield
            .get(piece_index / 8)
            .is_some_and(|byte| byte & (0x80 >> (piece_index % 8)) != 0);
        self.update_peer_availability(actor_id, self.piece_index, has_piece);
    }

    pub(super) fn peer_index_at(&self, address: SocketAddr) -> Option<usize> {
        self.indices_by_address
            .get_or_init(|| {
                self.peers
                    .iter()
                    .enumerate()
                    .filter_map(|(index, peer)| peer.address.map(|address| (address, index)))
                    .collect()
            })
            .get(&address)
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_bitfield_event_updates_only_target_actor_and_piece() {
        let actor_id = PeerActorId(1);
        let other_id = PeerActorId(2);
        let address = "127.0.0.1:6881".parse().unwrap();
        let other_address = "127.0.0.1:6882".parse().unwrap();
        let identity = PeerIdentity {
            peer_id: [1; 20],
            addr: address,
        };
        let other_identity = PeerIdentity {
            peer_id: [2; 20],
            addr: other_address,
        };
        let mut snapshot = PeerSchedulingSnapshot {
            piece_index: 9,
            peers: vec![
                PeerSchedulingEntry {
                    actor_id,
                    address: Some(address),
                    has_piece: false,
                    identity,
                },
                PeerSchedulingEntry {
                    actor_id: other_id,
                    address: Some(other_address),
                    has_piece: false,
                    identity: other_identity,
                },
            ],
            indices_by_actor_id: HashMap::from([(actor_id, 0), (other_id, 1)]),
            indices_by_identity: HashMap::from([(identity, 0), (other_identity, 1)]),
            indices_by_address: OnceLock::new(),
        };

        snapshot.update_peer_bitfield(actor_id, &[0, 0b0100_0000]);
        assert!(snapshot.peer(0).unwrap().has_piece);
        assert!(!snapshot.peer(1).unwrap().has_piece);
        snapshot.update_peer_bitfield(actor_id, &[]);
        assert!(!snapshot.peer(0).unwrap().has_piece);
    }
}

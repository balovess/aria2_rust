//! Peer metadata captured for a request batch and updated from actor events.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::OnceLock;

use super::super::types::DEFAULT_MAX_OUTSTANDING_REQUEST;
use super::peer_registry::PeerSwarm;
use crate::engine::bt_peer_connection::PeerActorId;
use crate::engine::choking_algorithm::PeerIdentity;

#[derive(Clone)]
pub(super) struct PeerSchedulingEntry {
    actor_id: PeerActorId,
    pub(super) address: Option<SocketAddr>,
    bitfield: Vec<u8>,
    seeder: bool,
    peer_choking: bool,
    peer_allowed_fast: HashSet<u32>,
    max_outstanding_requests: usize,
    identity: PeerIdentity,
}

pub(super) struct PeerSchedulingSnapshot {
    peers: Vec<PeerSchedulingEntry>,
    indices_by_actor_id: HashMap<PeerActorId, usize>,
    indices_by_identity: HashMap<PeerIdentity, usize>,
    indices_by_address: OnceLock<HashMap<SocketAddr, usize>>,
}

impl PeerSchedulingSnapshot {
    pub(super) fn capture(swarm: &PeerSwarm) -> Self {
        let mut peers = Vec::with_capacity(swarm.len());
        let mut indices_by_actor_id = HashMap::with_capacity(swarm.len());
        let mut indices_by_identity = HashMap::with_capacity(swarm.len());
        for actor in swarm.iter().filter(|actor| !actor.dead) {
            let index = peers.len();
            let identity = PeerIdentity::from(&actor.stats);
            peers.push(PeerSchedulingEntry {
                actor_id: actor.actor_id,
                address: Some(actor.endpoint),
                bitfield: actor.bitfield.clone(),
                seeder: actor.seeder,
                peer_choking: actor.stats.peer_choking,
                peer_allowed_fast: actor.peer_allowed_fast.clone(),
                max_outstanding_requests: actor.max_outstanding_requests,
                identity,
            });
            indices_by_actor_id.insert(actor.actor_id, index);
            indices_by_identity.insert(identity, index);
        }

        Self {
            peers,
            indices_by_actor_id,
            indices_by_identity,
            indices_by_address: OnceLock::new(),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.peers.len()
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

    pub(super) fn has_piece(&self, index: usize, piece_index: u32) -> bool {
        self.peers.get(index).is_some_and(|peer| {
            peer.seeder
                || peer
                    .bitfield
                    .get(piece_index as usize / 8)
                    .is_some_and(|byte| byte & (0x80 >> (piece_index % 8)) != 0)
        })
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
        if let Some(index) = self.peer_index_by_actor_id(actor_id)
            && let Some(peer) = self.peers.get_mut(index)
        {
            let byte_index = piece_index as usize / 8;
            if peer.bitfield.len() <= byte_index {
                peer.bitfield.resize(byte_index + 1, 0);
            }
            let mask = 0x80 >> (piece_index % 8);
            if has_piece {
                peer.bitfield[byte_index] |= mask;
            } else {
                peer.bitfield[byte_index] &= !mask;
            }
        }
    }

    pub(super) fn peer_can_request(&self, index: usize, piece_index: u32) -> bool {
        self.peers
            .get(index)
            .is_some_and(|peer| !peer.peer_choking || peer.peer_allowed_fast.contains(&piece_index))
    }

    pub(super) fn request_window(&self, index: usize) -> usize {
        self.peers
            .get(index)
            .map_or(DEFAULT_MAX_OUTSTANDING_REQUEST, |peer| {
                peer.max_outstanding_requests
            })
    }

    pub(super) fn update_request_window(&mut self, actor_id: PeerActorId, limit: usize) {
        if let Some(index) = self.peer_index_by_actor_id(actor_id)
            && let Some(peer) = self.peers.get_mut(index)
        {
            peer.max_outstanding_requests = limit;
        }
    }

    pub(super) fn update_peer_choking(&mut self, actor_id: PeerActorId, peer_choking: bool) {
        if let Some(index) = self.peer_index_by_actor_id(actor_id)
            && let Some(peer) = self.peers.get_mut(index)
        {
            peer.peer_choking = peer_choking;
        }
    }

    pub(super) fn add_peer_allowed_fast(&mut self, actor_id: PeerActorId, piece_index: u32) {
        if let Some(index) = self.peer_index_by_actor_id(actor_id)
            && let Some(peer) = self.peers.get_mut(index)
        {
            peer.peer_allowed_fast.insert(piece_index);
        }
    }

    pub(super) fn update_peer_bitfield(&mut self, actor_id: PeerActorId, bitfield: &[u8]) {
        if let Some(index) = self.peer_index_by_actor_id(actor_id)
            && let Some(peer) = self.peers.get_mut(index)
        {
            peer.bitfield.clear();
            peer.bitfield.extend_from_slice(bitfield);
            peer.seeder = false;
        }
    }

    pub(super) fn update_peer_seeder(&mut self, actor_id: PeerActorId, seeder: bool) {
        if let Some(index) = self.peer_index_by_actor_id(actor_id)
            && let Some(peer) = self.peers.get_mut(index)
        {
            peer.seeder = seeder;
        }
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
            peers: vec![
                PeerSchedulingEntry {
                    actor_id,
                    address: Some(address),
                    bitfield: vec![0, 0],
                    seeder: false,
                    peer_choking: false,
                    peer_allowed_fast: HashSet::new(),
                    max_outstanding_requests: DEFAULT_MAX_OUTSTANDING_REQUEST,
                    identity,
                },
                PeerSchedulingEntry {
                    actor_id: other_id,
                    address: Some(other_address),
                    bitfield: vec![0, 0],
                    seeder: false,
                    peer_choking: false,
                    peer_allowed_fast: HashSet::new(),
                    max_outstanding_requests: DEFAULT_MAX_OUTSTANDING_REQUEST,
                    identity: other_identity,
                },
            ],
            indices_by_actor_id: HashMap::from([(actor_id, 0), (other_id, 1)]),
            indices_by_identity: HashMap::from([(identity, 0), (other_identity, 1)]),
            indices_by_address: OnceLock::new(),
        };

        snapshot.update_peer_bitfield(actor_id, &[0, 0b0100_0000]);
        assert!(snapshot.has_piece(0, 9));
        assert!(!snapshot.has_piece(1, 9));
        snapshot.update_peer_bitfield(actor_id, &[]);
        assert!(!snapshot.has_piece(0, 9));
    }
}

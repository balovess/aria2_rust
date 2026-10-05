//! Piece-scoped request generations borrowed from the long-lived peer actor.
use super::*;

/// Piece-scoped scheduling handles for peer actors owned by the torrent swarm.
pub(crate) struct PeerGeneration {
    pub(super) senders: HashMap<PeerActorId, PeerActorControl>,
    pub(super) generation: RequestGeneration,
    pub(super) active_pieces: HashSet<u32>,
    pub(super) availability_changed_actor_ids: HashSet<PeerActorId>,
    pub(super) pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
}

pub(crate) enum TryRequestError {
    Full(watch::Receiver<u64>),
    Closed,
}

impl PeerGeneration {
    #[cfg(test)]
    pub(crate) fn for_test(
        senders: Vec<(PeerActorId, PeerActorControl)>,
        piece_index: u32,
    ) -> Self {
        Self {
            senders: senders.into_iter().collect(),
            generation: RequestGeneration::allocate(),
            active_pieces: HashSet::from([piece_index]),
            availability_changed_actor_ids: HashSet::new(),
            pex_peers: Vec::new(),
        }
    }

    /// Begin a piece request generation on actors owned by the torrent swarm.
    /// The returned scheduler borrows only command handles; ending it never
    /// shuts down the peer connections.
    pub(crate) fn from_swarm(swarm: &PeerSwarm, piece_indices: &[u32]) -> Self {
        let generation = RequestGeneration::allocate();
        let mut senders = HashMap::with_capacity(swarm.len());
        for actor in swarm.iter().filter(|actor| !actor.dead) {
            let control = actor.handle();
            let mut began_all_pieces = true;
            for &piece_index in piece_indices {
                if control.begin_generation(generation, piece_index).is_err() {
                    began_all_pieces = false;
                    break;
                }
            }
            if began_all_pieces {
                senders.insert(actor.actor_id, control);
            }
        }

        Self {
            senders,
            generation,
            active_pieces: piece_indices.iter().copied().collect(),
            availability_changed_actor_ids: HashSet::new(),
            pex_peers: Vec::new(),
        }
    }

    pub(crate) fn record_availability_change(&mut self, actor_id: PeerActorId) {
        self.availability_changed_actor_ids.insert(actor_id);
    }

    pub(crate) fn take_availability_changes(&mut self) -> HashSet<PeerActorId> {
        std::mem::take(&mut self.availability_changed_actor_ids)
    }

    pub(crate) fn record_pex_peers(
        &mut self,
        peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    ) {
        self.pex_peers.extend(peers);
    }

    pub(crate) fn take_pex_peers(
        &mut self,
    ) -> Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> {
        std::mem::take(&mut self.pex_peers)
    }

    pub(crate) fn generation(&self) -> RequestGeneration {
        self.generation
    }

    pub(crate) fn has_peer(&self, actor_id: PeerActorId) -> bool {
        self.senders.contains_key(&actor_id)
    }

    pub(crate) fn try_request(
        &self,
        actor_id: PeerActorId,
        piece_index: u32,
        request: BlockRequest,
    ) -> std::result::Result<(), TryRequestError> {
        let Some(sender) = self.senders.get(&actor_id) else {
            return Err(TryRequestError::Closed);
        };
        let capacity_updates = sender.queue_capacity_updates();
        match sender.try_request(self.generation, piece_index, request) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(TryRequestError::Full(capacity_updates)),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TryRequestError::Closed),
        }
    }

    /// Reuse the same peer I/O tasks for a retry while advancing the request
    /// epoch. This drains old in-flight blocks before accepting new requests.
    pub(crate) fn advance_generations(&mut self) {
        let piece_indices = self.active_pieces.iter().copied().collect::<Vec<_>>();
        for sender in self.senders.values() {
            for &piece_index in &piece_indices {
                let _ = sender.end_generation(self.generation, piece_index);
            }
        }
        self.generation = RequestGeneration::allocate();
        let actor_ids = self.senders.keys().copied().collect::<Vec<_>>();
        let mut failed_peers = Vec::new();
        for actor_id in actor_ids {
            let Some(control) = self.senders.get(&actor_id) else {
                continue;
            };
            let mut began_all_pieces = true;
            for &piece_index in &piece_indices {
                if control
                    .begin_generation(self.generation, piece_index)
                    .is_err()
                {
                    began_all_pieces = false;
                    break;
                }
            }
            if !began_all_pieces {
                failed_peers.push(actor_id);
            }
        }
        for actor_id in failed_peers {
            self.senders.remove(&actor_id);
        }
    }

    pub(crate) async fn finish_piece_generation(&mut self, piece_index: u32) {
        if !self.active_pieces.contains(&piece_index) {
            return;
        }
        for sender in self.senders.values() {
            let _ = sender.end_generation(self.generation, piece_index);
        }
        self.active_pieces.remove(&piece_index);
    }

    /// Cancel this attempt's requests after they have been requeued. The peer
    /// I/O task stays alive so a later piece retry can reuse the connection.
    pub(crate) fn cancel_peer_requests(
        &self,
        actor_id: PeerActorId,
        requests: &[BlockRequest],
        piece_index: u32,
    ) {
        let Some(sender) = self.senders.get(&actor_id) else {
            return;
        };

        for request in requests {
            let _ = sender.try_cancel(self.generation, piece_index, *request);
        }
    }

    pub(crate) fn apply_choke_action(&self, actor_id: PeerActorId, choke: bool) -> bool {
        let Some(sender) = self.senders.get(&actor_id) else {
            return false;
        };
        sender.set_upload_choked(choke)
    }

    /// End this piece generation without stopping torrent-owned peer actors.
    pub(crate) async fn finish_generation(&mut self, event_rx: &mut PeerSwarmEventLease<'_>) {
        for sender in self.senders.values() {
            for &piece_index in &self.active_pieces {
                let _ = sender.end_generation(self.generation, piece_index);
            }
        }
        self.active_pieces.clear();
        self.senders.clear();
        while let Ok(event) = event_rx.try_recv() {
            match event {
                PeerEvent::PeerAvailabilityChanged { actor_id, .. }
                | PeerEvent::PeerAvailabilitySnapshot { actor_id, .. } => {
                    self.record_availability_change(actor_id);
                }
                PeerEvent::PexPeers { peers, .. } => self.record_pex_peers(peers),
                PeerEvent::TrackerPeers { .. } => {}
                _ => {}
            }
        }
    }
}

impl Drop for PeerGeneration {
    fn drop(&mut self) {
        for sender in self.senders.values() {
            for &piece_index in &self.active_pieces {
                let _ = sender.end_generation(self.generation, piece_index);
            }
        }
    }
}

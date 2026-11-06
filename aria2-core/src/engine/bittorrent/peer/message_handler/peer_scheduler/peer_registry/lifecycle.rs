use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::engine::bittorrent::peer::connection::{BtPeerConn, PeerActorId};
use crate::engine::bittorrent::peer::stats::SwarmUploadRate;
use crate::engine::bittorrent::peer::upload_session::PieceDataProvider;

use super::super::{PeerActorPayloadConfig, PeerCommand, PeerEvent};
use super::{PeerActorEntry, PeerSwarm};

impl PeerSwarm {
    pub(crate) fn new(event_capacity: usize) -> Self {
        let (event_tx, event_rx) = mpsc::channel(event_capacity.max(1));
        Self {
            actors: Vec::new(),
            indices: std::collections::HashMap::new(),
            peer_id_counts: std::collections::HashMap::new(),
            endpoint_counts: std::collections::HashMap::new(),
            recently_dropped_endpoints: std::collections::VecDeque::new(),
            known_seeders: std::collections::HashSet::new(),
            known_seeder_order: std::collections::VecDeque::new(),
            wanted_pieces: Arc::from([]),
            local_seeder: false,
            local_metadata: None,
            peer_snapshot_store: None,
            last_stats_snapshot_publish: None,
            stats_snapshot_dirty: false,
            upload_rate: Arc::new(SwarmUploadRate::default()),
            event_tx: Some(event_tx),
            event_rx: Some(event_rx),
        }
    }

    /// Drop the receiver so actors blocked on a full event queue can exit.
    pub(crate) fn close_event_receiver(&mut self) {
        self.event_rx = None;
    }

    /// Prevent this registry from admitting actors into its event stream.
    pub(crate) fn close_event_sender(&mut self) {
        self.event_tx = None;
    }

    pub(crate) fn event_sender(&self) -> Option<mpsc::Sender<PeerEvent>> {
        self.event_tx.as_ref().cloned()
    }

    pub(crate) fn upload_rate(&self) -> Arc<SwarmUploadRate> {
        Arc::clone(&self.upload_rate)
    }

    pub(crate) fn upload_speed_at(&self, now: Instant) -> u64 {
        self.upload_rate.speed_at(now)
    }

    pub(crate) fn lease_event_receiver(&mut self) -> Option<super::PeerSwarmEventLease<'_>> {
        let receiver = self.event_rx.take()?;
        Some(super::PeerSwarmEventLease {
            swarm: self,
            receiver: Some(receiver),
        })
    }

    pub(crate) fn spawn_peer(
        &mut self,
        connection: BtPeerConn,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        provider: Arc<dyn PieceDataProvider>,
    ) -> Result<PeerActorId, Box<BtPeerConn>> {
        self.spawn_peer_with_provider(connection, dht_engine, Some(provider))
    }

    /// Admit a connection whose metadata bootstrap will run inside its peer
    /// actor before torrent piece geometry is known.
    pub(crate) fn spawn_metadata_peer(
        &mut self,
        mut connection: BtPeerConn,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    ) -> Result<PeerActorId, Box<BtPeerConn>> {
        connection.enter_metadata_mode();
        self.spawn_peer_with_provider(connection, dht_engine, None)
    }

    /// Transition every metadata-bootstrap actor to the parsed torrent's
    /// piece layout without replacing its connection or actor task.
    pub(crate) async fn activate_payload_actors(
        &mut self,
        config: Arc<PeerActorPayloadConfig>,
    ) -> usize {
        self.local_metadata = Some(Arc::clone(&config.local_metadata));
        let actors = self
            .actors
            .iter()
            .filter(|actor| actor.metadata_pending && !actor.dead)
            .map(|actor| (actor.actor_id, actor.handle()))
            .collect::<Vec<_>>();
        let mut disconnected = Vec::new();
        let mut activated = 0;
        for (actor_id, control) in actors {
            if control
                .send(PeerCommand::ActivatePayload(Arc::clone(&config)))
                .await
                .is_ok()
            {
                if let Some(actor) = self.actor_mut(actor_id) {
                    actor.metadata_pending = false;
                }
                activated += 1;
            } else {
                disconnected.push(actor_id);
            }
        }
        for actor_id in disconnected {
            self.mark_dead(actor_id);
        }
        activated
    }

    fn spawn_peer_with_provider(
        &mut self,
        mut connection: BtPeerConn,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        provider: Option<Arc<dyn PieceDataProvider>>,
    ) -> Result<PeerActorId, Box<BtPeerConn>> {
        let Some(event_tx) = self.event_sender() else {
            return Err(Box::new(connection));
        };
        let Some(endpoint) = connection.remote_endpoint() else {
            return Err(Box::new(connection));
        };
        if connection.local_metadata.is_none() {
            connection.local_metadata = self.local_metadata.as_ref().map(Arc::clone);
        }
        let actor_id = connection.actor_id;
        let wanted_pieces = Arc::clone(&self.wanted_pieces);
        self.insert(PeerActorEntry::spawn(
            actor_id, endpoint, connection, dht_engine, provider, event_tx,
        ));
        let actor_control = self.actor(actor_id).map(PeerActorEntry::handle);
        if actor_control.is_none_or(|control| {
            control.set_wanted_pieces(wanted_pieces).is_err()
                || !control.set_local_seeder(self.local_seeder)
        }) {
            self.mark_dead(actor_id);
        }
        self.publish_peer_snapshots();
        Ok(actor_id)
    }

    pub(crate) fn len(&self) -> usize {
        self.actors.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.actors.is_empty()
    }

    pub(crate) fn has_endpoint(&self, endpoint: SocketAddr) -> bool {
        self.endpoint_counts.contains_key(&endpoint)
    }

    pub(crate) fn is_known_seeder(&self, endpoint: SocketAddr) -> bool {
        self.known_seeders.contains(&endpoint)
    }

    pub(crate) fn recently_dropped_endpoints(
        &self,
    ) -> impl Iterator<Item = (SocketAddr, Instant)> + '_ {
        self.recently_dropped_endpoints.iter().copied()
    }

    pub(crate) fn has_peer_id(&self, peer_id: [u8; 20]) -> bool {
        self.peer_id_counts.contains_key(&peer_id)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &PeerActorEntry> {
        self.actors.iter()
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut PeerActorEntry> {
        self.actors.iter_mut()
    }

    pub(crate) fn actor(&self, actor_id: PeerActorId) -> Option<&PeerActorEntry> {
        let index = *self.indices.get(&actor_id)?;
        self.actors.get(index)
    }

    pub(crate) fn actor_mut(&mut self, actor_id: PeerActorId) -> Option<&mut PeerActorEntry> {
        let index = *self.indices.get(&actor_id)?;
        self.actors.get_mut(index)
    }

    pub(crate) fn set_upload_choked(&self, actor_id: PeerActorId, choked: bool) -> bool {
        self.actor(actor_id)
            .is_some_and(|actor| actor.handle().set_upload_choked(choked))
    }

    pub(crate) async fn send_to(
        &self,
        actor_id: PeerActorId,
        command: PeerCommand,
    ) -> Result<(), mpsc::error::SendError<PeerCommand>> {
        let Some(actor) = self.actor(actor_id) else {
            return Err(mpsc::error::SendError(command));
        };
        actor.handle().send(command).await
    }

    pub(crate) fn try_send_to(
        &self,
        actor_id: PeerActorId,
        command: PeerCommand,
    ) -> Result<(), mpsc::error::TrySendError<PeerCommand>> {
        let Some(actor) = self.actor(actor_id) else {
            return Err(mpsc::error::TrySendError::Closed(command));
        };
        actor.try_send(command)
    }

    /// Broadcast a validated piece to every live peer through its bounded
    /// actor mailbox. Awaiting each send preserves backpressure instead of
    /// silently dropping Have notifications when a mailbox is full.
    pub(crate) async fn broadcast_have(&mut self, piece_index: u32) {
        let mut disconnected = Vec::new();
        for actor in self.actors.iter().filter(|actor| !actor.dead) {
            if actor
                .handle()
                .send(PeerCommand::HavePiece { piece_index })
                .await
                .is_err()
            {
                disconnected.push(actor.actor_id);
            }
        }
        for actor_id in disconnected {
            self.mark_dead(actor_id);
        }
    }

    pub(crate) fn insert(&mut self, actor: PeerActorEntry) {
        let index = self.actors.len();
        self.indices.insert(actor.actor_id, index);
        *self.peer_id_counts.entry(actor.stats.peer_id).or_default() += 1;
        *self.endpoint_counts.entry(actor.endpoint).or_default() += 1;
        self.actors.push(actor);
    }

    pub(crate) fn mark_dead(&mut self, actor_id: PeerActorId) {
        let marked = if let Some(actor) = self.actor_mut(actor_id) {
            actor.dead = true;
            true
        } else {
            false
        };
        if marked {
            self.publish_peer_snapshots();
        }
    }

    pub(crate) fn set_local_metadata(&mut self, metadata: Arc<[u8]>) {
        self.local_metadata = Some(metadata);
    }

    pub(crate) fn set_wanted_pieces(&mut self, wanted_pieces: Arc<[u8]>) {
        if self.wanted_pieces.as_ref() == wanted_pieces.as_ref() {
            return;
        }
        self.wanted_pieces = Arc::clone(&wanted_pieces);
        let actors = self
            .actors
            .iter()
            .filter(|actor| !actor.dead)
            .map(|actor| (actor.actor_id, actor.handle()))
            .collect::<Vec<_>>();
        for (actor_id, control) in actors {
            if control
                .set_wanted_pieces(Arc::clone(&wanted_pieces))
                .is_err()
            {
                self.mark_dead(actor_id);
            }
        }
    }

    pub(crate) fn set_local_seeder(&mut self, local_seeder: bool) {
        if self.local_seeder == local_seeder {
            return;
        }
        self.local_seeder = local_seeder;
        let actors = self
            .actors
            .iter()
            .filter(|actor| !actor.dead)
            .map(|actor| (actor.actor_id, actor.handle()))
            .collect::<Vec<_>>();
        for (actor_id, control) in actors {
            if !control.set_local_seeder(local_seeder) {
                self.mark_dead(actor_id);
            }
        }
    }

    pub(crate) async fn remove_dead(&mut self) -> Vec<(PeerActorId, SocketAddr)> {
        let mut removed = Vec::new();
        let mut dropped_advertisements = Vec::new();
        for index in (0..self.actors.len()).rev() {
            if self.actors[index].dead {
                let actor_id = self.actors[index].actor_id;
                let endpoint = self.actors[index].endpoint;
                let actor = &self.actors[index];
                let dropped_advertisement = if actor.incoming {
                    None
                } else {
                    actor
                        .advertised_endpoint
                        .zip(actor.graceful_disconnected_at)
                };
                // Keep the entry registered until shutdown completes. If this
                // future is cancelled while awaiting the actor, the next
                // maintenance pass can safely resume the same shutdown.
                self.actors[index].shutdown().await;
                removed.push((actor_id, endpoint));
                if let Some(dropped_advertisement) = dropped_advertisement {
                    dropped_advertisements.push(dropped_advertisement);
                }
            }
        }
        if !removed.is_empty() {
            for dropped_advertisement @ (endpoint, _) in dropped_advertisements {
                if let Some(index) = self
                    .recently_dropped_endpoints
                    .iter()
                    .position(|(known, _)| *known == endpoint)
                {
                    self.recently_dropped_endpoints.remove(index);
                }
                self.recently_dropped_endpoints
                    .push_front(dropped_advertisement);
                self.recently_dropped_endpoints
                    .truncate(super::MAX_RECENTLY_DROPPED_PEERS);
            }
            // Do not compact and rebuild the stable-ID index once per dead
            // peer. All awaits happen before mutation, so cancellation leaves
            // every entry registered and the next pass can resume shutdown.
            self.actors.retain(|actor| !actor.dead);
            self.rebuild_index();
        }
        removed
    }

    pub(crate) async fn shutdown_all(&mut self) {
        // Shutdown is terminal for this registry. Closing both ends first
        // unblocks actors awaiting bounded event delivery and prevents new
        // actors from being admitted while existing tasks are joined.
        self.close_event_receiver();
        self.close_event_sender();
        futures::future::join_all(self.actors.iter_mut().map(PeerActorEntry::shutdown)).await;
        self.actors.clear();
        self.indices.clear();
        self.peer_id_counts.clear();
        self.endpoint_counts.clear();
        self.recently_dropped_endpoints.clear();
    }

    fn rebuild_index(&mut self) {
        self.indices.clear();
        self.peer_id_counts.clear();
        self.endpoint_counts.clear();
        for (index, actor) in self.actors.iter().enumerate() {
            self.indices.insert(actor.actor_id, index);
            *self.peer_id_counts.entry(actor.stats.peer_id).or_default() += 1;
            *self.endpoint_counts.entry(actor.endpoint).or_default() += 1;
        }
    }
}

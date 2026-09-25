//! Owned peer actors and stable-ID routing for a torrent swarm.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::engine::bt_peer_connection::{BtPeerConn, PeerActorId};
use crate::engine::bt_upload_session::PieceDataProvider;
use crate::engine::peer_stats::PeerStats;

use super::{PeerActorControl, PeerActorTask, PeerCommand, PeerEvent};

/// One long-lived I/O owner for a handshaken BitTorrent connection.
pub(crate) struct PeerActorEntry {
    pub(crate) actor_id: PeerActorId,
    pub(crate) endpoint: SocketAddr,
    pub(crate) dead: bool,
    pub(crate) incoming: bool,
    pub(crate) source: crate::request::request_group::BtPeerSource,
    pub(crate) stats: PeerStats,
    pub(crate) has_bitfield: bool,
    pub(crate) bitfield: Vec<u8>,
    pub(crate) seeder: bool,
    pub(crate) ut_pex_id: Option<u8>,
    actor: PeerActorTask,
}

impl PeerActorEntry {
    pub(crate) fn spawn(
        actor_id: PeerActorId,
        connection: BtPeerConn,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        provider: Arc<dyn PieceDataProvider>,
        event_tx: mpsc::Sender<PeerEvent>,
    ) -> Self {
        let stats = connection.stats.clone();
        let seeder = connection.seeder;
        let incoming = connection.incoming;
        let source = connection.source;
        let ut_pex_id = connection.peer_extension_id("ut_pex");
        let has_bitfield = connection.session_resource.is_some();
        let bitfield = connection
            .session_resource
            .as_ref()
            .map_or_else(Vec::new, |resource| resource.bitfield().to_vec());
        let endpoint = format!("{}:{}", connection.remote_ip(), connection.remote_port())
            .parse()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
        let actor = PeerActorTask::spawn_owned(
            actor_id,
            connection,
            event_tx,
            dht_engine,
            Some(provider),
            16,
        );

        Self {
            actor_id,
            endpoint,
            dead: false,
            incoming,
            source,
            stats,
            has_bitfield,
            bitfield,
            seeder,
            ut_pex_id,
            actor,
        }
    }

    pub(crate) fn handle(&self) -> PeerActorControl {
        self.actor.control.clone()
    }

    pub(crate) fn try_send(
        &self,
        command: PeerCommand,
    ) -> Result<(), mpsc::error::TrySendError<PeerCommand>> {
        self.actor.control.try_send(command)
    }

    pub(crate) async fn shutdown(&mut self) {
        let _ = self.actor.shutdown().await;
    }
}

fn update_peer_availability(actor: &mut PeerActorEntry, piece_index: u32, has_piece: bool) {
    let byte_index = piece_index as usize / 8;
    if has_piece && actor.bitfield.len() <= byte_index {
        actor.bitfield.resize(byte_index + 1, 0);
    }
    if let Some(byte) = actor.bitfield.get_mut(byte_index) {
        let mask = 0x80 >> (piece_index % 8);
        if has_piece {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }
}

/// Torrent-scoped owner for peer actors and their bounded I/O event stream.
#[derive(Default)]
pub(crate) struct PeerSwarm {
    actors: Vec<PeerActorEntry>,
    indices: HashMap<PeerActorId, usize>,
    peer_id_counts: HashMap<[u8; 20], usize>,
    endpoint_counts: HashMap<SocketAddr, usize>,
    pub(crate) event_tx: Option<mpsc::Sender<PeerEvent>>,
    pub(crate) event_rx: Option<mpsc::Receiver<PeerEvent>>,
}

/// Temporarily lends the swarm event receiver to one coordinator without
/// losing it when that coordinator's future is cancelled.
pub(crate) struct PeerSwarmEventLease<'a> {
    swarm: &'a mut PeerSwarm,
    receiver: Option<mpsc::Receiver<PeerEvent>>,
}

impl PeerSwarmEventLease<'_> {
    pub(crate) async fn recv(&mut self) -> Option<PeerEvent> {
        let event = self.recv_unapplied().await?;
        self.swarm.apply_event(&event);
        Some(event)
    }

    /// Receive an event without mutating the registry. Coordinators that
    /// apply additional event-side effects can use this to apply registry
    /// state exactly once in their common event handler.
    pub(crate) async fn recv_unapplied(&mut self) -> Option<PeerEvent> {
        self.receiver.as_mut()?.recv().await
    }

    pub(crate) fn try_recv(&mut self) -> Result<PeerEvent, mpsc::error::TryRecvError> {
        let event = self.try_recv_unapplied()?;
        self.swarm.apply_event(&event);
        Ok(event)
    }

    pub(crate) fn try_recv_unapplied(&mut self) -> Result<PeerEvent, mpsc::error::TryRecvError> {
        self.receiver
            .as_mut()
            .ok_or(mpsc::error::TryRecvError::Disconnected)?
            .try_recv()
    }

    /// Admit a handshaken connection through the same coordinator lease that
    /// owns event consumption, so dynamic joins cannot race another swarm
    /// mutator while the event loop is active.
    pub(crate) fn spawn_peer(
        &mut self,
        connection: BtPeerConn,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        provider: Arc<dyn PieceDataProvider>,
    ) -> Result<PeerActorId, Box<BtPeerConn>> {
        self.swarm.spawn_peer(connection, dht_engine, provider)
    }

    pub(crate) fn actor_mut(&mut self, actor_id: PeerActorId) -> Option<&mut PeerActorEntry> {
        self.swarm.actor_mut(actor_id)
    }
}

impl Drop for PeerSwarmEventLease<'_> {
    fn drop(&mut self) {
        if self.swarm.event_rx.is_none() {
            self.swarm.event_rx = self.receiver.take();
        }
    }
}

impl PeerSwarm {
    pub(crate) fn new(event_capacity: usize) -> Self {
        let (event_tx, event_rx) = mpsc::channel(event_capacity.max(1));
        Self {
            actors: Vec::new(),
            indices: HashMap::new(),
            peer_id_counts: HashMap::new(),
            endpoint_counts: HashMap::new(),
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

    pub(crate) fn lease_event_receiver(&mut self) -> Option<PeerSwarmEventLease<'_>> {
        let receiver = self.event_rx.take()?;
        Some(PeerSwarmEventLease {
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
        let Some(event_tx) = self.event_sender() else {
            return Err(Box::new(connection));
        };
        let actor_id = connection.actor_id;
        self.insert(PeerActorEntry::spawn(
            actor_id, connection, dht_engine, provider, event_tx,
        ));
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
        if let Some(actor) = self.actor_mut(actor_id) {
            actor.dead = true;
        }
    }

    /// Apply one consumed I/O event to the registry-owned peer snapshot.
    pub(crate) fn apply_event(&mut self, event: &PeerEvent) {
        match event {
            PeerEvent::InterestChanged {
                actor_id, snapshot, ..
            }
            | PeerEvent::ChokeStateChanged {
                actor_id, snapshot, ..
            }
            | PeerEvent::UploadBytes {
                actor_id, snapshot, ..
            }
            | PeerEvent::UploadQueueChanged {
                actor_id, snapshot, ..
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.stats = (**snapshot).clone();
                }
            }
            PeerEvent::Message {
                actor_id,
                stats: Some(snapshot),
                ..
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.stats = (**snapshot).clone();
                }
            }
            PeerEvent::PeerAvailabilityChanged {
                actor_id,
                piece_index,
                has_piece,
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    update_peer_availability(actor, *piece_index, *has_piece);
                }
            }
            PeerEvent::PeerAvailabilitySnapshot {
                actor_id,
                bitfield,
                seeder,
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.bitfield.clone_from(bitfield);
                    actor.has_bitfield = true;
                    actor.seeder = *seeder;
                }
            }
            PeerEvent::PeerChokingChanged {
                actor_id,
                peer_choking,
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.stats.peer_choking = *peer_choking;
                }
            }
            PeerEvent::Disconnected { actor_id } | PeerEvent::RequestFailed { actor_id, .. } => {
                self.mark_dead(*actor_id);
            }
            PeerEvent::AvailabilityChanged { .. }
            | PeerEvent::Message { stats: None, .. }
            | PeerEvent::PexPeers { .. } => {}
        }
    }

    pub(crate) async fn remove_dead(&mut self) -> Vec<(PeerActorId, SocketAddr)> {
        let mut removed = Vec::new();
        for index in (0..self.actors.len()).rev() {
            if self.actors[index].dead {
                let actor_id = self.actors[index].actor_id;
                let endpoint = self.actors[index].endpoint;
                // Keep the entry registered until shutdown completes. If this
                // future is cancelled while awaiting the actor, the next
                // maintenance pass can safely resume the same shutdown.
                self.actors[index].shutdown().await;
                removed.push((actor_id, endpoint));
            }
        }
        if !removed.is_empty() {
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
        for actor in &mut self.actors {
            actor.shutdown().await;
        }
        self.actors.clear();
        self.indices.clear();
        self.peer_id_counts.clear();
        self.endpoint_counts.clear();
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

#[cfg(test)]
mod tests {
    use super::super::peer_request::RequestGeneration;
    use super::super::pipelined::BlockRequest;
    use super::*;
    use crate::engine::bt_upload_session::{InMemoryPieceProvider, PieceDataProvider};
    use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
    use aria2_protocol::bittorrent::peer::connection::PeerConnection;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn swarm_coordinator_lease_can_admit_a_peer_without_releasing_event_ownership() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        {
            let mut coordinator = swarm.lease_event_receiver().unwrap();
            assert!(matches!(
                coordinator.spawn_peer(connection, None, provider),
                Ok(registered_actor_id) if registered_actor_id == actor_id
            ));
            assert!(coordinator.actor_mut(actor_id).is_some());
        }
        assert_eq!(swarm.len(), 1);
        assert!(swarm.actor(actor_id).is_some());

        swarm.shutdown_all().await;
        drop(remote_stream);
    }

    #[tokio::test]
    async fn upload_choke_command_updates_peer_wire_state_and_swarm_snapshot() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        let actor_id = connection.actor_id;
        connection.configure_upload_with_auto_unchoke(
            &crate::engine::bt_upload_session::BtSeedingConfig::default(),
            1,
            16,
            false,
        );
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        assert!(swarm.spawn_peer(connection, None, provider).is_ok());

        swarm
            .send_to(actor_id, PeerCommand::UnchokeUpload)
            .await
            .unwrap();
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Unchoke
        );

        {
            let mut events = swarm.lease_event_receiver().unwrap();
            loop {
                let event = timeout(Duration::from_secs(1), events.recv())
                    .await
                    .expect("actor should report the upload state transition")
                    .expect("swarm event stream should remain open");
                if matches!(event, PeerEvent::ChokeStateChanged { actor_id: id, .. } if id == actor_id)
                {
                    break;
                }
            }
        }
        assert!(!swarm.actor(actor_id).unwrap().stats.am_choking);
        swarm.shutdown_all().await;
    }

    #[tokio::test]
    async fn peer_actor_sends_pex_frame_with_the_remote_extension_id() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        connection.register_peer_extension("ut_pex", 19);
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        assert!(swarm.spawn_peer(connection, None, provider).is_ok());

        let payload =
            aria2_protocol::bittorrent::extension::pex::PexHandler::build_pex_message(&[], &[])
                .encode();
        let wire = aria2_protocol::bittorrent::message::serializer::serialize_extended(19, payload);
        swarm
            .send_to(actor_id, PeerCommand::SendPex(wire))
            .await
            .unwrap();

        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        assert!(matches!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Extended { ext_id: 19, .. }
        ));
        swarm.shutdown_all().await;
    }

    #[tokio::test]
    async fn swarm_broadcasts_validated_piece_have_through_peer_actors() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 4));
        let mut swarm = PeerSwarm::new(8);
        assert!(swarm.spawn_peer(connection, None, provider).is_ok());

        swarm.broadcast_have(3).await;

        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Have { piece_index: 3 }
        );
        swarm.shutdown_all().await;
    }

    #[tokio::test]
    async fn swarm_registry_tracks_download_bytes_reported_by_peer_actor() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        connection.allocate_session_resource(16, 1, 16);
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        assert_eq!(
            swarm.spawn_peer(connection, None, provider).ok(),
            Some(actor_id)
        );

        let generation = RequestGeneration::allocate();
        let request = BlockRequest {
            block_index: 0,
            offset: 0,
            length: 16,
        };
        let control = swarm.actor(actor_id).unwrap().handle();
        control.begin_generation(generation, 0).await.unwrap();
        control.request(generation, 0, request).await.unwrap();

        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Request {
                request: PieceBlockRequest::new(0, 0, 16),
            }
        );
        remote
            .send_message(&BtMessage::Piece {
                index: 0,
                begin: 0,
                data: vec![0x6a; 16].into(),
            })
            .await
            .unwrap();

        let mut event_lease = swarm.lease_event_receiver().unwrap();
        let event = timeout(Duration::from_secs(1), event_lease.recv())
            .await
            .unwrap()
            .unwrap();
        drop(event_lease);
        assert!(matches!(
            event,
            PeerEvent::Message {
                actor_id: event_actor,
                message: BtMessage::Piece { .. },
                stats: Some(_),
                ..
            } if event_actor == actor_id
        ));
        assert_eq!(swarm.actor(actor_id).unwrap().stats.downloaded_bytes, 16);

        swarm.shutdown_all().await;
    }

    #[tokio::test]
    async fn shutdown_all_closes_a_full_event_queue_before_joining_peer_actors() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        let mut piece_provider = InMemoryPieceProvider::new(16, 1);
        piece_provider.set_piece_data(0, vec![0x5a; 16]);
        let provider: Arc<dyn PieceDataProvider> = Arc::new(piece_provider);
        connection.configure_upload_with_auto_unchoke(
            &crate::engine::bt_upload_session::BtSeedingConfig::default(),
            1,
            16,
            true,
        );
        connection.choke_upload_peer().await.unwrap();
        connection.unchoke_upload_peer().await.unwrap();

        let actor_id = connection.actor_id;
        let mut swarm = PeerSwarm::new(1);
        assert!(matches!(
            swarm.spawn_peer(connection, None, Arc::clone(&provider)),
            Ok(registered_actor_id) if registered_actor_id == actor_id
        ));
        let event_sender = swarm.event_sender().unwrap();
        event_sender
            .send(PeerEvent::PeerChokingChanged {
                actor_id,
                peer_choking: true,
            })
            .await
            .unwrap();

        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Choke
        );
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Unchoke
        );
        remote.send_message(&BtMessage::Interested).await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Unchoke
        );

        timeout(Duration::from_secs(1), swarm.shutdown_all())
            .await
            .expect("swarm shutdown must not wait on a full peer event queue");
        assert!(swarm.is_empty());
        assert!(swarm.event_sender().is_none());
        drop(event_sender);
    }

    #[tokio::test]
    async fn peer_actor_forwards_negotiated_pex_peers_as_a_swarm_event() {
        use aria2_protocol::bittorrent::message::extension::{CompactPeerV4, UtPexMessage};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        connection.allocate_session_resource(16, 1, 16);
        connection.set_pex_enabled(true);
        connection.register_peer_extension("ut_pex", 9);
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        assert!(matches!(
            swarm.spawn_peer(connection, None, provider),
            Ok(registered_actor_id) if registered_actor_id == actor_id
        ));

        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        let mut pex = UtPexMessage::new();
        pex.added.push(CompactPeerV4([127, 0, 0, 1, 0x1a, 0xe1]));
        remote
            .send_message(&BtMessage::Extended {
                ext_id: 9,
                payload: pex.to_payload(),
            })
            .await
            .unwrap();

        let event = {
            let mut events = swarm.lease_event_receiver().unwrap();
            timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap()
        };
        match event {
            PeerEvent::PexPeers { peers } => {
                assert_eq!(peers.len(), 1);
                assert_eq!(peers[0].ip, "127.0.0.1");
                assert_eq!(peers[0].port, 6881);
            }
            _ => panic!("expected PEX peer event"),
        }

        swarm.shutdown_all().await;
    }

    #[tokio::test]
    async fn unapplied_event_lease_leaves_registry_updates_to_its_coordinator() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        assert_eq!(
            swarm
                .spawn_peer(connection, None, provider)
                .unwrap_or_else(|_| unreachable!()),
            actor_id
        );
        let old_choke_state = swarm.actor(actor_id).unwrap().stats.peer_choking;
        let event_tx = swarm.event_sender().unwrap();
        let event = PeerEvent::PeerChokingChanged {
            actor_id,
            peer_choking: !old_choke_state,
        };
        event_tx.send(event).await.unwrap();

        let event = {
            let mut lease = swarm.lease_event_receiver().unwrap();
            lease.recv_unapplied().await.unwrap()
        };
        assert_eq!(
            swarm.actor(actor_id).unwrap().stats.peer_choking,
            old_choke_state
        );

        swarm.apply_event(&event);
        assert_eq!(
            swarm.actor(actor_id).unwrap().stats.peer_choking,
            !old_choke_state
        );
        swarm.shutdown_all().await;
        drop(remote_stream);
    }

    #[tokio::test]
    async fn registry_actor_handle_survives_piece_generation_rollover() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        connection.allocate_session_resource(16, 8, 128);
        let actor_id = connection.actor_id;
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut registry = PeerSwarm::new(8);
        registry.insert(PeerActorEntry::spawn(
            actor_id, connection, None, provider, event_tx,
        ));
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        let peer_handle = registry.actor(actor_id).unwrap().handle();
        let first_generation = RequestGeneration::allocate();
        let first_request = BlockRequest {
            block_index: 0,
            offset: 0,
            length: 16,
        };

        peer_handle
            .send(PeerCommand::BeginGeneration {
                generation: first_generation,
                piece_index: 2,
            })
            .await
            .unwrap();
        peer_handle
            .send(PeerCommand::Request {
                generation: first_generation,
                piece_index: 2,
                request: first_request,
            })
            .await
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Request {
                request: PieceBlockRequest::new(2, 0, 16),
            }
        );
        peer_handle
            .send(PeerCommand::EndGeneration {
                generation: first_generation,
                piece_index: 2,
            })
            .await
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Cancel {
                request: PieceBlockRequest::new(2, 0, 16),
            }
        );
        remote
            .send_message(&BtMessage::Piece {
                index: 2,
                begin: 0,
                data: vec![2; 16].into(),
            })
            .await
            .unwrap();
        let stale_response_deadline = tokio::time::Instant::now() + Duration::from_millis(25);
        while tokio::time::Instant::now() < stale_response_deadline {
            while let Ok(event) = event_rx.try_recv() {
                match event {
                    PeerEvent::Message { generation, .. } if generation == first_generation => {
                        panic!("stale block response escaped the peer actor")
                    }
                    PeerEvent::Disconnected { .. } => {
                        panic!("peer actor disconnected while ignoring a stale block")
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        let next_generation = RequestGeneration::allocate();
        peer_handle
            .send(PeerCommand::BeginGeneration {
                generation: next_generation,
                piece_index: 5,
            })
            .await
            .unwrap();
        peer_handle
            .send(PeerCommand::Request {
                generation: next_generation,
                piece_index: 5,
                request: first_request,
            })
            .await
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Request {
                request: PieceBlockRequest::new(5, 0, 16),
            }
        );
        remote
            .send_message(&BtMessage::Piece {
                index: 5,
                begin: 0,
                data: vec![5; 16].into(),
            })
            .await
            .unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            PeerEvent::Message {
                actor_id: received_actor,
                generation,
                message: BtMessage::Piece { index: 5, .. },
                ..
            } if received_actor == actor_id && generation == next_generation
        ));

        registry.shutdown_all().await;
    }

    #[tokio::test]
    async fn actor_availability_messages_update_swarm_seeder_snapshot_end_to_end() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        connection.allocate_session_resource(16, 8, 128);
        connection.incoming = false;
        connection.source = crate::request::request_group::BtPeerSource::Tracker;
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        assert!(swarm.spawn_peer(connection, None, provider).is_ok());
        assert!(!swarm.actor(actor_id).unwrap().incoming);
        assert!(swarm.actor(actor_id).unwrap().has_bitfield);
        assert_eq!(
            swarm.actor(actor_id).unwrap().source,
            crate::request::request_group::BtPeerSource::Tracker
        );
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);

        for (message, expected_seeder) in [(BtMessage::HaveAll, true), (BtMessage::HaveNone, false)]
        {
            remote.send_message(&message).await.unwrap();
            let events = {
                let mut receiver = swarm.lease_event_receiver().unwrap();
                let mut events = Vec::new();
                loop {
                    let event = timeout(Duration::from_secs(1), receiver.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    let reached_snapshot = matches!(
                        event,
                        PeerEvent::PeerAvailabilitySnapshot { actor_id: event_actor, .. }
                            if event_actor == actor_id
                    );
                    events.push(event);
                    if reached_snapshot {
                        break;
                    }
                }
                events
            };
            for event in &events {
                swarm.apply_event(event);
            }

            let actor = swarm.actor(actor_id).unwrap();
            assert_eq!(actor.seeder, expected_seeder);
            assert_eq!(actor.bitfield, if expected_seeder { [0xff] } else { [0] });
        }

        for (message, expected_choking) in [(BtMessage::Unchoke, false), (BtMessage::Choke, true)] {
            remote.send_message(&message).await.unwrap();
            let event = {
                let mut receiver = swarm.lease_event_receiver().unwrap();
                loop {
                    let event = timeout(Duration::from_secs(1), receiver.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    if matches!(
                        event,
                        PeerEvent::PeerChokingChanged { actor_id: event_actor, .. }
                            if event_actor == actor_id
                    ) {
                        break event;
                    }
                }
            };
            swarm.apply_event(&event);
            assert_eq!(
                swarm.actor(actor_id).unwrap().stats.peer_choking,
                expected_choking
            );
        }

        swarm.shutdown_all().await;
    }

    #[tokio::test]
    async fn removing_dead_actor_cancels_inflight_request_before_joining() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        connection.allocate_session_resource(1, 16, 16);
        let actor_id = connection.actor_id;
        let (event_tx, _event_rx) = mpsc::channel(8);
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut registry = PeerSwarm::new(8);
        registry.insert(PeerActorEntry::spawn(
            actor_id, connection, None, provider, event_tx,
        ));
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        let peer_handle = registry.actor(actor_id).unwrap().handle();
        let generation = RequestGeneration::allocate();
        let request = BlockRequest {
            block_index: 0,
            offset: 0,
            length: 16,
        };

        peer_handle
            .send(PeerCommand::BeginGeneration {
                generation,
                piece_index: 3,
            })
            .await
            .unwrap();
        peer_handle
            .send(PeerCommand::Request {
                generation,
                piece_index: 3,
                request,
            })
            .await
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Request {
                request: PieceBlockRequest::new(3, 0, 16),
            }
        );

        registry.mark_dead(actor_id);
        assert_eq!(
            timeout(Duration::from_secs(1), registry.remove_dead())
                .await
                .unwrap(),
            vec![(actor_id, endpoint)]
        );
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Cancel {
                request: PieceBlockRequest::new(3, 0, 16),
            }
        );
        assert!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn cancelled_actor_shutdown_keeps_join_handle_for_retry() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        let actor_id = connection.actor_id;
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let mut actor =
            PeerActorTask::spawn_owned(actor_id, connection, event_tx.clone(), None, None, 4);
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);

        event_tx
            .send(PeerEvent::PeerAvailabilityChanged {
                actor_id,
                piece_index: 0,
                has_piece: false,
            })
            .await
            .unwrap();
        remote
            .send_message(&BtMessage::Have { piece_index: 0 })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        {
            let shutdown = actor.shutdown();
            tokio::pin!(shutdown);
            assert!(
                timeout(Duration::from_millis(25), &mut shutdown)
                    .await
                    .is_err()
            );
        }

        assert!(matches!(
            event_rx.recv().await,
            Some(PeerEvent::PeerAvailabilityChanged { .. })
        ));
        assert!(matches!(
            timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap(),
            Some(PeerEvent::PeerAvailabilityChanged { .. })
        ));
        timeout(Duration::from_secs(1), actor.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn removing_multiple_dead_actors_rebuilds_stable_id_index() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let (event_tx, _event_rx) = mpsc::channel(8);
        let mut registry = PeerSwarm::new(8);
        let mut endpoints = Vec::new();
        let mut actor_ids = Vec::new();
        let mut remote_streams = Vec::new();

        for _ in 0..3 {
            let remote_stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (local_stream, endpoint) = listener.accept().await.unwrap();
            let connection = BtPeerConn::from_incoming_plain(
                PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
                endpoint,
            );
            actor_ids.push(connection.actor_id);
            endpoints.push(endpoint);
            registry.insert(PeerActorEntry::spawn(
                connection.actor_id,
                connection,
                None,
                Arc::clone(&provider),
                event_tx.clone(),
            ));
            remote_streams.push(remote_stream);
        }

        registry.mark_dead(actor_ids[0]);
        registry.mark_dead(actor_ids[2]);
        assert_eq!(
            timeout(Duration::from_secs(1), registry.remove_dead())
                .await
                .unwrap(),
            vec![(actor_ids[2], endpoints[2]), (actor_ids[0], endpoints[0])]
        );
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.actor(actor_ids[1]).map(|actor| actor.actor_id),
            Some(actor_ids[1])
        );
        assert!(registry.actor(actor_ids[0]).is_none());
        assert!(registry.actor(actor_ids[2]).is_none());

        registry.shutdown_all().await;
        drop(remote_streams);
    }

    #[tokio::test]
    async fn registry_applies_peer_availability_and_stats_events_by_actor_id() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        connection.allocate_session_resource(1, 16, 16);
        connection.set_peer_bitfield(&[0x80, 0]);
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut registry = PeerSwarm::new(8);
        let event_tx = registry.event_sender().unwrap();
        assert_eq!(
            registry.spawn_peer(connection, None, provider).ok(),
            Some(actor_id)
        );

        event_tx
            .send(PeerEvent::PeerAvailabilityChanged {
                actor_id,
                piece_index: 9,
                has_piece: true,
            })
            .await
            .unwrap();
        let event = registry
            .lease_event_receiver()
            .unwrap()
            .recv()
            .await
            .unwrap();
        registry.apply_event(&event);
        assert_eq!(registry.actor(actor_id).unwrap().bitfield, [0x80, 0x40]);

        let mut snapshot = registry.actor(actor_id).unwrap().stats.clone();
        snapshot.outstanding_upload_count = 2;
        event_tx
            .send(PeerEvent::UploadQueueChanged {
                actor_id,
                snapshot: Box::new(snapshot),
            })
            .await
            .unwrap();
        let event = registry
            .lease_event_receiver()
            .unwrap()
            .recv()
            .await
            .unwrap();
        registry.apply_event(&event);
        assert_eq!(
            registry
                .actor(actor_id)
                .unwrap()
                .stats
                .outstanding_upload_count,
            2
        );

        event_tx
            .send(PeerEvent::PeerAvailabilitySnapshot {
                actor_id,
                bitfield: vec![0x01],
                seeder: true,
            })
            .await
            .unwrap();
        let event = registry
            .lease_event_receiver()
            .unwrap()
            .recv()
            .await
            .unwrap();
        registry.apply_event(&event);
        assert_eq!(registry.actor(actor_id).unwrap().bitfield, [0x01]);
        assert!(registry.actor(actor_id).unwrap().seeder);
        registry.shutdown_all().await;
        drop(remote_stream);
    }

    #[tokio::test]
    async fn swarm_owns_peer_actor_and_routes_bounded_state_events() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        connection.allocate_session_resource(1, 16, 16);
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(4);
        assert!(swarm.spawn_peer(connection, None, provider).is_ok());
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);

        {
            let mut events = swarm.lease_event_receiver().unwrap();
            assert!(
                timeout(Duration::from_millis(1), events.recv())
                    .await
                    .is_err()
            );
        }

        remote
            .send_message(&BtMessage::Have { piece_index: 3 })
            .await
            .unwrap();
        let availability_event = {
            let mut events = swarm.lease_event_receiver().unwrap();
            timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap()
        };
        assert!(matches!(
            availability_event,
            PeerEvent::PeerAvailabilityChanged {
                actor_id: event_actor_id,
                piece_index: 3,
                has_piece: true,
            } if event_actor_id == actor_id
        ));
        swarm.apply_event(&availability_event);
        assert_eq!(swarm.actor(actor_id).unwrap().bitfield, [0x10, 0]);
        swarm.shutdown_all().await;
    }
}

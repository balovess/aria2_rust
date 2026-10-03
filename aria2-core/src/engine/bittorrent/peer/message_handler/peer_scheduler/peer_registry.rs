//! Owned peer actors and stable-ID routing for a torrent swarm.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::engine::bittorrent::peer::connection::{BtPeerConn, PeerActorId};
use crate::engine::bittorrent::peer::stats::{PeerStats, SwarmUploadRate};
use crate::engine::bittorrent::peer::upload_session::PieceDataProvider;

use super::super::types::{DEFAULT_MAX_OUTSTANDING_REQUEST, MAX_OUTSTANDING_REQUEST};
use super::{PeerActorControl, PeerActorPayloadConfig, PeerActorTask, PeerCommand, PeerEvent};

const PEER_STATS_SNAPSHOT_MIN_INTERVAL: Duration = Duration::from_millis(250);
const MAX_RECENTLY_DROPPED_PEERS: usize = 50;
const MAX_KNOWN_SEEDER_ENDPOINTS: usize = 1024;

/// One long-lived I/O owner for a handshaken BitTorrent connection.
pub(crate) struct PeerActorEntry {
    pub(crate) actor_id: PeerActorId,
    /// Socket endpoint used for actor identity, deduplication, and cleanup.
    pub(crate) endpoint: SocketAddr,
    /// Remote listen endpoint suitable for RPC and peer exchange, if known.
    pub(crate) advertised_endpoint: Option<SocketAddr>,
    pub(crate) first_contact_time: Instant,
    graceful_disconnected_at: Option<Instant>,
    pub(crate) dead: bool,
    pub(crate) metadata_pending: bool,
    pub(crate) incoming: bool,
    pub(crate) source: crate::request::request_group::BtPeerSource,
    pub(crate) client: Arc<std::sync::RwLock<Option<String>>>,
    pub(crate) pending_download_requests: Arc<AtomicUsize>,
    pub(crate) max_outstanding_requests: usize,
    pub(crate) stats: PeerStats,
    pub(crate) has_bitfield: bool,
    pub(crate) bitfield: Vec<u8>,
    pub(crate) peer_allowed_fast: HashSet<u32>,
    pub(crate) seeder: bool,
    pub(crate) ut_pex_id: Option<u8>,
    actor: PeerActorTask,
}

impl PeerActorEntry {
    pub(crate) fn spawn(
        actor_id: PeerActorId,
        endpoint: SocketAddr,
        connection: BtPeerConn,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        provider: Option<Arc<dyn PieceDataProvider>>,
        event_tx: mpsc::Sender<PeerEvent>,
    ) -> Self {
        let stats = connection.stats.clone();
        let seeder = connection.seeder;
        let incoming = connection.incoming;
        let metadata_pending = connection.is_metadata_pending();
        let advertised_endpoint = connection.advertised_endpoint();
        let first_contact_time = connection.first_contact_time();
        let source = connection.source;
        let client = connection.remote_client.clone();
        let pending_download_requests = Arc::new(AtomicUsize::new(0));
        let ut_pex_id = connection.peer_extension_id("ut_pex");
        let has_bitfield =
            !connection.is_metadata_pending() && connection.session_resource.is_some();
        let bitfield = connection
            .session_resource
            .as_ref()
            .map_or_else(Vec::new, |resource| resource.bitfield().to_vec());
        let peer_allowed_fast = connection.peer_allowed_fast_set().clone();
        let actor = PeerActorTask::spawn_owned(
            actor_id,
            connection,
            event_tx,
            dht_engine,
            provider,
            Arc::clone(&pending_download_requests),
            16,
        );

        Self {
            actor_id,
            endpoint,
            advertised_endpoint,
            first_contact_time,
            graceful_disconnected_at: None,
            dead: false,
            metadata_pending,
            incoming,
            source,
            client,
            pending_download_requests,
            max_outstanding_requests: DEFAULT_MAX_OUTSTANDING_REQUEST,
            stats,
            has_bitfield,
            bitfield,
            peer_allowed_fast,
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
    recently_dropped_endpoints: VecDeque<(SocketAddr, Instant)>,
    known_seeders: HashSet<SocketAddr>,
    known_seeder_order: VecDeque<SocketAddr>,
    wanted_pieces: Arc<[u8]>,
    local_seeder: bool,
    local_metadata: Option<Arc<[u8]>>,
    peer_snapshot_store:
        Option<Arc<std::sync::RwLock<Vec<crate::request::request_group::BtPeerSnapshot>>>>,
    last_stats_snapshot_publish: Option<Instant>,
    stats_snapshot_dirty: bool,
    upload_rate: Arc<SwarmUploadRate>,
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
        loop {
            let receiver = self.receiver.as_mut()?;
            let event = if let Some(deadline) = self.swarm.pending_stats_snapshot_deadline() {
                tokio::select! {
                    event = receiver.recv() => event,
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                        self.swarm.publish_pending_stats_snapshot();
                        continue;
                    }
                }
            } else {
                receiver.recv().await
            };
            return event;
        }
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

    pub(crate) async fn send_to(
        &self,
        actor_id: PeerActorId,
        command: PeerCommand,
    ) -> Result<(), mpsc::error::SendError<PeerCommand>> {
        let Some(actor) = self.swarm.actor(actor_id) else {
            return Err(mpsc::error::SendError(command));
        };
        actor.handle().send(command).await
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

    pub(crate) fn increase_request_window(&mut self, actor_id: PeerActorId) -> Option<usize> {
        let actor = self.swarm.actor_mut(actor_id)?;
        actor.max_outstanding_requests = actor
            .max_outstanding_requests
            .saturating_mul(2)
            .min(MAX_OUTSTANDING_REQUEST);
        Some(actor.max_outstanding_requests)
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
            recently_dropped_endpoints: VecDeque::new(),
            known_seeders: HashSet::new(),
            known_seeder_order: VecDeque::new(),
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

    fn remember_known_seeder(&mut self, endpoint: SocketAddr) {
        if !self.known_seeders.insert(endpoint) {
            return;
        }
        self.known_seeder_order.push_back(endpoint);
        while self.known_seeder_order.len() > MAX_KNOWN_SEEDER_ENDPOINTS {
            if let Some(expired) = self.known_seeder_order.pop_front() {
                self.known_seeders.remove(&expired);
            }
        }
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

    pub(crate) fn attach_peer_snapshot_store(
        &mut self,
        store: Arc<std::sync::RwLock<Vec<crate::request::request_group::BtPeerSnapshot>>>,
    ) {
        self.peer_snapshot_store = Some(store);
        self.publish_peer_snapshots();
    }

    pub(crate) fn peer_snapshots(&self) -> Vec<crate::request::request_group::BtPeerSnapshot> {
        let now = Instant::now();
        self.actors
            .iter()
            .filter(|actor| !actor.dead)
            .map(|actor| crate::request::request_group::BtPeerSnapshot {
                peer_id: actor.stats.peer_id,
                client: actor
                    .client
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
                addr: actor.advertised_endpoint.unwrap_or(actor.endpoint),
                is_incoming: actor.incoming,
                source: actor.source,
                bitfield: actor.has_bitfield.then(|| actor.bitfield.clone()),
                uploaded_bytes: actor.stats.uploaded_bytes,
                downloaded_bytes: actor.stats.downloaded_bytes,
                upload_speed: actor.stats.recent_upload_speed_at(now) as f64,
                download_speed: actor.stats.recent_download_speed_at(now) as f64,
                avg_upload_speed: actor.stats.avg_upload_speed,
                avg_download_speed: actor.stats.avg_download_speed,
                am_choking: actor.stats.am_choking,
                peer_choking: actor.stats.peer_choking,
                am_interested: actor.stats.am_interested,
                peer_interested: actor.stats.peer_interested,
                outstanding_upload_requests: actor.stats.outstanding_upload_count,
                outstanding_download_requests: actor
                    .pending_download_requests
                    .load(std::sync::atomic::Ordering::Relaxed),
                seeder: Some(actor.seeder),
                connection_duration_secs: actor.stats.connection_duration_secs(),
                last_data_age_secs: actor
                    .stats
                    .last_data_time
                    .map_or(actor.stats.age().as_secs(), |time| time.elapsed().as_secs()),
                is_snubbed: actor.stats.is_snubbed,
                is_banned: actor.stats.is_banned,
            })
            .collect()
    }

    fn publish_peer_snapshots(&self) {
        let Some(store) = &self.peer_snapshot_store else {
            return;
        };
        *store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self.peer_snapshots();
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

    /// Apply one consumed I/O event to the registry-owned peer snapshot.
    pub(crate) fn apply_event(&mut self, event: &PeerEvent) {
        if let PeerEvent::UploadBytes {
            bytes, recorded_at, ..
        } = event
        {
            self.upload_rate.record(*bytes, *recorded_at);
        }
        match event {
            PeerEvent::InterestChanged {
                actor_id, snapshot, ..
            }
            | PeerEvent::AmInterestChanged {
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
                if *seeder
                    && let Some((endpoint, advertised_endpoint)) = self
                        .actor(*actor_id)
                        .map(|actor| (actor.endpoint, actor.advertised_endpoint))
                {
                    self.remember_known_seeder(endpoint);
                    if let Some(advertised_endpoint) = advertised_endpoint {
                        self.remember_known_seeder(advertised_endpoint);
                    }
                }
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
            PeerEvent::AllowedFast {
                actor_id,
                piece_index,
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.peer_allowed_fast.insert(*piece_index);
                }
            }
            PeerEvent::ExtensionHandshakeReceived {
                actor_id,
                ut_pex_id,
                remote_listen_port,
                ..
            } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.ut_pex_id = *ut_pex_id;
                    if let Some(port) = remote_listen_port {
                        actor.advertised_endpoint =
                            Some(SocketAddr::new(actor.endpoint.ip(), *port));
                        actor.incoming = false;
                    }
                }
            }
            PeerEvent::Disconnected { actor_id } => {
                self.mark_dead(*actor_id);
            }
            PeerEvent::GracefulDisconnected { actor_id } => {
                if let Some(actor) = self.actor_mut(*actor_id) {
                    actor.graceful_disconnected_at = Some(Instant::now());
                }
                self.mark_dead(*actor_id);
            }
            PeerEvent::RequestFailed { .. } => {}
            PeerEvent::Message { stats: None, .. }
            | PeerEvent::MetadataMessage { .. }
            | PeerEvent::PexPeers { .. }
            | PeerEvent::TrackerPeers { .. } => {}
        }
        let publishes_peer_state = matches!(
            event,
            PeerEvent::InterestChanged { .. }
                | PeerEvent::AmInterestChanged { .. }
                | PeerEvent::ChokeStateChanged { .. }
                | PeerEvent::PeerChokingChanged { .. }
                | PeerEvent::PeerAvailabilityChanged { .. }
                | PeerEvent::PeerAvailabilitySnapshot { .. }
                | PeerEvent::ExtensionHandshakeReceived {
                    remote_listen_port: Some(_),
                    ..
                }
        );
        let publishes_peer_stats = matches!(
            event,
            PeerEvent::UploadBytes { .. }
                | PeerEvent::UploadQueueChanged { .. }
                | PeerEvent::Message { stats: Some(_), .. }
        );
        let now = Instant::now();
        let stats_snapshot_due = publishes_peer_stats
            && self.last_stats_snapshot_publish.is_none_or(|last| {
                now.saturating_duration_since(last) >= PEER_STATS_SNAPSHOT_MIN_INTERVAL
            });
        if publishes_peer_state {
            self.publish_peer_snapshots();
            self.last_stats_snapshot_publish = Some(now);
            self.stats_snapshot_dirty = false;
        }
        if stats_snapshot_due {
            if !publishes_peer_state {
                self.publish_peer_snapshots();
            }
            self.last_stats_snapshot_publish = Some(now);
            self.stats_snapshot_dirty = false;
        } else if publishes_peer_stats {
            self.stats_snapshot_dirty = true;
        }
    }

    fn pending_stats_snapshot_deadline(&self) -> Option<Instant> {
        self.stats_snapshot_dirty.then(|| {
            self.last_stats_snapshot_publish
                .map_or_else(Instant::now, |last| last + PEER_STATS_SNAPSHOT_MIN_INTERVAL)
        })
    }

    fn publish_pending_stats_snapshot(&mut self) {
        let now = Instant::now();
        if self.stats_snapshot_dirty
            && self.last_stats_snapshot_publish.is_none_or(|last| {
                now.saturating_duration_since(last) >= PEER_STATS_SNAPSHOT_MIN_INTERVAL
            })
        {
            self.publish_peer_snapshots();
            self.last_stats_snapshot_publish = Some(now);
            self.stats_snapshot_dirty = false;
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
                    .truncate(MAX_RECENTLY_DROPPED_PEERS);
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

#[cfg(test)]
mod tests {
    use super::super::peer_request::RequestGeneration;
    use super::super::pipelined::BlockRequest;
    use super::*;
    use crate::engine::bittorrent::peer::upload_session::{
        InMemoryPieceProvider, PieceDataProvider,
    };
    use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
    use aria2_protocol::bittorrent::peer::connection::PeerConnection;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn request_failure_keeps_peer_alive_until_transport_disconnect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        assert_eq!(
            swarm.spawn_peer(connection, None, provider).ok(),
            Some(actor_id)
        );

        swarm.apply_event(&PeerEvent::RequestFailed {
            actor_id,
            generation: RequestGeneration::allocate(),
            piece_index: 0,
            request: BlockRequest {
                block_index: 0,
                offset: 0,
                length: 16,
            },
        });
        assert!(!swarm.actor(actor_id).unwrap().dead);

        swarm.apply_event(&PeerEvent::Disconnected { actor_id });
        assert!(swarm.actor(actor_id).unwrap().dead);
        swarm.shutdown_all().await;
        drop(remote_stream);
    }

    #[tokio::test]
    async fn swarm_registry_preserves_ipv6_peer_endpoints() {
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        assert_eq!(
            swarm.spawn_peer(connection, None, provider).ok(),
            Some(actor_id)
        );

        assert_eq!(swarm.actor(actor_id).unwrap().endpoint, endpoint);

        swarm.shutdown_all().await;
        drop(remote_stream);
    }

    #[tokio::test]
    async fn swarm_coordinator_lease_can_admit_a_peer_without_releasing_event_ownership() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let connection = BtPeerConn::from_incoming_tcp(
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
    async fn upload_choke_state_updates_peer_wire_state_and_swarm_snapshot() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer_capabilities(
                local_stream,
                [0; 20],
                false,
                true,
                true,
            ),
            endpoint,
        );
        let actor_id = connection.actor_id;
        connection.configure_upload_with_auto_unchoke(
            &crate::engine::bittorrent::peer::upload_session::BtSeedingConfig::default(),
            crate::rate_limiter::RateLimiter::unlimited(),
            1,
            16,
            false,
        );
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        assert!(swarm.spawn_peer(connection, None, provider).is_ok());

        assert!(swarm.set_upload_choked(actor_id, false));
        let mut remote = PeerConnection::from_stream_with_peer_capabilities(
            remote_stream,
            [1; 20],
            false,
            false,
            true,
        );
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
        let mut connection = BtPeerConn::from_incoming_tcp(
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
        let connection = BtPeerConn::from_incoming_tcp(
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
        let mut connection = BtPeerConn::from_incoming_tcp(
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
        control.begin_generation(generation, 0).unwrap();
        control.try_request(generation, 0, request).unwrap();

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
    async fn peer_snapshots_use_rolling_rates_instead_of_burst_ema() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        connection.stats.upload_speed = 80_000_000.0;
        connection.stats.download_speed = 120_000_000.0;
        let sample_time = Instant::now() - Duration::from_secs(1);
        connection
            .stats
            .record_upload_rate_at(8 * 1024, sample_time);
        connection
            .stats
            .record_download_rate_at(16 * 1024, sample_time);

        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        assert!(swarm.spawn_peer(connection, None, provider).is_ok());

        let snapshot = swarm.peer_snapshots().remove(0);
        assert!(
            (7_000.0..9_000.0).contains(&snapshot.upload_speed),
            "peer snapshot upload speed should use the 10-second byte window, got {}",
            snapshot.upload_speed
        );
        assert!(
            (14_000.0..18_000.0).contains(&snapshot.download_speed),
            "peer snapshot download speed should use the 10-second byte window, got {}",
            snapshot.download_speed
        );

        swarm.shutdown_all().await;
        drop(remote_stream);
    }

    #[tokio::test]
    async fn shutdown_all_closes_a_full_event_queue_before_joining_peer_actors() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        let mut piece_provider = InMemoryPieceProvider::new(16, 1);
        piece_provider.set_piece_data(0, vec![0x5a; 16]);
        let provider: Arc<dyn PieceDataProvider> = Arc::new(piece_provider);
        connection.configure_upload_with_auto_unchoke(
            &crate::engine::bittorrent::peer::upload_session::BtSeedingConfig::default(),
            crate::rate_limiter::RateLimiter::unlimited(),
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
        use aria2_protocol::bittorrent::message::extension::{
            CompactPeerV4, ExtensionHandshake, UtPexMessage,
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer_capabilities(
                local_stream,
                [0; 20],
                false,
                true,
                true,
            ),
            endpoint,
        );
        connection.allocate_session_resource(16, 1, 16);
        connection.set_pex_enabled(true);
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = PeerSwarm::new(8);
        assert!(matches!(
            swarm.spawn_peer(connection, None, provider),
            Ok(registered_actor_id) if registered_actor_id == actor_id
        ));

        let mut remote = PeerConnection::from_stream_with_peer_capabilities(
            remote_stream,
            [1; 20],
            false,
            false,
            true,
        );
        let mut extension_handshake = ExtensionHandshake::new();
        extension_handshake.with_ut_pex(9);
        remote
            .send_message(&BtMessage::Extended {
                ext_id: 0,
                payload: extension_handshake.to_bytes(),
            })
            .await
            .unwrap();
        let negotiation = {
            let mut events = swarm.lease_event_receiver().unwrap();
            timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap()
        };
        assert!(matches!(
            negotiation,
            PeerEvent::ExtensionHandshakeReceived {
                actor_id: event_actor,
                ut_pex_id: Some(9),
                remote_listen_port: None,
                ..
            } if event_actor == actor_id
        ));
        assert_eq!(swarm.actor(actor_id).unwrap().ut_pex_id, Some(9));

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
        let connection = BtPeerConn::from_incoming_tcp(
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
        let mut connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        connection.allocate_session_resource(16, 8, 128);
        let actor_id = connection.actor_id;
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut registry = PeerSwarm::new(8);
        registry.insert(PeerActorEntry::spawn(
            actor_id,
            endpoint,
            connection,
            None,
            Some(provider),
            event_tx,
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

        peer_handle.begin_generation(first_generation, 2).unwrap();
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
        peer_handle.end_generation(first_generation, 2).unwrap();
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
        peer_handle.begin_generation(next_generation, 5).unwrap();
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
        let mut connection = BtPeerConn::from_incoming_tcp(
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

        for (message, expected_seeder) in [
            (BtMessage::Bitfield { data: vec![0xff] }, true),
            (BtMessage::HaveAll, true),
            (BtMessage::HaveNone, false),
        ] {
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
    async fn actor_publishes_seeder_state_when_have_completes_the_bitfield() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        connection.allocate_session_resource(16, 1, 16);
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let peer_snapshots = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mut swarm = PeerSwarm::new(8);
        swarm.attach_peer_snapshot_store(Arc::clone(&peer_snapshots));
        assert!(swarm.spawn_peer(connection, None, provider).is_ok());
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        remote
            .send_message(&BtMessage::Have { piece_index: 0 })
            .await
            .unwrap();

        let events = {
            let mut receiver = swarm.lease_event_receiver().unwrap();
            let mut events = Vec::new();
            loop {
                let event = timeout(Duration::from_secs(1), receiver.recv())
                    .await
                    .expect("peer must report the seeder-state transition")
                    .expect("peer actor event channel must stay open");
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
        assert!(actor.seeder);
        assert_eq!(actor.bitfield, [0x80]);
        {
            let snapshots = peer_snapshots
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(snapshots.len(), 1);
            assert_eq!(snapshots[0].seeder, Some(true));
        }

        swarm.shutdown_all().await;
    }

    #[tokio::test]
    async fn removing_dead_actor_cancels_inflight_request_before_joining() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        connection.allocate_session_resource(1, 16, 16);
        let actor_id = connection.actor_id;
        let (event_tx, _event_rx) = mpsc::channel(8);
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut registry = PeerSwarm::new(8);
        registry.insert(PeerActorEntry::spawn(
            actor_id,
            endpoint,
            connection,
            None,
            Some(provider),
            event_tx,
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

        peer_handle.begin_generation(generation, 3).unwrap();
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
        let connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        let actor_id = connection.actor_id;
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let mut actor = PeerActorTask::spawn_owned(
            actor_id,
            connection,
            event_tx.clone(),
            None,
            None,
            Arc::new(AtomicUsize::new(0)),
            4,
        );
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
        timeout(Duration::from_secs(1), actor.shutdown())
            .await
            .unwrap()
            .unwrap();
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
            let connection = BtPeerConn::from_incoming_tcp(
                PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
                endpoint,
            );
            actor_ids.push(connection.actor_id);
            endpoints.push(endpoint);
            registry.insert(PeerActorEntry::spawn(
                connection.actor_id,
                endpoint,
                connection,
                None,
                Some(Arc::clone(&provider)),
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
        let mut connection = BtPeerConn::from_incoming_tcp(
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
        let mut connection = BtPeerConn::from_incoming_tcp(
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

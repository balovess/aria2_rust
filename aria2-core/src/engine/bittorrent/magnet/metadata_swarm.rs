//! Actor-owned BEP 9 metadata bootstrap, retained for payload downloading.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aria2_protocol::bittorrent::message::extension::UtMetadataMessage;
use aria2_protocol::bittorrent::peer::connection::PeerAddr;

use crate::engine::bittorrent::dht::engine_set::DhtEngineSet;
use crate::engine::bittorrent::magnet::metadata_collector::MetadataCollector;
use crate::engine::bittorrent::magnet::metadata_exchange::{
    METADATA_MAX_SIZE, MetadataExchangeConfig, MetadataExchangeError,
};
use crate::engine::bittorrent::peer::connection::PeerActorId;
use crate::engine::bittorrent::peer::interaction::{BtPeerConnectionOptions, BtPeerInteraction};
use crate::engine::bittorrent::peer::message_handler::{
    PeerCommand, PeerEvent, PeerSwarm, PeerSwarmEventLease,
};
use crate::network::OutboundNetworkPolicy;
use crate::request::request_group::{BtPeerSource, DownloadOptions};

const MAX_METADATA_INFLIGHT: usize = 64;
const MAX_METADATA_INFLIGHT_PER_PEER: usize = 4;

struct MetadataPeer {
    handshake_deadline: Option<Instant>,
    metadata_size: Option<u32>,
    supports_extended_messaging: bool,
    supports_metadata: bool,
    disconnected: bool,
}

struct MetadataRequest {
    actor_id: PeerActorId,
    deadline: Instant,
}

/// Owns the metadata-phase swarm until it is handed to the payload session.
pub(crate) struct MetadataPeerSwarm {
    swarm: PeerSwarm,
    state: MetadataExchangeState,
}

struct MetadataExchangeState {
    peers: HashMap<PeerActorId, MetadataPeer>,
    max_peers: usize,
    connect_timeout: Duration,
    piece_size: u32,
    request_timeout: Duration,
    max_attempts: usize,
    metadata_size: Option<u32>,
    piece_count: u32,
    collector: Option<MetadataCollector>,
    completed_pieces: HashSet<u32>,
    in_flight: HashMap<u32, MetadataRequest>,
    attempts: HashMap<(PeerActorId, u32), usize>,
    last_error: String,
}

impl MetadataPeerSwarm {
    pub(crate) fn new(config: MetadataExchangeConfig) -> Self {
        Self {
            swarm: PeerSwarm::new(128),
            state: MetadataExchangeState {
                peers: HashMap::new(),
                max_peers: config.max_peers_to_try,
                connect_timeout: config.connect_timeout,
                piece_size: config.piece_size,
                request_timeout: config.request_timeout,
                max_attempts: config.max_attempts.max(1),
                metadata_size: None,
                piece_count: 0,
                collector: None,
                completed_pieces: HashSet::new(),
                in_flight: HashMap::new(),
                attempts: HashMap::new(),
                last_error: String::new(),
            },
        }
    }

    pub(crate) fn into_swarm(self) -> PeerSwarm {
        self.swarm
    }

    pub(crate) async fn shutdown(&mut self) {
        self.swarm.shutdown_all().await;
    }

    pub(crate) async fn add_peers(
        &mut self,
        peers: &[(SocketAddr, BtPeerSource)],
        info_hash: &[u8; 20],
        local_peer_id: [u8; 20],
        options: &DownloadOptions,
        dht_engines: &DhtEngineSet,
        outbound_network_policy: Arc<OutboundNetworkPolicy>,
    ) -> std::result::Result<usize, MetadataExchangeError> {
        if self.state.piece_size == 0 {
            return Err(MetadataExchangeError::InvalidPieceSize { size: 0 });
        }
        self.swarm.remove_dead().await;

        let remaining = self.state.max_peers.saturating_sub(self.state.peers.len());
        if remaining == 0 {
            return Ok(0);
        }
        let mut seen = HashSet::new();
        let candidates = peers
            .iter()
            .filter(|(endpoint, _)| seen.insert(*endpoint) && !self.swarm.has_endpoint(*endpoint))
            .take(remaining)
            .map(|(endpoint, source)| (*endpoint, *source))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(0);
        }

        let addresses = candidates
            .iter()
            .map(|(endpoint, _)| PeerAddr::new(&endpoint.ip().to_string(), endpoint.port()))
            .collect::<Vec<_>>();
        let mut connection_options =
            BtPeerConnectionOptions::from_download_options(options, local_peer_id);
        // The previous metadata transport was TCP-only. Do not introduce a
        // transport change during this actor-lifecycle refactor.
        connection_options.enable_utp = false;
        connection_options.connection_timeout = self.state.connect_timeout;
        connection_options.dht_enabled = !dht_engines.is_empty();

        let result = BtPeerInteraction::connect_to_peers(
            &addresses,
            info_hash,
            0,
            0,
            0,
            &connection_options,
            None,
            &outbound_network_policy,
        )
        .await
        .map_err(|error| MetadataExchangeError::IoError(error.to_string()))?;

        let source_by_endpoint = candidates.into_iter().collect::<HashMap<_, _>>();
        let mut accepted_peer_ids = HashSet::new();
        let mut admitted = 0;
        for mut connection in result.connections {
            let Some(endpoint) = connection.remote_endpoint() else {
                continue;
            };
            let Some(peer_id) = connection.peer_id else {
                continue;
            };
            if peer_id == local_peer_id
                || self.swarm.has_peer_id(peer_id)
                || !accepted_peer_ids.insert(peer_id)
            {
                continue;
            }
            if let Some(source) = source_by_endpoint.get(&endpoint) {
                connection.set_source(*source);
            }
            let supports_extended_messaging = connection.remote_supports_extended_messaging();
            let dht_engine = dht_engines.for_peer(endpoint);
            let actor_id = match self.swarm.spawn_metadata_peer(connection, dht_engine) {
                Ok(actor_id) => actor_id,
                Err(_) => continue,
            };
            tracing::debug!(actor_id = actor_id.0, %endpoint, "Admitted peer actor for magnet metadata bootstrap");
            self.state.peers.insert(
                actor_id,
                MetadataPeer {
                    handshake_deadline: supports_extended_messaging
                        .then(|| Instant::now() + self.state.request_timeout),
                    metadata_size: None,
                    supports_extended_messaging,
                    supports_metadata: false,
                    disconnected: false,
                },
            );
            admitted += 1;
        }
        if admitted == 0 && result.failed_count > 0 {
            return Err(MetadataExchangeError::AllPeersFailed {
                attempts: result.failed_count,
                last_error: format!(
                    "all {} BitTorrent peer connection attempts failed",
                    result.failed_count
                ),
            });
        }
        Ok(admitted)
    }

    pub(crate) async fn fetch_metadata(
        &mut self,
    ) -> std::result::Result<Vec<u8>, MetadataExchangeError> {
        if self.state.piece_size == 0 {
            return Err(MetadataExchangeError::InvalidPieceSize { size: 0 });
        }
        if self.state.peers.is_empty() {
            return Err(MetadataExchangeError::NoPeersAvailable);
        }
        let mut events = self.swarm.lease_event_receiver().ok_or_else(|| {
            MetadataExchangeError::IoError("peer event stream is unavailable".into())
        })?;
        let state = &mut self.state;

        loop {
            state.schedule_requests(&events).await;
            if state
                .collector
                .as_ref()
                .is_some_and(MetadataCollector::is_complete)
            {
                return state
                    .collector
                    .take()
                    .and_then(MetadataCollector::into_bytes)
                    .ok_or(MetadataExchangeError::IncompleteMetadata {
                        expected: u64::from(state.metadata_size.unwrap_or_default()),
                        received: 0,
                    });
            }
            if state.is_exhausted() {
                return Err(state.incomplete_error());
            }

            let event = if let Some(deadline) = state.next_deadline() {
                tokio::select! {
                    event = events.recv() => event,
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                        state.expire_deadlines(Instant::now());
                        continue;
                    }
                }
            } else {
                events.recv().await
            };
            let Some(event) = event else {
                return Err(MetadataExchangeError::IoError(
                    "peer event stream closed during metadata exchange".into(),
                ));
            };
            state.apply_event(event);
        }
    }
}

impl MetadataExchangeState {
    async fn schedule_requests(&mut self, events: &PeerSwarmEventLease<'_>) {
        if self.metadata_size.is_none() || self.in_flight.len() >= MAX_METADATA_INFLIGHT {
            return;
        }
        let actor_ids = self.peers.keys().copied().collect::<Vec<_>>();
        for actor_id in actor_ids {
            loop {
                if self.in_flight.len() >= MAX_METADATA_INFLIGHT
                    || self.actor_in_flight_count(actor_id) >= MAX_METADATA_INFLIGHT_PER_PEER
                {
                    break;
                }
                let Some(piece) = self.next_piece_for(actor_id) else {
                    break;
                };
                if events
                    .send_to(actor_id, PeerCommand::RequestMetadata { piece })
                    .await
                    .is_err()
                {
                    self.disconnect_actor(actor_id);
                    break;
                }
                self.in_flight.insert(
                    piece,
                    MetadataRequest {
                        actor_id,
                        deadline: Instant::now() + self.request_timeout,
                    },
                );
            }
        }
    }

    fn actor_in_flight_count(&self, actor_id: PeerActorId) -> usize {
        self.in_flight
            .values()
            .filter(|request| request.actor_id == actor_id)
            .count()
    }

    fn next_piece_for(&self, actor_id: PeerActorId) -> Option<u32> {
        let peer = self.peers.get(&actor_id)?;
        let metadata_size = self.metadata_size?;
        if peer.disconnected || !peer.supports_metadata || peer.metadata_size != Some(metadata_size)
        {
            return None;
        }
        (0..self.piece_count).find(|piece| {
            !self.completed_pieces.contains(piece)
                && !self.in_flight.contains_key(piece)
                && self
                    .attempts
                    .get(&(actor_id, *piece))
                    .copied()
                    .unwrap_or_default()
                    < self.max_attempts
        })
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.peers
            .values()
            .filter_map(|peer| peer.handshake_deadline)
            .chain(self.in_flight.values().map(|request| request.deadline))
            .min()
    }

    fn expire_deadlines(&mut self, now: Instant) {
        for peer in self.peers.values_mut() {
            if peer
                .handshake_deadline
                .is_some_and(|deadline| deadline <= now)
            {
                peer.handshake_deadline = None;
            }
        }
        let expired = self
            .in_flight
            .iter()
            .filter_map(|(piece, request)| {
                (request.deadline <= now).then_some((*piece, request.actor_id))
            })
            .collect::<Vec<_>>();
        for (piece, actor_id) in expired {
            self.in_flight.remove(&piece);
            self.record_failure(actor_id, piece, "ut_metadata request timed out");
        }
    }

    fn apply_event(&mut self, event: PeerEvent) {
        match event {
            PeerEvent::ExtensionHandshakeReceived {
                actor_id,
                ut_metadata_id,
                metadata_size,
                ..
            } => self.accept_extension_handshake(actor_id, ut_metadata_id, metadata_size),
            PeerEvent::MetadataMessage { actor_id, message } => {
                self.accept_metadata_message(actor_id, message)
            }
            PeerEvent::Disconnected { actor_id } | PeerEvent::GracefulDisconnected { actor_id } => {
                self.disconnect_actor(actor_id)
            }
            _ => {}
        }
    }

    fn accept_extension_handshake(
        &mut self,
        actor_id: PeerActorId,
        ut_metadata_id: Option<u8>,
        metadata_size: Option<u32>,
    ) {
        if !self.peers.contains_key(&actor_id) {
            return;
        }
        if let Some(peer) = self.peers.get_mut(&actor_id) {
            peer.handshake_deadline = None;
        }
        let Some(metadata_size) =
            metadata_size.filter(|size| *size > 0 && u64::from(*size) <= METADATA_MAX_SIZE)
        else {
            return;
        };
        if ut_metadata_id.is_none_or(|id| id == 0) {
            return;
        }

        if self.metadata_size.is_none() {
            let Ok(collector) = MetadataCollector::new(u64::from(metadata_size), self.piece_size)
            else {
                return;
            };
            self.piece_count = u64::from(metadata_size).div_ceil(u64::from(self.piece_size)) as u32;
            self.collector = Some(collector);
            self.metadata_size = Some(metadata_size);
        }
        if let Some(peer) = self.peers.get_mut(&actor_id) {
            peer.metadata_size = Some(metadata_size);
            peer.supports_metadata = self.metadata_size == Some(metadata_size);
        }
    }

    fn accept_metadata_message(&mut self, actor_id: PeerActorId, message: UtMetadataMessage) {
        match message {
            UtMetadataMessage::Data {
                piece,
                data,
                total_size,
            } => {
                let Some(request) = self.in_flight.get(&piece) else {
                    return;
                };
                if request.actor_id != actor_id {
                    return;
                }
                self.in_flight.remove(&piece);
                if self.metadata_size == Some(total_size)
                    && self
                        .collector
                        .as_mut()
                        .is_some_and(|collector| collector.add_piece(piece, &data))
                {
                    self.completed_pieces.insert(piece);
                } else {
                    self.record_failure(actor_id, piece, "invalid ut_metadata data piece");
                }
            }
            UtMetadataMessage::Reject { piece } => {
                if self
                    .in_flight
                    .get(&piece)
                    .is_some_and(|request| request.actor_id == actor_id)
                {
                    self.in_flight.remove(&piece);
                    self.record_failure(actor_id, piece, "peer rejected ut_metadata piece");
                }
            }
            UtMetadataMessage::Request { .. } => {}
        }
    }

    fn record_failure(&mut self, actor_id: PeerActorId, piece: u32, reason: &str) {
        *self.attempts.entry((actor_id, piece)).or_default() += 1;
        self.last_error = reason.to_owned();
    }

    fn disconnect_actor(&mut self, actor_id: PeerActorId) {
        if let Some(peer) = self.peers.get_mut(&actor_id) {
            peer.disconnected = true;
            peer.handshake_deadline = None;
        }
        let failed = self
            .in_flight
            .iter()
            .filter_map(|(piece, request)| (request.actor_id == actor_id).then_some(*piece))
            .collect::<Vec<_>>();
        for piece in failed {
            self.in_flight.remove(&piece);
            self.record_failure(
                actor_id,
                piece,
                "peer disconnected during metadata exchange",
            );
        }
    }

    fn is_exhausted(&self) -> bool {
        if self
            .peers
            .values()
            .any(|peer| peer.handshake_deadline.is_some())
            || !self.in_flight.is_empty()
        {
            return false;
        }
        let Some(_) = self.metadata_size else {
            return true;
        };
        (0..self.piece_count)
            .filter(|piece| !self.completed_pieces.contains(piece))
            .all(|piece| {
                self.peers.iter().all(|(actor_id, peer)| {
                    peer.disconnected
                        || !peer.supports_metadata
                        || peer.metadata_size != self.metadata_size
                        || self
                            .attempts
                            .get(&(*actor_id, piece))
                            .copied()
                            .unwrap_or_default()
                            >= self.max_attempts
                })
            })
    }

    fn incomplete_error(&self) -> MetadataExchangeError {
        if let Some(metadata_size) = self.metadata_size {
            MetadataExchangeError::IncompleteMetadata {
                expected: u64::from(metadata_size),
                received: self
                    .completed_pieces
                    .iter()
                    .map(|piece| {
                        u64::from(metadata_size)
                            .saturating_sub(u64::from(*piece) * u64::from(self.piece_size))
                            .min(u64::from(self.piece_size))
                    })
                    .sum(),
            }
        } else {
            MetadataExchangeError::AllPeersFailed {
                attempts: self.peers.len(),
                last_error: if self
                    .peers
                    .values()
                    .all(|peer| !peer.supports_extended_messaging)
                {
                    "unsupported: peer did not advertise BEP 10 extension messaging".into()
                } else if self.last_error.is_empty() {
                    "no peer provided a usable ut_metadata extension handshake".into()
                } else {
                    self.last_error.clone()
                },
            }
        }
    }
}

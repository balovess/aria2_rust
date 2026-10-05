//! Long-lived peer I/O actors and piece-scoped actor command sets.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::trace;

use crate::engine::bittorrent::peer::choking_algorithm::{
    ChokingAlgorithm, IdentityChokeAction, PeerIdentity,
};
use crate::engine::bittorrent::peer::connection::{BtPeerConn, PeerActorId};
use crate::error::Result;

use super::normal::process_pex_during_read;
use super::peer_registry::{PeerSwarm, PeerSwarmEventLease};
use super::peer_request::PeerRequestLedger;
pub(super) use super::peer_request::RequestGeneration;
use super::peer_snapshot::PeerSchedulingSnapshot;
use super::pipelined::BlockRequest;

const METADATA_PIECE_SIZE: usize = 16 * 1024;
const PEER_ACTOR_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

fn local_metadata_response(
    metadata: Option<&[u8]>,
    piece: u32,
) -> aria2_protocol::bittorrent::message::extension::UtMetadataMessage {
    use aria2_protocol::bittorrent::message::extension::UtMetadataMessage;

    metadata
        .and_then(|metadata| {
            let total_size = u32::try_from(metadata.len()).ok()?;
            let start = usize::try_from(piece)
                .ok()?
                .checked_mul(METADATA_PIECE_SIZE)?;
            if start >= metadata.len() {
                return None;
            }
            let end = start
                .saturating_add(METADATA_PIECE_SIZE)
                .min(metadata.len());
            Some(UtMetadataMessage::Data {
                piece,
                total_size,
                data: metadata[start..end].to_vec(),
            })
        })
        .unwrap_or(UtMetadataMessage::Reject { piece })
}

pub(crate) enum PeerCommand {
    RequestMetadata {
        piece: u32,
    },
    ActivatePayload(Arc<PeerActorPayloadConfig>),
    Request {
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    },
    Cancel {
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    },
    HavePiece {
        piece_index: u32,
    },
    SendPex(Vec<u8>),
    AnnounceAvailability,
    Shutdown,
}

pub(crate) struct PeerActorPayloadConfig {
    pub(crate) network_info_hash: [u8; 20],
    pub(crate) local_metadata: Arc<[u8]>,
    pub(crate) piece_length: u32,
    pub(crate) num_pieces: u32,
    pub(crate) total_length: u64,
    pub(crate) upload_config: crate::engine::bittorrent::peer::upload_session::BtSeedingConfig,
    pub(crate) upload_limiter: crate::rate_limiter::RateLimiter,
    pub(crate) auto_unchoke: bool,
    pub(crate) upload_counter: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) upload_progress: Arc<crate::request::request_group::AtomicProgress>,
    pub(crate) provider:
        Arc<dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider>,
}

pub(crate) enum PeerEvent {
    Message {
        actor_id: PeerActorId,
        generation: RequestGeneration,
        message: aria2_protocol::bittorrent::message::types::BtMessage,
        stats: Option<Box<crate::engine::bittorrent::peer::stats::PeerStats>>,
    },
    InterestChanged {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::bittorrent::peer::stats::PeerStats>,
    },
    AmInterestChanged {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::bittorrent::peer::stats::PeerStats>,
    },
    ChokeStateChanged {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::bittorrent::peer::stats::PeerStats>,
    },
    PeerChokingChanged {
        actor_id: PeerActorId,
        peer_choking: bool,
    },
    AllowedFast {
        actor_id: PeerActorId,
        piece_index: u32,
    },
    PeerAvailabilityChanged {
        actor_id: PeerActorId,
        piece_index: u32,
        has_piece: bool,
    },
    PeerAvailabilitySnapshot {
        actor_id: PeerActorId,
        bitfield: Vec<u8>,
        seeder: bool,
    },
    OutstandingDownloadRequests {
        actor_id: PeerActorId,
        count: usize,
    },
    ExtensionHandshakeReceived {
        actor_id: PeerActorId,
        ut_pex_id: Option<u8>,
        ut_metadata_id: Option<u8>,
        metadata_size: Option<u32>,
        remote_listen_port: Option<u16>,
    },
    MetadataMessage {
        actor_id: PeerActorId,
        message: aria2_protocol::bittorrent::message::extension::UtMetadataMessage,
    },
    PexPeers {
        peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    },
    TrackerPeers {
        peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    },
    UploadBytes {
        actor_id: PeerActorId,
        bytes: u64,
        recorded_at: Instant,
        snapshot: Box<crate::engine::bittorrent::peer::stats::PeerStats>,
    },
    UploadQueueChanged {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::bittorrent::peer::stats::PeerStats>,
    },
    RequestFailed {
        actor_id: PeerActorId,
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    },
    Disconnected {
        actor_id: PeerActorId,
    },
    GracefulDisconnected {
        actor_id: PeerActorId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PeerGenerationUpdate {
    Begin {
        generation: RequestGeneration,
        piece_index: u32,
    },
    End {
        generation: RequestGeneration,
        piece_index: u32,
    },
}

#[derive(Clone)]
struct PeerActorDesiredState {
    wanted_pieces: Arc<[u8]>,
    choke_upload: bool,
    local_seeder: bool,
}

pub(crate) struct PeerActorCommandReceiver {
    commands: mpsc::Receiver<PeerCommand>,
    generation_updates: watch::Receiver<HashMap<u32, RequestGeneration>>,
    desired_state_updates: watch::Receiver<PeerActorDesiredState>,
    capacity_updates: watch::Sender<u64>,
}

fn notify_queue_capacity(capacity_updates: &watch::Sender<u64>) {
    capacity_updates.send_modify(|revision| *revision = revision.wrapping_add(1));
}

/// Bounded peer commands plus coalesced snapshots for actor-owned state.
#[derive(Clone)]
pub(crate) struct PeerActorControl {
    commands: mpsc::Sender<PeerCommand>,
    generation_updates: watch::Sender<HashMap<u32, RequestGeneration>>,
    desired_state: watch::Sender<PeerActorDesiredState>,
    capacity_updates: watch::Sender<u64>,
}

impl PeerActorControl {
    #[cfg(test)]
    pub(crate) fn channel(capacity: usize) -> (Self, PeerActorCommandReceiver) {
        Self::channel_with_initial_choke(capacity, true)
    }

    fn channel_with_initial_choke(
        capacity: usize,
        choke_upload: bool,
    ) -> (Self, PeerActorCommandReceiver) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        let (generation_updates, generation_update_receiver) = watch::channel(HashMap::new());
        let (desired_state, desired_state_updates) = watch::channel(PeerActorDesiredState {
            wanted_pieces: Arc::from([]),
            choke_upload,
            local_seeder: false,
        });
        let (capacity_updates, _) = watch::channel(0);
        (
            Self {
                commands: sender,
                generation_updates,
                desired_state,
                capacity_updates: capacity_updates.clone(),
            },
            PeerActorCommandReceiver {
                commands: receiver,
                generation_updates: generation_update_receiver,
                desired_state_updates,
                capacity_updates,
            },
        )
    }

    pub(super) fn queue_capacity_updates(&self) -> watch::Receiver<u64> {
        self.capacity_updates.subscribe()
    }

    pub(crate) async fn send(
        &self,
        command: PeerCommand,
    ) -> std::result::Result<(), mpsc::error::SendError<PeerCommand>> {
        self.commands.send(command).await
    }

    pub(crate) fn try_send(
        &self,
        command: PeerCommand,
    ) -> std::result::Result<(), mpsc::error::TrySendError<PeerCommand>> {
        self.commands.try_send(command)
    }

    pub(crate) fn begin_generation(
        &self,
        generation: RequestGeneration,
        piece_index: u32,
    ) -> std::result::Result<(), PeerGenerationUpdate> {
        self.update_generation(PeerGenerationUpdate::Begin {
            generation,
            piece_index,
        })
    }

    pub(crate) fn try_request(
        &self,
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    ) -> std::result::Result<(), mpsc::error::TrySendError<PeerCommand>> {
        self.try_send(PeerCommand::Request {
            generation,
            piece_index,
            request,
        })
    }

    pub(crate) fn try_cancel(
        &self,
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    ) -> std::result::Result<(), mpsc::error::TrySendError<PeerCommand>> {
        self.try_send(PeerCommand::Cancel {
            generation,
            piece_index,
            request,
        })
    }

    pub(crate) fn end_generation(
        &self,
        generation: RequestGeneration,
        piece_index: u32,
    ) -> std::result::Result<(), PeerGenerationUpdate> {
        self.update_generation(PeerGenerationUpdate::End {
            generation,
            piece_index,
        })
    }

    fn update_generation(
        &self,
        update: PeerGenerationUpdate,
    ) -> std::result::Result<(), PeerGenerationUpdate> {
        if self.generation_updates.receiver_count() == 0 {
            return Err(update);
        }
        self.generation_updates
            .send_if_modified(|active| match update {
                PeerGenerationUpdate::Begin {
                    generation,
                    piece_index,
                } => {
                    if active
                        .get(&piece_index)
                        .is_some_and(|current| current.is_at_least(generation))
                    {
                        return false;
                    }
                    active.insert(piece_index, generation);
                    true
                }
                PeerGenerationUpdate::End {
                    generation,
                    piece_index,
                } => {
                    if active.get(&piece_index) != Some(&generation) {
                        return false;
                    }
                    active.remove(&piece_index);
                    true
                }
            });
        Ok(())
    }

    pub(crate) fn set_wanted_pieces(
        &self,
        wanted_pieces: Arc<[u8]>,
    ) -> std::result::Result<(), watch::error::SendError<Arc<[u8]>>> {
        if self.desired_state.receiver_count() == 0 {
            return Err(watch::error::SendError(wanted_pieces));
        }
        self.desired_state.send_if_modified(|state| {
            if state.wanted_pieces.as_ref() == wanted_pieces.as_ref() {
                return false;
            }
            state.wanted_pieces = wanted_pieces;
            true
        });
        Ok(())
    }

    pub(crate) fn set_upload_choked(&self, choked: bool) -> bool {
        if self.desired_state.receiver_count() == 0 {
            return false;
        }
        self.desired_state.send_if_modified(|state| {
            if state.choke_upload == choked {
                return false;
            }
            state.choke_upload = choked;
            true
        });
        true
    }

    pub(crate) fn desired_upload_choked(&self) -> bool {
        self.desired_state.borrow().choke_upload
    }

    pub(crate) fn set_local_seeder(&self, local_seeder: bool) -> bool {
        if self.desired_state.receiver_count() == 0 {
            return false;
        }
        self.desired_state.send_if_modified(|state| {
            if state.local_seeder == local_seeder {
                return false;
            }
            state.local_seeder = local_seeder;
            true
        });
        true
    }
}

/// Tokio-owned peer actor used when the connection lifetime outlives one
/// piece-transfer future.
pub(crate) struct PeerActorTask {
    pub(crate) control: PeerActorControl,
    task: Option<JoinHandle<()>>,
}

impl PeerActorTask {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_owned(
        actor_id: PeerActorId,
        mut connection: BtPeerConn,
        event_tx: mpsc::Sender<PeerEvent>,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        upload_provider: Option<
            Arc<dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider>,
        >,
        pending_download_requests: Arc<AtomicUsize>,
        command_capacity: usize,
    ) -> Self {
        let initial_choke = connection
            .upload_state
            .as_ref()
            .is_none_or(|state| state.is_peer_choked());
        let (control, command_rx) =
            PeerActorControl::channel_with_initial_choke(command_capacity, initial_choke);
        let task = tokio::spawn(async move {
            run_peer_actor(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                dht_engine,
                upload_provider,
                pending_download_requests,
            )
            .await;
        });
        Self {
            control,
            task: Some(task),
        }
    }

    pub(crate) async fn shutdown(&mut self) -> std::result::Result<(), tokio::task::JoinError> {
        let _ = self.control.try_send(PeerCommand::Shutdown);
        let Some(task) = self.task.as_mut() else {
            return Ok(());
        };
        let joined = match tokio::time::timeout(PEER_ACTOR_SHUTDOWN_GRACE, &mut *task).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    "BitTorrent peer actor exceeded its shutdown grace; aborting stalled socket I/O"
                );
                task.abort();
                task.await
            }
        };
        self.task.take();
        match joined {
            Ok(()) => Ok(()),
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for PeerActorTask {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Piece-scoped scheduling handles for peer actors owned by the torrent swarm.
pub(super) struct PeerGeneration {
    senders: HashMap<PeerActorId, PeerActorControl>,
    generation: RequestGeneration,
    active_pieces: HashSet<u32>,
    availability_changed_actor_ids: HashSet<PeerActorId>,
    pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
}

pub(super) enum TryRequestError {
    Full(watch::Receiver<u64>),
    Closed,
}

impl PeerGeneration {
    #[cfg(test)]
    pub(super) fn for_test(
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
    pub(super) fn from_swarm(swarm: &PeerSwarm, piece_indices: &[u32]) -> Self {
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

    pub(super) fn record_availability_change(&mut self, actor_id: PeerActorId) {
        self.availability_changed_actor_ids.insert(actor_id);
    }

    pub(super) fn take_availability_changes(&mut self) -> HashSet<PeerActorId> {
        std::mem::take(&mut self.availability_changed_actor_ids)
    }

    pub(super) fn record_pex_peers(
        &mut self,
        peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    ) {
        self.pex_peers.extend(peers);
    }

    pub(super) fn take_pex_peers(
        &mut self,
    ) -> Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> {
        std::mem::take(&mut self.pex_peers)
    }

    pub(super) fn generation(&self) -> RequestGeneration {
        self.generation
    }

    pub(super) fn has_peer(&self, actor_id: PeerActorId) -> bool {
        self.senders.contains_key(&actor_id)
    }

    pub(super) fn try_request(
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
    pub(super) fn advance_generations(&mut self) {
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

    pub(super) async fn finish_piece_generation(&mut self, piece_index: u32) {
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
    pub(super) fn cancel_peer_requests(
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

    pub(super) fn apply_choke_action(&self, actor_id: PeerActorId, choke: bool) -> bool {
        let Some(sender) = self.senders.get(&actor_id) else {
            return false;
        };
        sender.set_upload_choked(choke)
    }

    /// End this piece generation without stopping torrent-owned peer actors.
    pub(super) async fn finish_generation(&mut self, event_rx: &mut PeerSwarmEventLease<'_>) {
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

pub(crate) async fn run_peer_actor(
    actor_id: PeerActorId,
    connection: &mut BtPeerConn,
    command_rx: PeerActorCommandReceiver,
    event_tx: mpsc::Sender<PeerEvent>,
    dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    mut upload_provider: Option<
        Arc<dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider>,
    >,
    pending_download_requests: Arc<AtomicUsize>,
) -> PeerActorId {
    let PeerActorCommandReceiver {
        mut commands,
        mut generation_updates,
        mut desired_state_updates,
        capacity_updates,
    } = command_rx;
    desired_state_updates.mark_changed();
    let mut availability_sent = false;
    let mut wanted_pieces: Arc<[u8]> = Arc::from([]);
    let mut extension_handshake_info = None;
    if let Some(startup) = connection.actor_startup.take() {
        extension_handshake_info = Some((startup.peer_agent.clone(), startup.listen_port));
        let startup_result = async {
            if connection.remote_supports_extended_messaging() {
                let metadata_size = connection
                    .local_metadata
                    .as_ref()
                    .and_then(|metadata| u32::try_from(metadata.len()).ok())
                    .filter(|size| *size > 0);
                connection
                    .send_extension_handshake_with_metadata(
                        &startup.peer_agent,
                        startup.listen_port,
                        metadata_size,
                    )
                    .await?;
            }
            if let Some(provider) = upload_provider.as_deref() {
                connection.announce_upload_availability(provider).await?;
                availability_sent = true;
            }
            if connection.remote_supports_dht()
                && let Some(engine) = dht_engine.as_ref()
            {
                connection.send_port(engine.local_addr().port()).await?;
            }
            if connection.remote_supports_fast_extension() {
                for piece_index in startup.allowed_fast {
                    connection
                        .send_bt_message(
                            &aria2_protocol::bittorrent::message::types::BtMessage::AllowedFast {
                                index: piece_index,
                            },
                        )
                        .await?;
                    connection.add_am_allowed_fast(piece_index);
                }
            }
            Result::<()>::Ok(())
        }
        .await;
        if let Err(error) = startup_result {
            tracing::debug!(actor_id = actor_id.0, %error, "BT peer actor startup failed");
            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
            return actor_id;
        }
    }
    let mut requests = PeerRequestLedger::default();
    let mut active_generations = HashMap::<u32, RequestGeneration>::new();
    let mut upload_flush_deadline: Option<tokio::time::Instant> = None;
    let mut upload_rate_changes = connection.upload_rate_change_receivers();
    let mut shutdown_requested = false;
    let mut local_seeder = false;
    let local_ut_metadata_id =
        aria2_protocol::bittorrent::message::extension::ExtensionHandshake::new()
            .ut_metadata_id()
            .unwrap_or(1);
    loop {
        pending_download_requests.store(requests.len(), Ordering::Relaxed);
        if shutdown_requested && !connection.has_pending_upload_messages() {
            break;
        }
        if connection.has_pending_upload_messages() {
            let delay = connection
                .pending_upload_flush_delay()
                .unwrap_or(Duration::ZERO);
            let delay = if delay.is_zero() {
                Duration::from_millis(2)
            } else {
                delay
            };
            upload_flush_deadline.get_or_insert_with(|| tokio::time::Instant::now() + delay);
        } else {
            upload_flush_deadline = None;
        }
        let keepalive_deadline = tokio::time::Instant::from_std(connection.keepalive_deadline());
        let peer_timeout_deadline =
            tokio::time::Instant::from_std(connection.peer_timeout_deadline());
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(peer_timeout_deadline) => {
                tracing::debug!(actor_id = actor_id.0, "BT peer inactivity timeout elapsed");
                let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                break;
            }
            _ = tokio::time::sleep_until(keepalive_deadline) => {
                if let Err(error) = connection.send_keepalive().await {
                    tracing::debug!(actor_id = actor_id.0, %error, "Failed to send BT peer keep-alive");
                    let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                    break;
                }
            }
            generation_update = generation_updates.changed() => {
                if generation_update.is_err() {
                    break;
                }
                let previous_request_count = requests.len();
                let latest_generations = {
                    let active = generation_updates.borrow_and_update();
                    active.clone()
                };
                let ended_generations = active_generations
                    .iter()
                    .filter_map(|(&piece_index, &generation)| {
                        (!latest_generations.contains_key(&piece_index))
                            .then_some((piece_index, generation))
                    })
                    .collect::<Vec<_>>();
                for (piece_index, generation) in ended_generations {
                    for (request_piece, request) in requests.drain_piece_generation(piece_index, generation) {
                        if connection
                            .send_cancel(&request.message(request_piece))
                            .await
                            .is_ok()
                        {
                            connection.record_outbound_activity();
                        }
                    }
                    active_generations.remove(&piece_index);
                }
                for (&piece_index, &generation) in &latest_generations {
                    if active_generations
                        .get(&piece_index)
                        .is_some_and(|active| active.is_at_least(generation))
                    {
                        continue;
                    }
                    for (request_piece, request) in requests.drain_piece_before_generation(piece_index, generation) {
                        if connection
                            .send_cancel(&request.message(request_piece))
                            .await
                            .is_ok()
                        {
                            connection.record_outbound_activity();
                        }
                    }
                    active_generations.insert(piece_index, generation);
                }
                pending_download_requests.store(requests.len(), Ordering::Relaxed);
                if requests.len() != previous_request_count
                    && event_tx
                        .send(PeerEvent::OutstandingDownloadRequests {
                            actor_id,
                            count: requests.len(),
                        })
                        .await
                        .is_err()
                {
                    break;
                }
            }
            desired_state_update = desired_state_updates.changed() => {
                if desired_state_update.is_err() {
                    break;
                }
                let desired_state = {
                    let latest_desired_state = desired_state_updates.borrow_and_update();
                    latest_desired_state.clone()
                };
                wanted_pieces = Arc::clone(&desired_state.wanted_pieces);
                local_seeder = desired_state.local_seeder;
                match reconcile_peer_interest(
                    actor_id,
                    connection,
                    &wanted_pieces,
                    &event_tx,
                ).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => {
                        tracing::debug!(actor_id = actor_id.0, %error, "Failed to update BT peer interest");
                        let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                        break;
                    }
                }
                match reconcile_peer_choke_state(
                    actor_id,
                    connection,
                    desired_state.choke_upload,
                    &event_tx,
                ).await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => {
                        tracing::debug!(actor_id = actor_id.0, %error, "Failed to update BT peer choke state");
                        let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                        break;
                    }
                }
                if local_seeder && connection.is_seeder() {
                    tracing::debug!(actor_id = actor_id.0, "Closing BT connection between seeders");
                    let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                    break;
                }
            }
            command = commands.recv(), if !shutdown_requested => {
                if command.is_some() {
                    notify_queue_capacity(&capacity_updates);
                }
                match command {
                    Some(PeerCommand::RequestMetadata { piece }) => {
                        if !connection.is_metadata_pending() {
                            continue;
                        }
                        let Some(ext_id) = connection.peer_extension_id("ut_metadata") else {
                            continue;
                        };
                        let message = aria2_protocol::bittorrent::message::types::BtMessage::Extended {
                            ext_id,
                            payload: aria2_protocol::bittorrent::message::extension::UtMetadataMessage::Request { piece }
                                .to_payload(),
                        };
                        if let Err(error) = connection.send_bt_message(&message).await {
                            tracing::debug!(actor_id = actor_id.0, %error, piece, "Failed to request BEP 9 metadata piece");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        connection.record_outbound_activity();
                    }
                    Some(PeerCommand::ActivatePayload(config)) => {
                        if !connection.activate_payload_session(
                            config.piece_length,
                            config.num_pieces,
                            config.total_length,
                        ) {
                            tracing::debug!(actor_id = actor_id.0, "Dropping metadata peer after availability state exceeded its bounded buffer");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        connection.local_metadata = Some(Arc::clone(&config.local_metadata));
                        if connection.remote_supports_extended_messaging()
                            && let Some((peer_agent, listen_port)) =
                                extension_handshake_info.as_ref()
                        {
                            let metadata_size = u32::try_from(config.local_metadata.len())
                                .ok()
                                .filter(|size| *size > 0);
                            if let Err(error) = connection
                                .send_extension_handshake_with_metadata(
                                    peer_agent,
                                    *listen_port,
                                    metadata_size,
                                )
                                .await
                            {
                                tracing::debug!(actor_id = actor_id.0, %error, "Failed to advertise local BEP 9 metadata after magnet activation");
                                let _ = event_tx
                                    .send(PeerEvent::Disconnected { actor_id })
                                    .await;
                                break;
                            }
                            connection.record_outbound_activity();
                        }
                        let allowed_fast = connection
                            .peer_allowed_fast_set()
                            .iter()
                            .copied()
                            .collect::<Vec<_>>();
                        let mut event_stream_closed = false;
                        for piece_index in allowed_fast {
                            if event_tx
                                .send(PeerEvent::AllowedFast {
                                    actor_id,
                                    piece_index,
                                })
                                .await
                                .is_err()
                            {
                                event_stream_closed = true;
                                break;
                            }
                        }
                        if event_stream_closed {
                            break;
                        }
                        connection.configure_upload_with_auto_unchoke(
                            &config.upload_config,
                            config.upload_limiter.clone(),
                            config.num_pieces,
                            config.piece_length,
                            config.auto_unchoke,
                        );
                        connection.set_upload_counter(Arc::clone(&config.upload_counter));
                        connection.set_upload_progress(Arc::clone(&config.upload_progress));
                        let allowed_fast = if connection.remote_supports_fast_extension() {
                            aria2_protocol::bittorrent::fast_set::compute_fast_set(
                                connection.remote_ip(),
                                config.num_pieces,
                                &config.network_info_hash,
                                10,
                            )
                        } else {
                            Vec::new()
                        };
                        let mut activation_failed = false;
                        for piece_index in allowed_fast {
                            if let Err(error) = connection
                                .send_bt_message(
                                    &aria2_protocol::bittorrent::message::types::BtMessage::AllowedFast {
                                        index: piece_index,
                                    },
                                )
                                .await
                            {
                                tracing::debug!(actor_id = actor_id.0, %error, "Failed to send post-metadata AllowedFast");
                                let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                                activation_failed = true;
                                break;
                            }
                            connection.add_am_allowed_fast(piece_index);
                        }
                        if activation_failed {
                            break;
                        }
                        upload_provider = Some(Arc::clone(&config.provider));
                        if let Err(error) = connection
                            .announce_upload_availability(config.provider.as_ref())
                            .await
                        {
                            tracing::debug!(actor_id = actor_id.0, %error, "Failed to announce availability after metadata resolution");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        availability_sent = true;
                        if let Some(resource) = connection.session_resource.as_ref()
                            && event_tx
                                .send(PeerEvent::PeerAvailabilitySnapshot {
                                    actor_id,
                                    bitfield: resource.bitfield().to_vec(),
                                    seeder: connection.seeder,
                                })
                                .await
                                .is_err()
                        {
                            break;
                        }
                        connection.record_outbound_activity();
                    }
                    Some(PeerCommand::Request { generation, piece_index, request }) => {
                        if active_generations.get(&piece_index) != Some(&generation)
                            || requests.contains(piece_index, request)
                        {
                            let _ = event_tx.send(PeerEvent::RequestFailed {
                                actor_id,
                                generation,
                                piece_index,
                                request,
                            }).await;
                            continue;
                        }
                        match connection.send_request(request.message(piece_index)).await {
                            Ok(()) => {
                                requests.record(generation, piece_index, request);
                                pending_download_requests.store(requests.len(), Ordering::Relaxed);
                                if event_tx
                                    .send(PeerEvent::OutstandingDownloadRequests {
                                        actor_id,
                                        count: requests.len(),
                                    })
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                                connection.record_outbound_activity();
                            }
                            Err(error) => {
                                requests.cancel(generation, piece_index, request);
                                tracing::debug!(actor_id = actor_id.0, %error, "Failed to send BT block request");
                                let _ = event_tx
                                    .send(PeerEvent::Disconnected { actor_id })
                                    .await;
                                break;
                            }
                        }
                    }
                    Some(PeerCommand::Cancel { generation, piece_index, request }) => {
                        if active_generations.get(&piece_index) == Some(&generation)
                            && requests.cancel(generation, piece_index, request)
                        {
                            pending_download_requests.store(requests.len(), Ordering::Relaxed);
                            if event_tx
                                .send(PeerEvent::OutstandingDownloadRequests {
                                    actor_id,
                                    count: requests.len(),
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if connection.send_cancel(&request.message(piece_index)).await.is_ok() {
                                connection.record_outbound_activity();
                            }
                        }
                    }
                    Some(PeerCommand::HavePiece { piece_index }) => {
                        if let Err(error) = connection.send_have(piece_index).await {
                            tracing::debug!(actor_id = actor_id.0, %error, "Failed to announce validated BT piece");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        connection.record_outbound_activity();
                    }
                    Some(PeerCommand::SendPex(wire_bytes)) => {
                        connection.queue_message(wire_bytes);
                        if let Err(error) = connection.flush_send_buffer().await {
                            tracing::debug!(actor_id = actor_id.0, %error, "Failed to send BT PEX message");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        connection.record_outbound_activity();
                    }
                    Some(PeerCommand::AnnounceAvailability) => {
                        if availability_sent {
                            continue;
                        }
                        let Some(provider) = upload_provider.as_deref() else {
                            continue;
                        };
                        if let Err(error) = connection.announce_upload_availability(provider).await {
                            tracing::debug!(actor_id = actor_id.0, %error, "Failed to announce BT peer availability");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        availability_sent = true;
                    }
                    Some(PeerCommand::Shutdown) | None => {
                        shutdown_requested = true;
                        for (piece_index, request) in requests.drain_all() {
                            if connection
                                .send_cancel(&request.message(piece_index))
                                .await
                                .is_ok()
                            {
                                connection.record_outbound_activity();
                            }
                        }
                        connection.discard_pending_upload_messages();
                        if !connection.has_pending_upload_messages() {
                            break;
                        }
                    }
                }
            }
            message = connection.read_message() => {
                match message {
                    Ok(Some(message)) => {
                        use aria2_protocol::bittorrent::message::types::BtMessage;
                        let remote_extension_handshake = match &message {
                            BtMessage::Extended {
                                ext_id: 0,
                                payload,
                            } => aria2_protocol::bittorrent::message::extension::
                                ExtensionHandshake::from_bytes(payload)
                                .ok(),
                            _ => None,
                        };
                        let metadata_message = match &message {
                            BtMessage::Extended { ext_id, payload }
                                if *ext_id == local_ut_metadata_id =>
                            {
                                match aria2_protocol::bittorrent::message::extension::
                                    UtMetadataMessage::from_payload(payload)
                                {
                                    Ok(parsed) => Some(parsed),
                                    Err(error) => {
                                        tracing::debug!(actor_id = actor_id.0, %error, "Invalid BEP 9 metadata message");
                                        let _ = event_tx
                                            .send(PeerEvent::Disconnected { actor_id })
                                            .await;
                                        break;
                                    }
                                }
                            }
                            _ => None,
                        };
                        let was_interested = connection.stats.peer_interested;
                        let was_peer_choking = connection.stats.peer_choking;
                        let was_seeder = connection.seeder;
                        let outstanding_upload_count = connection.stats.outstanding_upload_count;
                        let peer_availability_change = match &message {
                            aria2_protocol::bittorrent::message::types::BtMessage::Have {
                                piece_index,
                            } if !connection.is_metadata_pending() => Some(*piece_index),
                            _ => None,
                        };
                        let full_availability_change = !connection.is_metadata_pending()
                            && matches!(
                                message,
                                aria2_protocol::bittorrent::message::types::BtMessage::Bitfield { .. }
                                    | aria2_protocol::bittorrent::message::types::BtMessage::HaveAll
                                    | aria2_protocol::bittorrent::message::types::BtMessage::HaveNone
                            );
                        let received_extension_handshake = matches!(
                            &message,
                            aria2_protocol::bittorrent::message::types::BtMessage::Extended {
                                ext_id: 0,
                                ..
                            }
                        );
                        let allowed_fast_piece = match &message {
                            aria2_protocol::bittorrent::message::types::BtMessage::AllowedFast {
                                index,
                            } if !connection.is_metadata_pending() => Some(*index),
                            _ => None,
                        };
                        let (message, uploaded_bytes) = match process_peer_message(
                            connection,
                            message,
                            dht_engine.clone(),
                            upload_provider.as_deref(),
                        ).await {
                            Ok(message) => message,
                            Err(error) => {
                                tracing::debug!(actor_id = actor_id.0, %error, "BT upload request handling failed");
                                let _ = event_tx
                                    .send(PeerEvent::Disconnected { actor_id })
                                    .await;
                                break;
                            }
                        };
                        if local_seeder && connection.is_seeder() {
                            if let Some(resource) = connection.session_resource.as_ref()
                                && event_tx
                                    .send(PeerEvent::PeerAvailabilitySnapshot {
                                        actor_id,
                                        bitfield: resource.bitfield().to_vec(),
                                        seeder: true,
                                    })
                                    .await
                                    .is_err()
                            {
                                break;
                            }
                            tracing::debug!(actor_id = actor_id.0, "Closing BT connection between seeders");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        if let Some(piece_index) = allowed_fast_piece
                            && event_tx
                                .send(PeerEvent::AllowedFast {
                                    actor_id,
                                    piece_index,
                                })
                                .await
                                .is_err()
                        {
                            break;
                        }
                        if received_extension_handshake
                            && event_tx
                                .send(PeerEvent::ExtensionHandshakeReceived {
                                    actor_id,
                                    ut_pex_id: connection.peer_extension_id("ut_pex"),
                                    // The connection owns the effective BEP 10 map: omitted
                                    // entries retain their previous ID, while an explicit zero
                                    // removes the mapping. Publish that resolved state so swarm
                                    // coordinators can observe capability revocations.
                                    ut_metadata_id: connection.peer_extension_id("ut_metadata"),
                                    metadata_size: remote_extension_handshake
                                        .as_ref()
                                        .and_then(|handshake| handshake.metadata_size())
                                        .filter(|size| *size > 0),
                                    remote_listen_port: connection.remote_listen_port,
                                })
                                .await
                                .is_err()
                        {
                            break;
                        }
                        if let Some(metadata_message) = metadata_message {
                            use aria2_protocol::bittorrent::message::extension::UtMetadataMessage;
                            match metadata_message {
                                UtMetadataMessage::Request { piece } => {
                                    if let Some(ext_id) = connection.peer_extension_id("ut_metadata") {
                                        let response = local_metadata_response(
                                            connection.local_metadata.as_deref(),
                                            piece,
                                        );
                                        let message = BtMessage::Extended {
                                            ext_id,
                                            payload: response.to_payload(),
                                        };
                                        if connection.send_bt_message(&message).await.is_err() {
                                            let _ = event_tx
                                                .send(PeerEvent::Disconnected { actor_id })
                                                .await;
                                            break;
                                        }
                                        connection.record_outbound_activity();
                                    }
                                }
                                metadata_message => {
                                    if connection.is_metadata_pending()
                                        && event_tx
                                            .send(PeerEvent::MetadataMessage {
                                                actor_id,
                                                message: metadata_message,
                                            })
                                            .await
                                            .is_err()
                                    {
                                        break;
                                    }
                                }
                            }
                        }
                        let discovered_pex_peers = connection.drain_pex_peers();
                        if !discovered_pex_peers.is_empty()
                            && event_tx
                                .send(PeerEvent::PexPeers {
                                    peers: discovered_pex_peers,
                                })
                                .await
                                .is_err()
                        {
                            break;
                        }
                        if uploaded_bytes > 0 {
                            connection.record_outbound_activity();
                            let Ok(permit) = event_tx.reserve().await else {
                                break;
                            };
                            let recorded_at = Instant::now();
                            permit.send(PeerEvent::UploadBytes {
                                actor_id,
                                bytes: uploaded_bytes,
                                recorded_at,
                                snapshot: Box::new(connection.stats.clone()),
                            });
                        } else if connection.stats.outstanding_upload_count != outstanding_upload_count
                            && event_tx.send(PeerEvent::UploadQueueChanged {
                                actor_id,
                                snapshot: Box::new(connection.stats.clone()),
                            }).await.is_err()
                        {
                            tracing::debug!(actor_id = actor_id.0, "Peer swarm event receiver closed while publishing queued upload state");
                            break;
                        }
                        if connection.stats.outstanding_upload_count != outstanding_upload_count
                            && connection.has_pending_upload_messages()
                        {
                            let delay = connection
                                .pending_upload_flush_delay()
                                .unwrap_or(Duration::ZERO);
                            let delay = if delay.is_zero() {
                                Duration::from_millis(2)
                            } else {
                                delay
                            };
                            upload_flush_deadline =
                                Some(tokio::time::Instant::now() + delay);
                        }
                        if connection.stats.peer_interested != was_interested
                            && event_tx.send(PeerEvent::InterestChanged {
                                actor_id,
                                snapshot: Box::new(connection.stats.clone()),
                            }).await.is_err()
                        {
                            break;
                        }
                        if connection.stats.peer_choking != was_peer_choking
                            && event_tx
                                .send(PeerEvent::PeerChokingChanged {
                                    actor_id,
                                    peer_choking: connection.stats.peer_choking,
                                })
                                .await
                                .is_err()
                        {
                            break;
                        }
                        if let Some(piece_index) = peer_availability_change
                            && event_tx
                                .send(PeerEvent::PeerAvailabilityChanged {
                                    actor_id,
                                    piece_index,
                                    has_piece: connection.seeder
                                        || connection.has_piece(piece_index as usize),
                                })
                                .await
                                .is_err()
                        {
                            break;
                        }
                        if (full_availability_change || connection.seeder != was_seeder)
                            && let Some(resource) = connection.session_resource.as_ref()
                            && event_tx
                                .send(PeerEvent::PeerAvailabilitySnapshot {
                                    actor_id,
                                    bitfield: resource.bitfield().to_vec(),
                                    seeder: connection.seeder,
                                })
                                .await
                                .is_err()
                        {
                            break;
                        }
                        if peer_availability_change.is_some() || full_availability_change {
                            match reconcile_peer_interest(
                                actor_id,
                                connection,
                                &wanted_pieces,
                                &event_tx,
                            )
                            .await
                            {
                                Ok(true) => {}
                                Ok(false) => break,
                                Err(error) => {
                                    tracing::debug!(actor_id = actor_id.0, %error, "Failed to update BT peer interest");
                                    let _ = event_tx
                                        .send(PeerEvent::Disconnected { actor_id })
                                        .await;
                                    break;
                                }
                            }
                        }
                        let Some(message) = message else {
                            if shutdown_requested && !connection.has_pending_upload_messages() {
                                break;
                            }
                            continue;
                        };
                        let generation = match &message {
                            BtMessage::Piece { index, begin, .. }
                            | BtMessage::Reject {
                                index,
                                offset: begin,
                                ..
                            } => requests.complete(*index, *begin),
                            _ => None,
                        };
                        let Some(generation) = generation else {
                            match &message {
                                BtMessage::Piece { index, begin, .. }
                                | BtMessage::Reject {
                                    index,
                                    offset: begin,
                                    ..
                                } => {
                                trace!(
                                    actor_id = actor_id.0,
                                    piece_index = index,
                                    offset = begin,
                                    "Ignoring unsolicited or stale BT block response"
                                );
                                }
                                _ => unreachable!("peer actor only forwards block responses"),
                            }
                            continue;
                        };
                        let stats = matches!(message, BtMessage::Piece { .. })
                            .then(|| Box::new(connection.stats.clone()));
                        if event_tx
                            .send(PeerEvent::Message {
                                actor_id,
                                generation,
                                message,
                                stats,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if shutdown_requested && !connection.has_pending_upload_messages() {
                            break;
                        }
                    }
                    Ok(None) => {
                        tracing::debug!(actor_id = actor_id.0, "BT peer closed its connection");
                        let _ = event_tx
                            .send(PeerEvent::GracefulDisconnected { actor_id })
                            .await;
                        break;
                    }
                    Err(error) => {
                        tracing::debug!(actor_id = actor_id.0, %error, "BT peer message read failed");
                        let _ = event_tx
                            .send(PeerEvent::Disconnected { actor_id })
                            .await;
                        break;
                    }
                }
            }
            _ = async {
                if let Some(deadline) = upload_flush_deadline {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }, if upload_flush_deadline.is_some() => {
                let Some(provider) = upload_provider.as_deref() else {
                    upload_flush_deadline = None;
                    if shutdown_requested {
                        break;
                    }
                    continue;
                };
                let outstanding_upload_count = connection.stats.outstanding_upload_count;
                match connection.flush_upload_messages(provider).await {
                    Ok(bytes) if bytes > 0 => {
                        connection.record_outbound_activity();
                        let Ok(permit) = event_tx.reserve().await else {
                            break;
                        };
                        let recorded_at = Instant::now();
                        permit.send(PeerEvent::UploadBytes {
                            actor_id,
                            bytes,
                            recorded_at,
                            snapshot: Box::new(connection.stats.clone()),
                        });
                    }
                    Ok(_) => {
                        if connection.stats.outstanding_upload_count != outstanding_upload_count
                            && event_tx.send(PeerEvent::UploadQueueChanged {
                                actor_id,
                                snapshot: Box::new(connection.stats.clone()),
                            }).await.is_err()
                        {
                            tracing::debug!(actor_id = actor_id.0, "Peer swarm event receiver closed while publishing flushed upload state");
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::debug!(actor_id = actor_id.0, %error, "Failed to flush queued BT upload messages");
                        let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                        break;
                    }
                }
                let retry_delay = connection
                    .pending_upload_flush_delay()
                    .unwrap_or(Duration::ZERO);
                upload_flush_deadline = connection
                    .has_pending_upload_messages()
                    .then(|| tokio::time::Instant::now() + retry_delay);
                if shutdown_requested {
                    break;
                }
            }
            _ = wait_for_upload_rate_change(&mut upload_rate_changes),
                if connection.has_pending_upload_messages() && upload_rate_changes.is_some() =>
            {
                let retry_delay = connection
                    .pending_upload_flush_delay()
                    .unwrap_or(Duration::ZERO);
                upload_flush_deadline = Some(tokio::time::Instant::now() + retry_delay);
            }
        }
    }

    actor_id
}

async fn wait_for_upload_rate_change(
    receivers: &mut Option<(watch::Receiver<u64>, Option<watch::Receiver<u64>>)>,
) {
    let Some((local, global)) = receivers.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };

    if let Some(global) = global {
        tokio::select! {
            _ = local.changed() => {}
            _ = global.changed() => {}
        }
    } else {
        let _ = local.changed().await;
    }
}

async fn reconcile_peer_interest(
    actor_id: PeerActorId,
    connection: &mut BtPeerConn,
    wanted_pieces: &[u8],
    event_tx: &mpsc::Sender<PeerEvent>,
) -> Result<bool> {
    let has_wanted_pieces = wanted_pieces.iter().any(|pieces| *pieces != 0);
    let interested = has_wanted_pieces
        && (connection.seeder
            || connection
                .session_resource
                .as_ref()
                .is_some_and(|resource| {
                    wanted_pieces
                        .iter()
                        .zip(resource.bitfield())
                        .any(|(wanted, available)| wanted & available != 0)
                }));
    if connection.stats.am_interested == interested {
        return Ok(true);
    }

    if interested {
        connection.send_interested().await?;
    } else {
        connection.send_not_interested().await?;
    }
    connection.record_outbound_activity();
    Ok(event_tx
        .send(PeerEvent::AmInterestChanged {
            actor_id,
            snapshot: Box::new(connection.stats.clone()),
        })
        .await
        .is_ok())
}

async fn reconcile_peer_choke_state(
    actor_id: PeerActorId,
    connection: &mut BtPeerConn,
    should_choke: bool,
    event_tx: &mpsc::Sender<PeerEvent>,
) -> Result<bool> {
    let Some(was_choked) = connection
        .upload_state
        .as_ref()
        .map(|state| state.is_peer_choked())
    else {
        return Ok(true);
    };
    if was_choked == should_choke {
        return Ok(true);
    }

    if should_choke {
        connection.choke_upload_peer().await?;
    } else {
        connection.unchoke_upload_peer().await?;
    }
    connection.record_outbound_activity();
    Ok(event_tx
        .send(PeerEvent::ChokeStateChanged {
            actor_id,
            snapshot: Box::new(connection.stats.clone()),
        })
        .await
        .is_ok())
}

pub(crate) async fn process_peer_message(
    connection: &mut BtPeerConn,
    message: aria2_protocol::bittorrent::message::types::BtMessage,
    dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    upload_provider: Option<
        &dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider,
    >,
) -> Result<(
    Option<aria2_protocol::bittorrent::message::types::BtMessage>,
    u64,
)> {
    use aria2_protocol::bittorrent::message::types::BtMessage;

    connection.apply_peer_state_message(&message);

    let mut uploaded_bytes = 0;
    if !matches!(message, BtMessage::Piece { .. } | BtMessage::Reject { .. })
        && let Some(provider) = upload_provider
        && connection.upload_state.is_some()
    {
        uploaded_bytes = connection
            .handle_upload_message(message.clone(), provider)
            .await?;
    }

    match message {
        BtMessage::Piece { .. } | BtMessage::Reject { .. } => Ok((Some(message), uploaded_bytes)),
        BtMessage::Port { port } => {
            if port != 0
                && let Ok(ip) = connection.ip_addr.parse()
                && let Some(engine) = dht_engine
            {
                let address = SocketAddr::new(ip, port);
                trace!(peer = %address, "Received DHT port during pipelined block read");
                tokio::spawn(async move {
                    engine.add_node(address).await;
                });
            }
            Ok((None, uploaded_bytes))
        }
        BtMessage::Extended { ext_id, payload } => {
            if connection.is_pex_enabled()
                && ext_id != 0
                && connection.peer_extension_id("ut_pex") == Some(ext_id)
            {
                process_pex_during_read(connection, ext_id, &payload);
            }
            Ok((None, uploaded_bytes))
        }
        _ => Ok((None, uploaded_bytes)),
    }
}

pub(super) fn apply_interest_change(
    workers: &mut PeerGeneration,
    peers: &PeerSchedulingSnapshot,
    choking_algo: Option<&mut ChokingAlgorithm>,
    snapshot: crate::engine::bittorrent::peer::stats::PeerStats,
) {
    let Some(algo) = choking_algo else { return };
    algo.sync_peer_by_identity(&snapshot);
    apply_choke_round(workers, peers, Some(algo));
}

pub(super) fn rebalance_upload_slots_after_peer_disconnect(
    workers: &mut PeerGeneration,
    peers: &PeerSchedulingSnapshot,
    choking_algo: Option<&mut ChokingAlgorithm>,
    identity: PeerIdentity,
) {
    let Some(algo) = choking_algo else { return };
    let released_upload_slot = algo.peers().iter().any(|peer| {
        PeerIdentity::from(peer) == identity && peer.peer_interested && !peer.am_choking
    });
    algo.remove_peers_by_identity(&[identity]);
    if released_upload_slot {
        apply_choke_round(workers, peers, Some(algo));
    }
}

pub(super) fn apply_choke_round(
    workers: &mut PeerGeneration,
    peers: &PeerSchedulingSnapshot,
    choking_algo: Option<&mut ChokingAlgorithm>,
) {
    let Some(algo) = choking_algo else { return };
    let actions = algo.rotate_choke_by_identity();
    let optimistic = algo.optimistically_unchoke_by_identity();
    for action in actions {
        let identity = action.identity();
        let choke = match action {
            IdentityChokeAction::Choke(_) => Some(true),
            IdentityChokeAction::Unchoke(_) => Some(false),
            IdentityChokeAction::NoChange(_) => None,
        };
        if let Some(choke) = choke
            && let Some(index) = peers.peer_index(identity)
            && let Some(actor_id) = peers.actor_id(index)
        {
            workers.apply_choke_action(actor_id, choke);
        }
    }
    if let Some(identity) = optimistic
        && let Some(index) = peers.peer_index(identity)
        && let Some(actor_id) = peers.actor_id(index)
    {
        workers.apply_choke_action(actor_id, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::pipelined::BlockRequest;
    use crate::engine::bittorrent::peer::upload_session::{
        BtSeedingConfig, InMemoryPieceProvider, PieceDataProvider,
    };
    use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
    use aria2_protocol::bittorrent::peer::connection::PeerConnection;

    #[test]
    fn local_metadata_response_serves_bep9_chunks_and_rejects_out_of_range_pieces() {
        let metadata = vec![0x5a; METADATA_PIECE_SIZE + 7];

        assert_eq!(
            local_metadata_response(Some(&metadata), 0),
            aria2_protocol::bittorrent::message::extension::UtMetadataMessage::Data {
                piece: 0,
                total_size: metadata.len() as u32,
                data: vec![0x5a; METADATA_PIECE_SIZE],
            }
        );
        assert_eq!(
            local_metadata_response(Some(&metadata), 1),
            aria2_protocol::bittorrent::message::extension::UtMetadataMessage::Data {
                piece: 1,
                total_size: metadata.len() as u32,
                data: vec![0x5a; 7],
            }
        );
        assert_eq!(
            local_metadata_response(Some(&metadata), 2),
            aria2_protocol::bittorrent::message::extension::UtMetadataMessage::Reject { piece: 2 }
        );
        assert_eq!(
            local_metadata_response(None, 0),
            aria2_protocol::bittorrent::message::extension::UtMetadataMessage::Reject { piece: 0 }
        );
    }

    #[test]
    fn generation_end_updates_snapshot_when_bounded_peer_mailbox_is_full() {
        let (control, mut receiver) = PeerActorControl::channel(1);
        control
            .try_send(PeerCommand::HavePiece { piece_index: 4 })
            .unwrap();
        let generation = RequestGeneration::allocate();

        control.begin_generation(generation, 9).unwrap();
        control.end_generation(generation, 9).unwrap();

        assert!(matches!(
            receiver.commands.try_recv(),
            Ok(PeerCommand::HavePiece { piece_index: 4 })
        ));
        assert!(receiver.generation_updates.borrow().is_empty());
    }

    #[test]
    fn generation_start_updates_snapshot_when_bounded_peer_mailbox_is_full() {
        let (control, mut receiver) = PeerActorControl::channel(1);
        control
            .try_send(PeerCommand::HavePiece { piece_index: 4 })
            .unwrap();
        let generation = RequestGeneration::allocate();

        control.begin_generation(generation, 9).unwrap();

        assert_eq!(
            receiver.generation_updates.borrow().get(&9),
            Some(&generation)
        );
        assert!(matches!(
            receiver.commands.try_recv(),
            Ok(PeerCommand::HavePiece { piece_index: 4 })
        ));
    }

    #[test]
    fn generation_updates_do_not_accumulate_while_actor_is_not_polling() {
        let (control, receiver) = PeerActorControl::channel(1);
        let old_generation = RequestGeneration::allocate();
        let current_generation = RequestGeneration::allocate();
        let other_piece_generation = RequestGeneration::allocate();

        control.begin_generation(old_generation, 9).unwrap();
        control
            .begin_generation(other_piece_generation, 10)
            .unwrap();
        control.begin_generation(current_generation, 9).unwrap();
        control.end_generation(old_generation, 9).unwrap();

        let active = receiver.generation_updates.borrow();
        assert_eq!(active.get(&9), Some(&current_generation));
        assert_eq!(active.get(&10), Some(&other_piece_generation));
        assert_eq!(
            active.len(),
            2,
            "the snapshot keeps each piece's current generation but no history"
        );
    }

    #[tokio::test]
    async fn desired_peer_state_updates_do_not_wait_for_bounded_mailbox_capacity() {
        let (control, mut receiver) = PeerActorControl::channel(1);
        control
            .try_send(PeerCommand::HavePiece { piece_index: 4 })
            .unwrap();

        assert!(control.set_upload_choked(false));
        control
            .set_wanted_pieces(Arc::from([0x80]))
            .expect("the actor should still receive coalesced state updates");
        assert!(control.set_upload_choked(true));
        control
            .set_wanted_pieces(Arc::from([0x40]))
            .expect("the latest state should replace the pending intermediate value");

        receiver.desired_state_updates.changed().await.unwrap();
        let desired = receiver.desired_state_updates.borrow_and_update();
        assert_eq!(desired.wanted_pieces.as_ref(), &[0x40]);
        assert!(desired.choke_upload);
        assert!(matches!(
            receiver.commands.try_recv(),
            Ok(PeerCommand::HavePiece { piece_index: 4 })
        ));
    }

    #[tokio::test]
    async fn bounded_mailbox_capacity_release_wakes_waiter() {
        let (control, mut receiver) = PeerActorControl::channel(1);
        control
            .try_send(PeerCommand::HavePiece { piece_index: 1 })
            .unwrap();
        let mut capacity_updates = control.queue_capacity_updates();
        assert!(matches!(
            control.try_send(PeerCommand::HavePiece { piece_index: 2 }),
            Err(mpsc::error::TrySendError::Full(_))
        ));

        let command = receiver.commands.recv().await;
        assert!(matches!(
            command,
            Some(PeerCommand::HavePiece { piece_index: 1 })
        ));
        notify_queue_capacity(&receiver.capacity_updates);
        tokio::time::timeout(Duration::from_secs(1), capacity_updates.changed())
            .await
            .expect("dequeue should wake capacity waiters")
            .expect("capacity watch should remain open");
        assert!(
            control
                .try_send(PeerCommand::HavePiece { piece_index: 3 })
                .is_ok()
        );
    }

    async fn read_message_while_actor_runs(
        remote: &mut PeerConnection,
    ) -> aria2_protocol::bittorrent::message::types::BtMessage {
        use tokio::time::{Duration, timeout};

        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    async fn receive_event_while_actor_runs(event_rx: &mut PeerSwarmEventLease<'_>) -> PeerEvent {
        use tokio::time::{Duration, timeout};

        loop {
            let event = timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .expect("peer event channel closed");
            if !matches!(event, PeerEvent::OutstandingDownloadRequests { .. }) {
                return event;
            }
        }
    }

    #[tokio::test]
    async fn peer_actor_sends_startup_allowed_fast_message() {
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
        let mut piece_provider = InMemoryPieceProvider::new(16, 2);
        piece_provider.set_piece_data(0, vec![0xA5; 16]);
        connection.configure_upload_with_auto_unchoke(
            &BtSeedingConfig::default(),
            crate::rate_limiter::RateLimiter::unlimited(),
            2,
            16,
            false,
        );
        connection.actor_startup = Some(
            crate::engine::bittorrent::peer::connection::PeerActorStartup {
                peer_agent: "test-peer".to_string(),
                listen_port: None,
                allowed_fast: vec![3],
            },
        );

        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = PeerActorControl::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let provider = Arc::new(piece_provider);
        let worker = tokio::spawn(async move {
            run_peer_actor(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                None,
                Some(provider),
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            )
            .await;
        });

        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        assert_eq!(
            read_message_while_actor_runs(&mut remote).await,
            BtMessage::Bitfield {
                data: vec![0b1000_0000]
            },
            "outbound actor startup must advertise only verified local pieces"
        );
        let allowed_fast = timeout(Duration::from_secs(1), remote.read_message()).await;
        assert!(
            allowed_fast.is_ok(),
            "peer actor stopped during startup: {}",
            matches!(event_rx.try_recv(), Ok(PeerEvent::Disconnected { actor_id: id }) if id == actor_id)
        );
        assert_eq!(
            allowed_fast.unwrap().unwrap().unwrap(),
            BtMessage::AllowedFast { index: 3 }
        );

        command_tx.send(PeerCommand::Shutdown).await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn peer_actor_advertises_dht_udp_port_not_tcp_listen_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], true, true);
        let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
        connection.configure_upload_with_auto_unchoke(
            &BtSeedingConfig::default(),
            crate::rate_limiter::RateLimiter::unlimited(),
            1,
            16,
            false,
        );
        connection.actor_startup = Some(
            crate::engine::bittorrent::peer::connection::PeerActorStartup {
                peer_agent: "test-peer".to_string(),
                listen_port: Some(6881),
                allowed_fast: vec![0],
            },
        );
        let dht = aria2_protocol::bittorrent::dht::engine::DhtEngine::start(
            aria2_protocol::bittorrent::dht::engine::DhtEngineConfig::local(),
        )
        .await
        .expect("local DHT engine should start");
        let dht_port = dht.local_addr().port();

        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = PeerActorControl::channel(8);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let provider = Arc::new(InMemoryPieceProvider::new(16, 1));
        let actor_dht = Arc::clone(&dht);
        let worker = tokio::spawn(async move {
            let mut connection = connection;
            run_peer_actor(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                Some(actor_dht),
                Some(provider),
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            )
            .await;
        });

        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        assert_eq!(
            read_message_while_actor_runs(&mut remote).await,
            BtMessage::HaveNone,
            "availability must precede DHT and AllowedFast startup messages"
        );
        assert_eq!(
            read_message_while_actor_runs(&mut remote).await,
            BtMessage::Port { port: dht_port },
            "BEP 5 PORT must advertise the selected DHT engine's UDP port before AllowedFast"
        );
        assert_eq!(
            read_message_while_actor_runs(&mut remote).await,
            BtMessage::AllowedFast { index: 0 }
        );

        command_tx.send(PeerCommand::Shutdown).await.unwrap();
        worker.await.unwrap();
        dht.shutdown_async().await;
    }

    #[tokio::test]
    async fn peer_actor_cancels_inflight_request_before_orderly_shutdown() {
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = PeerActorControl::channel(8);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let worker = tokio::spawn(async move {
            run_peer_actor(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                None,
                None,
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            )
            .await;
            connection
        });

        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        let generation = RequestGeneration::allocate();
        let request = BlockRequest {
            block_index: 0,
            offset: 0,
            length: 16 * 1024,
        };
        command_tx.begin_generation(generation, 3).unwrap();
        command_tx
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
                request: PieceBlockRequest::new(3, 0, 16 * 1024),
            }
        );

        command_tx.send(PeerCommand::Shutdown).await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Cancel {
                request: PieceBlockRequest::new(3, 0, 16 * 1024),
            }
        );
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn peer_actor_publishes_piece_availability_independently_of_request_generation() {
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
        connection.allocate_session_resource(16 * 1024, 8, 8 * 16 * 1024);
        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = PeerActorControl::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let worker = tokio::spawn(async move {
            run_peer_actor(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                None,
                None,
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            )
            .await;
            connection
        });
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        remote
            .send_message(&BtMessage::Have { piece_index: 6 })
            .await
            .unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            PeerEvent::PeerAvailabilityChanged {
                actor_id: event_actor_id,
                piece_index: 6,
                has_piece: true,
            } if event_actor_id == actor_id
        ));

        let generation = RequestGeneration::allocate();
        command_tx.begin_generation(generation, 7).unwrap();
        remote
            .send_message(&BtMessage::Have { piece_index: 7 })
            .await
            .unwrap();

        assert!(matches!(
            timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            PeerEvent::PeerAvailabilityChanged {
                actor_id: event_actor_id,
                piece_index: 7,
                has_piece: true,
            } if event_actor_id == actor_id
        ));
        command_tx.send(PeerCommand::Shutdown).await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn peer_generation_reuses_actor_io_across_retry() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote_stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
        connection.allocate_session_resource(16, 8, 128);
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 8));
        let mut swarm = PeerSwarm::new(16);
        assert!(swarm.spawn_peer(connection, None, provider).is_ok());
        let mut workers = PeerGeneration::from_swarm(&swarm, &[2]);
        let mut event_rx = swarm.lease_event_receiver().unwrap();
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        let first_generation = workers.generation();
        let first_request = BlockRequest {
            block_index: 0,
            offset: 0,
            length: 16,
        };

        workers
            .senders
            .get(&actor_id)
            .as_ref()
            .unwrap()
            .try_request(first_generation, 2, first_request)
            .unwrap();
        assert_eq!(
            read_message_while_actor_runs(&mut remote).await,
            BtMessage::Request {
                request: PieceBlockRequest::new(2, 0, 16),
            }
        );

        workers.advance_generations();
        let retry_generation = workers.generation();
        assert_ne!(first_generation, retry_generation);
        assert_eq!(
            read_message_while_actor_runs(&mut remote).await,
            BtMessage::Cancel {
                request: PieceBlockRequest::new(2, 0, 16),
            }
        );
        remote
            .send_message(&BtMessage::Piece {
                index: 2,
                begin: 0,
                data: vec![0x22; 16].into(),
            })
            .await
            .unwrap();
        workers
            .senders
            .get(&actor_id)
            .as_ref()
            .unwrap()
            .try_request(retry_generation, 2, first_request)
            .unwrap();
        assert_eq!(
            read_message_while_actor_runs(&mut remote).await,
            BtMessage::Request {
                request: PieceBlockRequest::new(2, 0, 16),
            }
        );
        remote
            .send_message(&BtMessage::Piece {
                index: 2,
                begin: 0,
                data: vec![0x55; 16].into(),
            })
            .await
            .unwrap();
        assert!(matches!(
            receive_event_while_actor_runs(&mut event_rx).await,
            PeerEvent::Message {
                actor_id: event_actor_id,
                generation,
                message: BtMessage::Piece { index: 2, .. },
                ..
            } if event_actor_id == actor_id && generation == retry_generation
        ));

        workers.finish_generation(&mut event_rx).await;
        drop(event_rx);
        swarm.shutdown_all().await;
    }

    #[tokio::test]
    async fn peer_actor_publishes_full_availability_outside_generation() {
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
        connection.allocate_session_resource(16 * 1024, 8, 8 * 16 * 1024);
        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = PeerActorControl::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let worker = tokio::spawn(async move {
            run_peer_actor(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                None,
                None,
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            )
            .await;
        });
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);

        for (message, expected) in [
            (
                BtMessage::Bitfield {
                    data: vec![0b1010_0000],
                },
                vec![0b1010_0000],
            ),
            (BtMessage::HaveAll, vec![0xff]),
            (BtMessage::HaveNone, vec![0]),
        ] {
            remote.send_message(&message).await.unwrap();
            assert!(matches!(
                timeout(Duration::from_secs(1), event_rx.recv())
                    .await
                    .unwrap()
                    .unwrap(),
                PeerEvent::PeerAvailabilitySnapshot {
                    actor_id: event_actor_id,
                    bitfield,
                    seeder,
                } if event_actor_id == actor_id
                    && bitfield == expected
                    && seeder == matches!(message, BtMessage::HaveAll)
            ));
        }

        command_tx.send(PeerCommand::Shutdown).await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn generation_finish_preserves_queued_availability_events() {
        let actor_id = PeerActorId(41);
        let mut swarm = PeerSwarm::new(2);
        swarm
            .event_tx
            .as_ref()
            .unwrap()
            .send(PeerEvent::PeerAvailabilityChanged {
                actor_id,
                piece_index: 2,
                has_piece: true,
            })
            .await
            .unwrap();
        let mut workers = PeerGeneration {
            senders: HashMap::new(),
            generation: RequestGeneration::allocate(),
            active_pieces: HashSet::from([2]),
            availability_changed_actor_ids: HashSet::new(),
            pex_peers: Vec::new(),
        };

        let mut event_stream = swarm.lease_event_receiver().unwrap();
        workers.finish_generation(&mut event_stream).await;

        assert!(workers.take_availability_changes().contains(&actor_id));
    }

    #[tokio::test]
    async fn explicit_peer_actor_shutdown_cancels_inflight_request() {
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let connection = BtPeerConn::from_incoming_tcp(local, endpoint);
        let actor_id = connection.actor_id;
        let (event_tx, _event_rx) = mpsc::channel(8);
        let mut actor = PeerActorTask::spawn_owned(
            actor_id,
            connection,
            event_tx,
            None,
            None,
            Arc::new(AtomicUsize::new(0)),
            8,
        );
        let control = actor.control.clone();
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        let generation = RequestGeneration::allocate();
        let request = BlockRequest {
            block_index: 0,
            offset: 0,
            length: 16 * 1024,
        };
        control.begin_generation(generation, 5).unwrap();
        control
            .send(PeerCommand::Request {
                generation,
                piece_index: 5,
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
                request: PieceBlockRequest::new(5, 0, 16 * 1024),
            }
        );

        let shutdown = tokio::spawn(async move {
            let first = actor.shutdown().await;
            let second = actor.shutdown().await;
            (first, second)
        });

        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Cancel {
                request: PieceBlockRequest::new(5, 0, 16 * 1024),
            }
        );
        let (first, second) = shutdown.await.unwrap();
        first.unwrap();
        second.unwrap();
    }

    #[tokio::test]
    async fn peer_actor_shutdown_is_bounded_when_io_never_completes() {
        let (control, _receiver) = PeerActorControl::channel(1);
        let task = tokio::spawn(std::future::pending::<()>());
        let mut actor = PeerActorTask {
            control,
            task: Some(task),
        };
        let started = Instant::now();

        tokio::time::timeout(Duration::from_secs(2), actor.shutdown())
            .await
            .expect("peer actor shutdown must be bounded")
            .expect("aborting a stalled peer actor is successful cleanup");

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(actor.task.is_none());
    }

    #[tokio::test]
    async fn cancelling_peer_actor_shutdown_retains_join_handle_for_retry() {
        let (control, _receiver) = PeerActorControl::channel(1);
        let task = tokio::spawn(std::future::pending::<()>());
        let mut actor = PeerActorTask {
            control,
            task: Some(task),
        };

        {
            let shutdown = actor.shutdown();
            tokio::pin!(shutdown);
            assert!(
                tokio::time::timeout(Duration::from_millis(25), &mut shutdown)
                    .await
                    .is_err()
            );
        }

        assert!(
            actor.task.is_some(),
            "cancelling the shutdown future must not detach the peer task"
        );
        actor.task.as_ref().unwrap().abort();
        actor
            .shutdown()
            .await
            .expect("a retried shutdown must join the aborted task");
        assert!(actor.task.is_none());
    }

    #[tokio::test]
    async fn peer_actor_survives_piece_generation_rollover_and_drops_late_blocks() {
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let connection = BtPeerConn::from_incoming_tcp(local, endpoint);
        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = PeerActorControl::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let worker = tokio::spawn(async move {
            let mut connection = connection;
            run_peer_actor(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                None,
                None,
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            )
            .await;
        });
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        let first_generation = RequestGeneration::allocate();
        let first_request = BlockRequest {
            block_index: 0,
            offset: 0,
            length: 16,
        };
        command_tx.begin_generation(first_generation, 3).unwrap();
        command_tx
            .send(PeerCommand::Request {
                generation: first_generation,
                piece_index: 3,
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
                request: PieceBlockRequest::new(3, 0, 16),
            }
        );

        command_tx.end_generation(first_generation, 3).unwrap();
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
        let request_count_event = loop {
            let event = timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(
                event,
                PeerEvent::OutstandingDownloadRequests { count: 0, .. }
            ) {
                break event;
            }
        };
        assert!(matches!(
            request_count_event,
            PeerEvent::OutstandingDownloadRequests { actor_id: event_actor, count: 0 }
                if event_actor == actor_id
        ));
        remote
            .send_message(&BtMessage::Piece {
                index: 3,
                begin: 0,
                data: vec![0x33; 16].into(),
            })
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(50), event_rx.recv())
                .await
                .is_err()
        );

        let next_generation = RequestGeneration::allocate();
        let next_request = BlockRequest {
            block_index: 0,
            offset: 0,
            length: 16,
        };
        command_tx.begin_generation(next_generation, 4).unwrap();
        command_tx
            .send(PeerCommand::Request {
                generation: next_generation,
                piece_index: 4,
                request: next_request,
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
                request: PieceBlockRequest::new(4, 0, 16),
            }
        );
        remote
            .send_message(&BtMessage::Piece {
                index: 4,
                begin: 0,
                data: vec![0x44; 16].into(),
            })
            .await
            .unwrap();
        let piece_event = loop {
            let event = timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap();
            if !matches!(event, PeerEvent::OutstandingDownloadRequests { .. }) {
                break event;
            }
        };
        assert!(matches!(
            piece_event,
            PeerEvent::Message {
                actor_id: received_actor,
                generation,
                message: BtMessage::Piece { index: 4, .. },
                ..
            } if received_actor == actor_id && generation == next_generation
        ));

        command_tx.send(PeerCommand::Shutdown).await.unwrap();
        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn active_peer_actor_cancels_queued_upload_piece_before_flush() {
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
        connection.configure_upload_with_auto_unchoke(
            &BtSeedingConfig::default(),
            crate::rate_limiter::RateLimiter::unlimited(),
            1,
            32,
            true,
        );

        let mut provider = InMemoryPieceProvider::new(32, 1);
        provider.set_piece_data(0, vec![0x71; 32]);
        let provider: Arc<dyn PieceDataProvider> = Arc::new(provider);
        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = PeerActorControl::channel_with_initial_choke(8, false);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let worker = tokio::spawn(async move {
            run_peer_actor(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                None,
                Some(provider),
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            )
            .await;
            connection
        });

        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        let request = PieceBlockRequest::new(0, 8, 8);
        remote
            .send_message(&BtMessage::Request {
                request: request.clone(),
            })
            .await
            .unwrap();
        let first_event = timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let event_kind = if matches!(&first_event, PeerEvent::ChokeStateChanged { .. }) {
            "ChokeStateChanged"
        } else {
            "another PeerEvent variant"
        };
        assert!(
            matches!(
                &first_event,
                PeerEvent::UploadQueueChanged { actor_id: event_actor_id, snapshot }
                    if *event_actor_id == actor_id && snapshot.outstanding_upload_count == 1
            ),
            "unexpected first peer event: {event_kind}"
        );
        remote
            .send_message(&BtMessage::Cancel { request })
            .await
            .unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            PeerEvent::UploadQueueChanged { actor_id: event_actor_id, snapshot }
                if event_actor_id == actor_id && snapshot.outstanding_upload_count == 0
        ));

        let response = timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            response,
            BtMessage::Reject {
                index: 0,
                offset: 8,
                length: 8,
            }
        );

        command_tx.send(PeerCommand::Shutdown).await.unwrap();
        let _connection = worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn peer_actor_keeps_connection_on_keepalive_without_piece_progress() {
        use tokio::time::{Duration, advance, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
        connection.set_timeouts(Duration::from_secs(120), Duration::from_secs(80));
        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = PeerActorControl::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let worker = tokio::spawn(async move {
            run_peer_actor(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                None,
                None,
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            )
            .await;
        });
        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);

        remote.send_message(&BtMessage::Interested).await.unwrap();
        tokio::task::yield_now().await;
        loop {
            if matches!(
                timeout(Duration::from_secs(5), event_rx.recv())
                    .await
                    .unwrap()
                    .expect("peer actor event channel closed"),
                PeerEvent::InterestChanged { actor_id: event_actor_id, .. }
                    if event_actor_id == actor_id
            ) {
                break;
            }
        }

        advance(Duration::from_secs(50)).await;
        remote.send_message(&BtMessage::KeepAlive).await.unwrap();
        remote.send_message(&BtMessage::Unchoke).await.unwrap();
        tokio::task::yield_now().await;
        loop {
            if matches!(
                timeout(Duration::from_secs(5), event_rx.recv())
                    .await
                    .unwrap()
                    .expect("peer actor event channel closed"),
                PeerEvent::PeerChokingChanged {
                    actor_id: event_actor_id,
                    peer_choking: false,
                } if event_actor_id == actor_id
            ) {
                break;
            }
        }

        advance(Duration::from_secs(15)).await;
        tokio::task::yield_now().await;
        assert!(
            !worker.is_finished(),
            "valid keepalive/control traffic must keep the connection alive until bt-timeout"
        );

        command_tx.send(PeerCommand::Shutdown).await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn upload_rate_change_wakes_actor_without_retry_polling() {
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
        let limiter = crate::rate_limiter::RateLimiter::new(
            &crate::rate_limiter::RateLimiterConfig::new(None, Some(1)).with_burst(None, Some(0)),
        );
        let global_limiter = crate::rate_limiter::RateLimiter::new(
            &crate::rate_limiter::RateLimiterConfig::new(None, Some(1)).with_burst(None, Some(0)),
        );
        let config = BtSeedingConfig {
            global_limiter: Some(global_limiter.clone()),
            ..BtSeedingConfig::default()
        };
        connection.configure_upload_with_auto_unchoke(&config, limiter.clone(), 1, 16, true);

        let mut provider = InMemoryPieceProvider::new(16, 1);
        provider.set_piece_data(0, vec![0x71; 16]);
        let provider: Arc<dyn PieceDataProvider> = Arc::new(provider);
        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = PeerActorControl::channel_with_initial_choke(8, false);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let worker = tokio::spawn(async move {
            run_peer_actor(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                None,
                Some(provider),
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            )
            .await;
        });

        let mut remote =
            PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
        remote.send_message(&BtMessage::Interested).await.unwrap();
        assert_eq!(
            timeout(Duration::from_secs(1), remote.read_message())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            BtMessage::Unchoke
        );
        remote
            .send_message(&BtMessage::Request {
                request: PieceBlockRequest::new(0, 0, 8),
            })
            .await
            .unwrap();
        tokio::task::yield_now().await;

        loop {
            let event = timeout(Duration::from_secs(5), event_rx.recv())
                .await
                .unwrap()
                .expect("peer actor event channel closed");
            if matches!(
                event,
                PeerEvent::UploadQueueChanged { actor_id: event_actor_id, snapshot }
                    if event_actor_id == actor_id && snapshot.outstanding_upload_count == 1
            ) {
                break;
            }
        }

        assert!(
            timeout(Duration::from_millis(80), remote.read_message())
                .await
                .is_err(),
            "the request must remain queued while the one-byte-per-second limiter has no tokens"
        );
        limiter.set_upload_rate(None);
        global_limiter.set_upload_rate(None);

        assert_eq!(
            timeout(Duration::from_millis(100), remote.read_message())
                .await
                .expect(
                    "rate change should wake the actor without waiting for its old retry deadline"
                )
                .unwrap()
                .unwrap(),
            BtMessage::Piece {
                index: 0,
                begin: 0,
                data: vec![0x71; 8].into(),
            }
        );

        command_tx.send(PeerCommand::Shutdown).await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn peer_actor_downloads_a_block_over_utp_after_handshake_handoff() {
        use aria2_protocol::bittorrent::message::handshake::Handshake;
        use aria2_protocol::bittorrent::message::serializer::serialize;
        use aria2_protocol::bittorrent::utp::UtpSocket;

        let info_hash = [0x41; 20];
        let local_peer_id = [0x52; 20];
        let remote_peer_id = [0x63; 20];
        let mut server = UtpSocket::bind("127.0.0.1:0").expect("bind uTP test peer");
        let address = server.local_addr();
        let server_task = tokio::spawn(async move {
            let mut request_buffer = Vec::new();
            loop {
                for (connection_id, bytes) in server
                    .poll_recv()
                    .expect("uTP test peer should process incoming packets")
                {
                    request_buffer.extend_from_slice(&bytes);
                    if request_buffer.len() >= 68 {
                        let request = Handshake::parse(&request_buffer[..68])
                            .expect("client should send a valid BT handshake");
                        assert_eq!(request.info_hash, info_hash);
                        assert_eq!(request.peer_id, local_peer_id);
                        server
                            .send(
                                connection_id,
                                &Handshake::new(&info_hash, &remote_peer_id).to_bytes(),
                            )
                            .expect("uTP test peer should answer BT handshake");
                        return (server, connection_id);
                    }
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        let client_socket = Arc::new(tokio::sync::Mutex::new(
            UtpSocket::bind("127.0.0.1:0").expect("bind uTP client socket"),
        ));
        let mut connection = BtPeerConn::connect_utp_with_policy(
            address,
            &info_hash,
            None,
            crate::engine::bittorrent::peer::connection::UtpConnectionOptions {
                local_peer_id,
                timeout: Duration::from_secs(2),
                listen_port: None,
                shared_socket: Some(client_socket),
                dht_enabled: false,
            },
            &crate::network::OutboundNetworkPolicy::direct(),
        )
        .await
        .expect("BT handshake should complete over uTP");
        assert_eq!(
            connection.connection_type,
            crate::engine::bittorrent::peer::connection::ConnectionType::Utp
        );
        let (mut server, connection_id) = tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("uTP peer should finish the BitTorrent handshake")
            .expect("uTP peer task should not panic");

        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = PeerActorControl::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let worker = tokio::spawn(async move {
            run_peer_actor(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                None,
                None,
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            )
            .await;
        });

        server
            .send(connection_id, &serialize(&BtMessage::Unchoke))
            .expect("uTP peer should unchoke the actor");
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .expect("actor should process the uTP Unchoke")
                .expect("peer actor event channel should remain open");
            if matches!(
                event,
                PeerEvent::PeerChokingChanged {
                    actor_id: event_actor_id,
                    peer_choking: false,
                } if event_actor_id == actor_id
            ) {
                break;
            }
        }

        let generation = RequestGeneration::allocate();
        let request = BlockRequest {
            block_index: 0,
            offset: 0,
            length: 16,
        };
        command_tx.begin_generation(generation, 0).unwrap();
        command_tx
            .send(PeerCommand::Request {
                generation,
                piece_index: 0,
                request,
            })
            .await
            .unwrap();
        let expected_request = serialize(&BtMessage::Request {
            request: PieceBlockRequest::new(0, 0, 16),
        });
        let request_bytes = tokio::time::timeout(Duration::from_secs(2), async {
            let mut received = Vec::new();
            loop {
                for (received_connection_id, bytes) in server
                    .poll_recv()
                    .expect("uTP peer should receive actor packets")
                {
                    assert_eq!(received_connection_id, connection_id);
                    received.extend_from_slice(&bytes);
                    if received
                        .windows(expected_request.len())
                        .any(|window| window == expected_request)
                    {
                        return received;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("uTP peer should receive the actor's block request");
        assert!(
            request_bytes
                .windows(expected_request.len())
                .any(|window| window == expected_request)
        );

        let block = vec![0xA5; 16];
        server
            .send(
                connection_id,
                &serialize(&BtMessage::Piece {
                    index: 0,
                    begin: 0,
                    data: block.clone().into(),
                }),
            )
            .expect("uTP peer should return the requested block");
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .expect("actor should receive the requested block over uTP")
                .expect("peer actor event channel should remain open");
            if let PeerEvent::Message {
                actor_id: event_actor_id,
                message: BtMessage::Piece { index, begin, data },
                ..
            } = event
            {
                assert_eq!(event_actor_id, actor_id);
                assert_eq!(index, 0);
                assert_eq!(begin, 0);
                assert_eq!(data.as_ref(), block);
                break;
            }
        }

        command_tx.send(PeerCommand::Shutdown).await.unwrap();
        worker.await.unwrap();
    }
}

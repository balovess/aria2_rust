//! Long-lived peer I/O actors and piece-scoped actor command sets.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::trace;

use crate::engine::bt_peer_connection::{BtPeerConn, PeerActorId};
use crate::engine::choking_algorithm::{ChokingAlgorithm, IdentityChokeAction};
use crate::error::Result;

use super::normal::process_pex_during_read;
use super::peer_registry::{PeerSwarm, PeerSwarmEventLease};
use super::peer_request::PeerRequestLedger;
pub(super) use super::peer_request::RequestGeneration;
use super::peer_snapshot::PeerSchedulingSnapshot;
use super::pipelined::BlockRequest;

const UPLOAD_RATE_LIMIT_RETRY_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) enum PeerCommand {
    BeginGeneration {
        generation: RequestGeneration,
        piece_index: u32,
    },
    EndGeneration {
        generation: RequestGeneration,
        piece_index: u32,
    },
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
    SetWantedPieces(Arc<[u8]>),
    SendPex(Vec<u8>),
    AnnounceAvailability,
    ChokeUpload,
    UnchokeUpload,
    Shutdown,
}

pub(crate) enum PeerEvent {
    Message {
        actor_id: PeerActorId,
        generation: RequestGeneration,
        message: aria2_protocol::bittorrent::message::types::BtMessage,
        stats: Option<Box<crate::engine::peer_stats::PeerStats>>,
    },
    InterestChanged {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::peer_stats::PeerStats>,
    },
    AmInterestChanged {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::peer_stats::PeerStats>,
    },
    ChokeStateChanged {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::peer_stats::PeerStats>,
    },
    PeerChokingChanged {
        actor_id: PeerActorId,
        peer_choking: bool,
    },
    AllowedFast {
        actor_id: PeerActorId,
        piece_index: u32,
    },
    AvailabilityChanged {
        actor_id: PeerActorId,
        generation: RequestGeneration,
        has_piece: bool,
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
    PexNegotiated {
        actor_id: PeerActorId,
        ut_pex_id: Option<u8>,
    },
    PexPeers {
        peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    },
    UploadBytes {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::peer_stats::PeerStats>,
    },
    UploadQueueChanged {
        actor_id: PeerActorId,
        snapshot: Box<crate::engine::peer_stats::PeerStats>,
    },
    RequestFailed {
        actor_id: PeerActorId,
        generation: RequestGeneration,
        request: BlockRequest,
    },
    Disconnected {
        actor_id: PeerActorId,
    },
}

/// Bounded command endpoint shared by piece coordinators and seed actors.
#[derive(Clone)]
pub(crate) struct PeerActorControl(mpsc::Sender<PeerCommand>);

impl PeerActorControl {
    pub(crate) fn channel(capacity: usize) -> (Self, mpsc::Receiver<PeerCommand>) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        (Self(sender), receiver)
    }

    pub(crate) async fn send(
        &self,
        command: PeerCommand,
    ) -> std::result::Result<(), mpsc::error::SendError<PeerCommand>> {
        self.0.send(command).await
    }

    pub(crate) fn try_send(
        &self,
        command: PeerCommand,
    ) -> std::result::Result<(), mpsc::error::TrySendError<PeerCommand>> {
        self.0.try_send(command)
    }

    pub(crate) async fn begin_generation(
        &self,
        generation: RequestGeneration,
        piece_index: u32,
    ) -> std::result::Result<(), mpsc::error::SendError<PeerCommand>> {
        self.send(PeerCommand::BeginGeneration {
            generation,
            piece_index,
        })
        .await
    }

    pub(crate) async fn request(
        &self,
        generation: RequestGeneration,
        piece_index: u32,
        request: BlockRequest,
    ) -> std::result::Result<(), mpsc::error::SendError<PeerCommand>> {
        self.send(PeerCommand::Request {
            generation,
            piece_index,
            request,
        })
        .await
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

    pub(crate) async fn end_generation(
        &self,
        generation: RequestGeneration,
        piece_index: u32,
    ) -> std::result::Result<(), mpsc::error::SendError<PeerCommand>> {
        self.send(PeerCommand::EndGeneration {
            generation,
            piece_index,
        })
        .await
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
        upload_provider: Option<Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>>,
        pending_download_requests: Arc<AtomicUsize>,
        command_capacity: usize,
    ) -> Self {
        let (control, command_rx) = PeerActorControl::channel(command_capacity);
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
        let _ = self.control.send(PeerCommand::Shutdown).await;
        let Some(task) = self.task.as_mut() else {
            return Ok(());
        };
        let result = task.await;
        self.task = None;
        result
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
    piece_index: u32,
    availability_changed_actor_ids: HashSet<PeerActorId>,
    pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
}

pub(super) enum TryRequestError {
    Full,
    Closed,
}

impl PeerGeneration {
    /// Begin a piece request generation on actors owned by the torrent swarm.
    /// The returned scheduler borrows only command handles; ending it never
    /// shuts down the peer connections.
    pub(super) async fn from_swarm(swarm: &PeerSwarm, piece_index: u32) -> Self {
        let generation = RequestGeneration::allocate();
        let mut senders = HashMap::with_capacity(swarm.len());
        for actor in swarm.iter().filter(|actor| !actor.dead) {
            let control = actor.handle();
            if control
                .begin_generation(generation, piece_index)
                .await
                .is_ok()
            {
                senders.insert(actor.actor_id, control);
            }
        }

        Self {
            senders,
            generation,
            piece_index,
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

    pub(super) async fn request(
        &mut self,
        actor_id: PeerActorId,
        piece_index: u32,
        request: BlockRequest,
    ) -> bool {
        let Some(sender) = self.senders.get(&actor_id) else {
            return false;
        };
        sender
            .request(self.generation, piece_index, request)
            .await
            .is_ok()
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
        match sender.try_request(self.generation, piece_index, request) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(TryRequestError::Full),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(TryRequestError::Closed),
        }
    }

    /// Reuse the same peer I/O tasks for a retry while advancing the request
    /// epoch. This drains old in-flight blocks before accepting new requests.
    pub(super) async fn advance_generation(&mut self, piece_index: u32) {
        for sender in self.senders.values() {
            let _ = sender
                .end_generation(self.generation, self.piece_index)
                .await;
        }
        self.generation = RequestGeneration::allocate();
        self.piece_index = piece_index;
        let actor_ids = self.senders.keys().copied().collect::<Vec<_>>();
        let mut failed_peers = Vec::new();
        for actor_id in actor_ids {
            let Some(control) = self.senders.get(&actor_id) else {
                continue;
            };
            if control
                .begin_generation(self.generation, self.piece_index)
                .await
                .is_err()
            {
                failed_peers.push(actor_id);
            }
        }
        for actor_id in failed_peers {
            self.senders.remove(&actor_id);
        }
    }

    /// Cancel this attempt's requests after they have been requeued. The peer
    /// I/O task stays alive so a later piece retry can reuse the connection.
    pub(super) fn cancel_peer_requests(
        &mut self,
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

    pub(super) async fn apply_choke_action(&mut self, actor_id: PeerActorId, choke: bool) -> bool {
        let Some(sender) = self.senders.get(&actor_id) else {
            return false;
        };
        let command = if choke {
            PeerCommand::ChokeUpload
        } else {
            PeerCommand::UnchokeUpload
        };
        sender.send(command).await.is_ok()
    }

    /// End this piece generation without stopping torrent-owned peer actors.
    pub(super) async fn finish_generation(&mut self, event_rx: &mut PeerSwarmEventLease<'_>) {
        for sender in self.senders.values() {
            let _ = sender
                .end_generation(self.generation, self.piece_index)
                .await;
        }
        self.senders.clear();
        while let Ok(event) = event_rx.try_recv() {
            match event {
                PeerEvent::PeerAvailabilityChanged { actor_id, .. }
                | PeerEvent::PeerAvailabilitySnapshot { actor_id, .. } => {
                    self.record_availability_change(actor_id);
                }
                PeerEvent::PexPeers { peers, .. } => self.record_pex_peers(peers),
                _ => {}
            }
        }
    }
}

pub(crate) async fn run_peer_actor(
    actor_id: PeerActorId,
    connection: &mut BtPeerConn,
    mut command_rx: mpsc::Receiver<PeerCommand>,
    event_tx: mpsc::Sender<PeerEvent>,
    dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    upload_provider: Option<Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>>,
    pending_download_requests: Arc<AtomicUsize>,
) -> PeerActorId {
    let mut availability_sent = false;
    let mut wanted_pieces: Arc<[u8]> = Arc::from([]);
    if let Some(startup) = connection.actor_startup.take() {
        let startup_result = async {
            connection
                .send_extension_handshake_with_port(&startup.peer_agent, startup.listen_port)
                .await?;
            let provider = upload_provider.as_deref().ok_or_else(|| {
                crate::error::Aria2Error::DownloadFailed(
                    "peer actor started without an upload provider".into(),
                )
            })?;
            connection.announce_upload_availability(provider).await?;
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
            if startup.dht_enabled
                && connection.remote_supports_dht()
                && let Some(port) = startup.listen_port
            {
                connection.send_port(port).await?;
            }
            Result::<()>::Ok(())
        }
        .await;
        if let Err(error) = startup_result {
            tracing::debug!(actor_id = actor_id.0, %error, "BT peer actor startup failed");
            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
            return actor_id;
        }
        availability_sent = true;
    }
    let mut requests = PeerRequestLedger::default();
    let mut active_generation: Option<(RequestGeneration, u32)> = None;
    let mut upload_flush_deadline: Option<tokio::time::Instant> = None;
    let mut shutdown_requested = false;
    loop {
        pending_download_requests.store(requests.len(), Ordering::Relaxed);
        if shutdown_requested && !connection.has_pending_upload_messages() {
            break;
        }
        if connection.has_pending_upload_messages() {
            upload_flush_deadline.get_or_insert_with(|| {
                tokio::time::Instant::now() + std::time::Duration::from_millis(2)
            });
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
            command = command_rx.recv(), if !shutdown_requested => {
                match command {
                    Some(PeerCommand::BeginGeneration { generation, piece_index }) => {
                        if active_generation
                            .is_some_and(|(active, _)| active.is_at_least(generation))
                        {
                            continue;
                        }
                        for (piece_index, request) in requests.drain_generation(generation) {
                            if connection
                                .send_cancel(&request.message(piece_index))
                                .await
                                .is_ok()
                            {
                                connection.record_outbound_activity();
                            }
                        }
                        active_generation = Some((generation, piece_index));
                    }
                    Some(PeerCommand::EndGeneration { generation, piece_index }) => {
                        if active_generation != Some((generation, piece_index)) {
                            continue;
                        }
                        for (request_piece, request) in requests.drain_exact_generation(generation) {
                            if connection
                                .send_cancel(&request.message(request_piece))
                                .await
                                .is_ok()
                            {
                                connection.record_outbound_activity();
                            }
                        }
                        active_generation = None;
                    }
                    Some(PeerCommand::Request { generation, piece_index, request }) => {
                        if active_generation != Some((generation, piece_index))
                            || requests.contains(piece_index, request)
                        {
                            let _ = event_tx.send(PeerEvent::RequestFailed {
                                actor_id,
                                generation,
                                request,
                            }).await;
                            continue;
                        }
                        match connection.send_request(request.message(piece_index)).await {
                            Ok(()) => {
                                requests.record(generation, piece_index, request);
                                connection.record_outbound_activity();
                            }
                            Err(_) => {
                                requests.cancel(generation, piece_index, request);
                                let _ = event_tx.send(PeerEvent::RequestFailed {
                                    actor_id,
                                    generation,
                                    request,
                                }).await;
                                break;
                            }
                        }
                    }
                    Some(PeerCommand::Cancel { generation, piece_index, request }) => {
                        if active_generation.is_some_and(|(active, _)| active == generation)
                            && requests.cancel(generation, piece_index, request)
                            && connection.send_cancel(&request.message(piece_index)).await.is_ok()
                        {
                            connection.record_outbound_activity();
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
                    Some(PeerCommand::SetWantedPieces(wanted)) => {
                        wanted_pieces = wanted;
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
                            tracing::debug!(actor_id = actor_id.0, "Peer actor has no upload provider for availability announcement");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        };
                        if let Err(error) = connection.announce_upload_availability(provider).await {
                            tracing::debug!(actor_id = actor_id.0, %error, "Failed to announce BT peer availability");
                            let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                            break;
                        }
                        availability_sent = true;
                    }
                    Some(PeerCommand::ChokeUpload) => {
                        let has_upload_state = connection.upload_state.is_some();
                        let was_choked = connection
                            .upload_state
                            .as_ref()
                            .is_some_and(|state| state.is_peer_choked());
                        match connection.choke_upload_peer().await {
                            Ok(()) if has_upload_state && !was_choked => {
                                connection.record_outbound_activity();
                                let _ = event_tx
                                    .send(PeerEvent::ChokeStateChanged {
                                        actor_id,
                                        snapshot: Box::new(connection.stats.clone()),
                                    })
                                    .await;
                            }
                            Ok(()) => {}
                            Err(error) => {
                                tracing::debug!(actor_id = actor_id.0, %error, "Failed to choke BT upload peer");
                            }
                        }
                    }
                    Some(PeerCommand::UnchokeUpload) => {
                        let has_upload_state = connection.upload_state.is_some();
                        let was_choked = connection
                            .upload_state
                            .as_ref()
                            .is_some_and(|state| state.is_peer_choked());
                        match connection.unchoke_upload_peer().await {
                            Ok(()) if has_upload_state && was_choked => {
                                connection.record_outbound_activity();
                                let _ = event_tx
                                    .send(PeerEvent::ChokeStateChanged {
                                        actor_id,
                                        snapshot: Box::new(connection.stats.clone()),
                                    })
                                    .await;
                            }
                            Ok(()) => {}
                            Err(error) => {
                                tracing::debug!(actor_id = actor_id.0, %error, "Failed to unchoke BT upload peer");
                            }
                        }
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
                        let was_interested = connection.stats.peer_interested;
                        let was_peer_choking = connection.stats.peer_choking;
                        let outstanding_upload_count = connection.stats.outstanding_upload_count;
                        let peer_availability_change = match &message {
                            aria2_protocol::bittorrent::message::types::BtMessage::Have {
                                piece_index,
                            } => Some(*piece_index),
                            _ => None,
                        };
                        let full_availability_change = matches!(
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
                            } => Some(*index),
                            _ => None,
                        };
                        let availability_change = active_generation.filter(|(_, target_piece)| {
                            use aria2_protocol::bittorrent::message::types::BtMessage;
                            match &message {
                                BtMessage::Have { piece_index } => piece_index == target_piece,
                                BtMessage::Bitfield { .. }
                                | BtMessage::HaveAll
                                | BtMessage::HaveNone => true,
                                _ => false,
                            }
                        });
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
                                .send(PeerEvent::PexNegotiated {
                                    actor_id,
                                    ut_pex_id: connection.peer_extension_id("ut_pex"),
                                })
                                .await
                                .is_err()
                        {
                            break;
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
                            if event_tx.send(PeerEvent::UploadBytes {
                                actor_id,
                                snapshot: Box::new(connection.stats.clone()),
                            }).await.is_err() {
                                break;
                            }
                        } else if connection.stats.outstanding_upload_count != outstanding_upload_count
                            && event_tx.send(PeerEvent::UploadQueueChanged {
                                actor_id,
                                snapshot: Box::new(connection.stats.clone()),
                            }).await.is_err()
                        {
                            break;
                        }
                        if connection.stats.outstanding_upload_count != outstanding_upload_count
                            && connection.has_pending_upload_messages()
                        {
                            upload_flush_deadline = Some(
                                tokio::time::Instant::now()
                                    + std::time::Duration::from_millis(2),
                            );
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
                        if full_availability_change
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
                        if let Some((generation, piece_index)) = availability_change
                            && event_tx
                                .send(PeerEvent::AvailabilityChanged {
                                    actor_id,
                                    generation,
                                    has_piece: connection.seeder
                                        || connection.has_piece(piece_index as usize),
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
                        use aria2_protocol::bittorrent::message::types::BtMessage;
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
                    Ok(None) | Err(_) => {
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
                        if event_tx.send(PeerEvent::UploadBytes {
                            actor_id,
                            snapshot: Box::new(connection.stats.clone()),
                        }).await.is_err() {
                            break;
                        }
                    }
                    Ok(_) => {
                        if connection.stats.outstanding_upload_count != outstanding_upload_count
                            && event_tx.send(PeerEvent::UploadQueueChanged {
                                actor_id,
                                snapshot: Box::new(connection.stats.clone()),
                            }).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::debug!(actor_id = actor_id.0, %error, "Failed to flush queued BT upload messages");
                        let _ = event_tx.send(PeerEvent::Disconnected { actor_id }).await;
                        break;
                    }
                }
                upload_flush_deadline = connection.has_pending_upload_messages().then(|| {
                    tokio::time::Instant::now() + UPLOAD_RATE_LIMIT_RETRY_INTERVAL
                });
                if shutdown_requested {
                    break;
                }
            }
        }
    }

    actor_id
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

pub(crate) async fn process_peer_message(
    connection: &mut BtPeerConn,
    message: aria2_protocol::bittorrent::message::types::BtMessage,
    dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    upload_provider: Option<&dyn crate::engine::bt_upload_session::PieceDataProvider>,
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

pub(super) async fn apply_interest_change(
    workers: &mut PeerGeneration,
    peers: &PeerSchedulingSnapshot,
    choking_algo: Option<&mut ChokingAlgorithm>,
    snapshot: crate::engine::peer_stats::PeerStats,
) {
    let Some(algo) = choking_algo else { return };
    algo.sync_peer_by_identity(&snapshot);
    apply_choke_round(workers, peers, Some(algo)).await;
}

pub(super) async fn apply_choke_round(
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
            workers.apply_choke_action(actor_id, choke).await;
        }
    }
    if let Some(identity) = optimistic
        && let Some(index) = peers.peer_index(identity)
        && let Some(actor_id) = peers.actor_id(index)
    {
        workers.apply_choke_action(actor_id, false).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::pipelined::BlockRequest;
    use crate::engine::bt_upload_session::{
        BtSeedingConfig, InMemoryPieceProvider, PieceDataProvider,
    };
    use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
    use aria2_protocol::bittorrent::peer::connection::PeerConnection;

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

        timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .expect("peer event channel closed")
    }

    #[tokio::test]
    async fn peer_actor_sends_startup_allowed_fast_message() {
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let mut connection = BtPeerConn::from_incoming_plain(local, endpoint);
        connection.configure_upload_with_auto_unchoke(
            &BtSeedingConfig::default(),
            crate::rate_limiter::RateLimiter::unlimited(),
            1,
            16,
            false,
        );
        connection.actor_startup = Some(crate::engine::bt_peer_connection::PeerActorStartup {
            peer_agent: "test-peer".to_string(),
            listen_port: None,
            dht_enabled: false,
            allowed_fast: vec![3],
        });

        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let provider = Arc::new(InMemoryPieceProvider::new(16, 1));
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
        let _extension_handshake = read_message_while_actor_runs(&mut remote).await;
        let _availability = read_message_while_actor_runs(&mut remote).await;
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
    async fn peer_actor_cancels_inflight_request_before_orderly_shutdown() {
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let mut connection = BtPeerConn::from_incoming_plain(local, endpoint);
        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = mpsc::channel(8);
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
        command_tx
            .send(PeerCommand::BeginGeneration {
                generation,
                piece_index: 3,
            })
            .await
            .unwrap();
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
    async fn peer_actor_publishes_target_piece_availability_for_active_generation() {
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let mut connection = BtPeerConn::from_incoming_plain(local, endpoint);
        connection.allocate_session_resource(16 * 1024, 8, 8 * 16 * 1024);
        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = mpsc::channel(8);
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
        command_tx
            .send(PeerCommand::BeginGeneration {
                generation,
                piece_index: 7,
            })
            .await
            .unwrap();
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
        assert!(matches!(
            timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            PeerEvent::AvailabilityChanged {
                actor_id: event_actor_id,
                generation: event_generation,
                has_piece: true,
            } if event_actor_id == actor_id && event_generation == generation
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
        let mut connection = BtPeerConn::from_incoming_plain(local, endpoint);
        connection.allocate_session_resource(16, 8, 128);
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 8));
        let mut swarm = PeerSwarm::new(16);
        assert!(swarm.spawn_peer(connection, None, provider).is_ok());
        let mut workers = PeerGeneration::from_swarm(&swarm, 2).await;
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
            .request(first_generation, 2, first_request)
            .await
            .unwrap();
        assert_eq!(
            read_message_while_actor_runs(&mut remote).await,
            BtMessage::Request {
                request: PieceBlockRequest::new(2, 0, 16),
            }
        );

        workers.advance_generation(5).await;
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
            .request(retry_generation, 5, first_request)
            .await
            .unwrap();
        assert_eq!(
            read_message_while_actor_runs(&mut remote).await,
            BtMessage::Request {
                request: PieceBlockRequest::new(5, 0, 16),
            }
        );
        remote
            .send_message(&BtMessage::Piece {
                index: 5,
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
                message: BtMessage::Piece { index: 5, .. },
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
        let mut connection = BtPeerConn::from_incoming_plain(local, endpoint);
        connection.allocate_session_resource(16 * 1024, 8, 8 * 16 * 1024);
        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = mpsc::channel(8);
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
            piece_index: 2,
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
        let connection = BtPeerConn::from_incoming_plain(local, endpoint);
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
        control
            .send(PeerCommand::BeginGeneration {
                generation,
                piece_index: 5,
            })
            .await
            .unwrap();
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
    async fn peer_actor_survives_piece_generation_rollover_and_drops_late_blocks() {
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
        let connection = BtPeerConn::from_incoming_plain(local, endpoint);
        let actor_id = connection.actor_id;
        let (command_tx, command_rx) = mpsc::channel(8);
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
        command_tx
            .send(PeerCommand::BeginGeneration {
                generation: first_generation,
                piece_index: 3,
            })
            .await
            .unwrap();
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

        command_tx
            .send(PeerCommand::EndGeneration {
                generation: first_generation,
                piece_index: 3,
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
                request: PieceBlockRequest::new(3, 0, 16),
            }
        );
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
        command_tx
            .send(PeerCommand::BeginGeneration {
                generation: next_generation,
                piece_index: 4,
            })
            .await
            .unwrap();
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
        assert!(matches!(
            timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
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
        let mut connection = BtPeerConn::from_incoming_plain(local, endpoint);
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
        let (command_tx, command_rx) = mpsc::channel(8);
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
        assert!(matches!(
            timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            PeerEvent::UploadQueueChanged { actor_id: event_actor_id, snapshot }
                if event_actor_id == actor_id && snapshot.outstanding_upload_count == 1
        ));
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
}

//! Single-owner connection workers shared by normal and endgame schedulers.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::trace;

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::choking_algorithm::{ChokingAlgorithm, IdentityChokeAction, PeerIdentity};
use crate::error::Result;

use super::super::BtMessageHandler;
use super::super::types::DEFAULT_MAX_OUTSTANDING_REQUEST;
use super::pipelined::BlockRequest;

pub(crate) enum PeerCommand {
    Request {
        piece_index: u32,
        request: BlockRequest,
    },
    Cancel {
        piece_index: u32,
        request: BlockRequest,
    },
    ChokeUpload,
    UnchokeUpload,
    Shutdown,
}

pub(crate) enum PeerEvent {
    Message {
        peer_index: usize,
        message: aria2_protocol::bittorrent::message::types::BtMessage,
    },
    InterestChanged {
        peer_index: usize,
        snapshot: Box<crate::engine::peer_stats::PeerStats>,
    },
    UploadBytes {
        peer_index: usize,
        snapshot: Box<crate::engine::peer_stats::PeerStats>,
    },
    RequestFailed {
        peer_index: usize,
        request: BlockRequest,
    },
    Disconnected {
        peer_index: usize,
    },
}

/// Bounded command endpoint shared by download workers and long-lived seed actors.
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
}

/// Tokio-owned peer worker used when the connection lifetime outlives one
/// piece-transfer future.
pub(crate) struct PeerActorTask {
    pub(crate) control: PeerActorControl,
    task: Option<JoinHandle<BtPeerConn>>,
}

impl PeerActorTask {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_owned(
        peer_index: usize,
        mut connection: BtPeerConn,
        event_tx: mpsc::Sender<PeerEvent>,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        upload_provider: Option<Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>>,
        command_capacity: usize,
    ) -> Self {
        let (control, command_rx) = PeerActorControl::channel(command_capacity);
        let task = tokio::spawn(async move {
            let _ = peer_worker(
                peer_index,
                &mut connection,
                command_rx,
                event_tx,
                dht_engine,
                upload_provider,
            )
            .await;
            connection
        });
        Self {
            control,
            task: Some(task),
        }
    }

    pub(crate) async fn shutdown(
        &mut self,
    ) -> std::result::Result<BtPeerConn, tokio::task::JoinError> {
        let _ = self.control.send(PeerCommand::Shutdown).await;
        self.task
            .take()
            .expect("peer actor task can only be shut down once")
            .await
    }
}

impl Drop for PeerActorTask {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub(super) type WorkerFuture<'a> = Pin<Box<dyn Future<Output = usize> + Send + 'a>>;

/// Owns one local worker future per connection while a piece is in flight.
///
/// Workers borrow connections rather than moving them into detached tasks, so
/// cancellation cannot silently remove a live connection from the session.
pub(super) struct PeerWorkers<'a> {
    pub(super) senders: Vec<Option<PeerActorControl>>,
    pub(super) workers: FuturesUnordered<WorkerFuture<'a>>,
}

impl<'a> PeerWorkers<'a> {
    pub(super) fn new(
        connections: &'a mut [BtPeerConn],
        event_tx: mpsc::Sender<PeerEvent>,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        upload_provider: Option<Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>>,
    ) -> Self {
        let mut senders = Vec::with_capacity(connections.len());
        let workers = FuturesUnordered::new();

        for (peer_index, connection) in connections.iter_mut().enumerate() {
            let (command_tx, command_rx) =
                PeerActorControl::channel(DEFAULT_MAX_OUTSTANDING_REQUEST.saturating_mul(2).max(8));
            senders.push(Some(command_tx));
            let worker: WorkerFuture<'a> = Box::pin(peer_worker(
                peer_index,
                connection,
                command_rx,
                event_tx.clone(),
                dht_engine.clone(),
                upload_provider.clone(),
            ));
            workers.push(worker);
        }

        Self { senders, workers }
    }

    /// Stop a peer after its in-flight requests have been requeued.
    pub(super) fn stop_peer(
        &mut self,
        peer_index: usize,
        requests: &[BlockRequest],
        piece_index: u32,
    ) {
        let Some(sender) = self.senders.get_mut(peer_index).and_then(Option::take) else {
            return;
        };

        for request in requests {
            let _ = sender.try_send(PeerCommand::Cancel {
                piece_index,
                request: *request,
            });
        }
        let _ = sender.try_send(PeerCommand::Shutdown);
    }

    pub(super) async fn apply_choke_action(&mut self, peer_index: usize, choke: bool) -> bool {
        let Some(sender) = self.senders.get(peer_index).and_then(Option::as_ref) else {
            return false;
        };
        let command = if choke {
            PeerCommand::ChokeUpload
        } else {
            PeerCommand::UnchokeUpload
        };
        sender.send(command).await.is_ok()
    }

    /// Gracefully stop workers before returning connection ownership.
    pub(super) async fn shutdown(&mut self, event_rx: &mut mpsc::Receiver<PeerEvent>) {
        for sender in self.senders.iter().flatten() {
            let _ = sender.try_send(PeerCommand::Shutdown);
        }
        self.senders.fill(None);

        while !self.workers.is_empty() {
            tokio::select! {
                _ = self.workers.next() => {}
                event = event_rx.recv() => {
                    if event.is_none() {
                        break;
                    }
                }
            }
        }
    }
}

pub(crate) async fn peer_worker(
    peer_index: usize,
    connection: &mut BtPeerConn,
    mut command_rx: mpsc::Receiver<PeerCommand>,
    event_tx: mpsc::Sender<PeerEvent>,
    dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    upload_provider: Option<Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>>,
) -> usize {
    loop {
        tokio::select! {
            biased;
            command = command_rx.recv() => {
                match command {
                    Some(PeerCommand::Request { piece_index, request }) => {
                        if connection.send_request(request.message(piece_index)).await.is_err() {
                            let _ = event_tx.send(PeerEvent::RequestFailed {
                                peer_index,
                                request,
                            }).await;
                            break;
                        }
                    }
                    Some(PeerCommand::Cancel { piece_index, request }) => {
                        let _ = connection.send_cancel(&request.message(piece_index)).await;
                    }
                    Some(PeerCommand::ChokeUpload) => {
                        if let Err(error) = connection.choke_upload_peer().await {
                            tracing::debug!(peer_index, %error, "Failed to choke BT upload peer");
                        }
                    }
                    Some(PeerCommand::UnchokeUpload) => {
                        if let Err(error) = connection.unchoke_upload_peer().await {
                            tracing::debug!(peer_index, %error, "Failed to unchoke BT upload peer");
                        }
                    }
                    Some(PeerCommand::Shutdown) | None => break,
                }
            }
            message = connection.read_message() => {
                match message {
                    Ok(Some(message)) => {
                        let was_interested = connection.stats.peer_interested;
                        let (message, uploaded_bytes) = match process_peer_message(
                            connection,
                            message,
                            dht_engine.clone(),
                            upload_provider.as_deref(),
                        ).await {
                            Ok(message) => message,
                            Err(error) => {
                                tracing::debug!(peer_index, %error, "BT upload request handling failed");
                                let _ = event_tx.send(PeerEvent::Disconnected { peer_index }).await;
                                break;
                            }
                        };
                        if uploaded_bytes > 0
                            && event_tx.send(PeerEvent::UploadBytes {
                                peer_index,
                                snapshot: Box::new(connection.stats.clone()),
                            }).await.is_err()
                        {
                            break;
                        }
                        if connection.stats.peer_interested != was_interested
                            && event_tx.send(PeerEvent::InterestChanged {
                                peer_index,
                                snapshot: Box::new(connection.stats.clone()),
                            }).await.is_err()
                        {
                            break;
                        }
                        let Some(message) = message else { continue; };
                        if event_tx.send(PeerEvent::Message { peer_index, message }).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) | Err(_) => {
                        let _ = event_tx.send(PeerEvent::Disconnected { peer_index }).await;
                        break;
                    }
                }
            }
        }
    }

    peer_index
}

async fn process_peer_message(
    connection: &mut BtPeerConn,
    message: aria2_protocol::bittorrent::message::types::BtMessage,
    dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    upload_provider: Option<&dyn crate::engine::bt_upload_session::PieceDataProvider>,
) -> Result<(
    Option<aria2_protocol::bittorrent::message::types::BtMessage>,
    u64,
)> {
    use aria2_protocol::bittorrent::message::types::BtMessage;

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
        BtMessage::AllowedFast { index } => {
            connection.add_allowed_fast(index);
            Ok((None, uploaded_bytes))
        }
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
                BtMessageHandler::try_process_pex_during_read(connection, ext_id, &payload);
            }
            Ok((None, uploaded_bytes))
        }
        BtMessage::Have { piece_index } => {
            connection.update_peer_bitfield(piece_index as usize, 1);
            Ok((None, uploaded_bytes))
        }
        BtMessage::Bitfield { data } => {
            connection.set_peer_bitfield(&data);
            Ok((None, uploaded_bytes))
        }
        BtMessage::HaveAll => {
            connection.mark_seeder();
            Ok((None, uploaded_bytes))
        }
        BtMessage::HaveNone => {
            connection.seeder = false;
            connection.set_peer_bitfield(&[]);
            Ok((None, uploaded_bytes))
        }
        BtMessage::Choke => {
            connection.stats.peer_choking = true;
            Ok((None, uploaded_bytes))
        }
        BtMessage::Unchoke => {
            connection.stats.peer_choking = false;
            Ok((None, uploaded_bytes))
        }
        _ => Ok((None, uploaded_bytes)),
    }
}

pub(super) async fn apply_interest_change(
    workers: &mut PeerWorkers<'_>,
    peer_indices: &HashMap<PeerIdentity, usize>,
    choking_algo: Option<&mut ChokingAlgorithm>,
    snapshot: crate::engine::peer_stats::PeerStats,
) {
    let Some(algo) = choking_algo else { return };
    algo.sync_peer_by_identity(&snapshot);
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
            && let Some(&index) = peer_indices.get(&identity)
        {
            workers.apply_choke_action(index, choke).await;
        }
    }
    if let Some(identity) = optimistic
        && let Some(&index) = peer_indices.get(&identity)
    {
        workers.apply_choke_action(index, false).await;
    }
}

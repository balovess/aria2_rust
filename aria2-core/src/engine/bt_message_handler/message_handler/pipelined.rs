//! Bounded, event-driven block pipeline for normal BitTorrent pieces.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use crate::constants;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::AtomicProgress;

use super::super::types::{
    BLOCK_SIZE, DEFAULT_MAX_OUTSTANDING_REQUEST, PeerDownloadBytes, PieceDownloadResult,
};
use super::BtMessageHandler;

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{block_request_deadline, piece_attempt_budget_exhausted};
    use crate::engine::bt_message_handler::BLOCK_REQUEST_TIMEOUT_SECS;

    #[test]
    fn configured_bt_request_timeout_controls_pending_deadline() {
        let sent_at = Instant::now();
        let configured = Duration::from_secs(61);

        assert_eq!(
            block_request_deadline(sent_at, configured),
            sent_at + configured
        );
        assert_ne!(
            block_request_deadline(sent_at, configured),
            sent_at + Duration::from_secs(BLOCK_REQUEST_TIMEOUT_SECS)
        );
    }

    #[test]
    fn piece_attempt_budget_uses_total_attempts_and_zero_is_unlimited() {
        assert!(piece_attempt_budget_exhausted(1, 1));
        assert!(!piece_attempt_budget_exhausted(1, 3));
        assert!(piece_attempt_budget_exhausted(3, 3));
        assert!(!piece_attempt_budget_exhausted(100, 0));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockRequest {
    block_index: u32,
    offset: u32,
    length: u32,
}

impl BlockRequest {
    fn message(
        self,
        piece_index: u32,
    ) -> aria2_protocol::bittorrent::message::types::PieceBlockRequest {
        aria2_protocol::bittorrent::message::types::PieceBlockRequest {
            index: piece_index,
            begin: self.offset,
            length: self.length,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingRequest {
    request: BlockRequest,
    peer_index: usize,
    sent_at: Instant,
}

fn block_request_deadline(sent_at: Instant, timeout: Duration) -> Instant {
    sent_at + timeout
}

fn piece_attempt_budget_exhausted(attempts: u32, max_attempts: u32) -> bool {
    max_attempts != 0 && attempts >= max_attempts
}

enum PeerCommand {
    Request {
        piece_index: u32,
        request: BlockRequest,
    },
    Cancel {
        piece_index: u32,
        request: BlockRequest,
    },
    Shutdown,
}

enum PeerEvent {
    Message {
        peer_index: usize,
        message: aria2_protocol::bittorrent::message::types::BtMessage,
    },
    RequestFailed {
        peer_index: usize,
        request: BlockRequest,
    },
    Disconnected {
        peer_index: usize,
    },
}

type WorkerFuture<'a> = Pin<Box<dyn Future<Output = usize> + Send + 'a>>;

/// Owns one local worker future per connection while a piece is in flight.
///
/// The futures borrow the caller's connections instead of moving them into
/// spawned tasks. This keeps cancellation safe: dropping the piece future
/// cannot silently remove connections from the active peer list.
struct PeerWorkers<'a> {
    senders: Vec<Option<mpsc::Sender<PeerCommand>>>,
    workers: FuturesUnordered<WorkerFuture<'a>>,
}

impl<'a> PeerWorkers<'a> {
    fn new(
        connections: &'a mut [BtPeerConn],
        event_tx: mpsc::Sender<PeerEvent>,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    ) -> Self {
        let mut senders = Vec::with_capacity(connections.len());
        let workers = FuturesUnordered::new();

        for (peer_index, connection) in connections.iter_mut().enumerate() {
            let (command_tx, command_rx) =
                mpsc::channel(DEFAULT_MAX_OUTSTANDING_REQUEST.saturating_mul(2).max(8));
            senders.push(Some(command_tx));
            let worker: WorkerFuture<'a> = Box::pin(peer_worker(
                peer_index,
                connection,
                command_rx,
                event_tx.clone(),
                dht_engine.clone(),
            ));
            workers.push(worker);
        }

        Self { senders, workers }
    }

    /// Stop a peer after all of its in-flight requests have been requeued.
    fn stop_peer(&mut self, peer_index: usize, requests: &[BlockRequest], piece_index: u32) {
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

    /// Gracefully stop all worker futures before returning to the peer loop.
    async fn shutdown(&mut self, event_rx: &mut mpsc::Receiver<PeerEvent>) {
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

async fn peer_worker(
    peer_index: usize,
    connection: &mut BtPeerConn,
    mut command_rx: mpsc::Receiver<PeerCommand>,
    event_tx: mpsc::Sender<PeerEvent>,
    dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
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
                    Some(PeerCommand::Shutdown) | None => break,
                }
            }
            message = connection.read_message() => {
                match message {
                    Ok(Some(message)) => {
                        let Some(message) = process_peer_message(
                            connection,
                            message,
                            dht_engine.clone(),
                        ) else {
                            continue;
                        };
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

/// Apply connection-local protocol state while the worker owns the mutable
/// connection. Only messages that belong to the current piece pipeline are
/// returned to the scheduler.
fn process_peer_message(
    connection: &mut BtPeerConn,
    message: aria2_protocol::bittorrent::message::types::BtMessage,
    dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
) -> Option<aria2_protocol::bittorrent::message::types::BtMessage> {
    use aria2_protocol::bittorrent::message::types::BtMessage;

    match message {
        BtMessage::Piece { .. } | BtMessage::Reject { .. } => Some(message),
        BtMessage::AllowedFast { index } => {
            connection.add_allowed_fast(index);
            None
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
            None
        }
        BtMessage::Extended { ext_id, payload } => {
            if connection.is_pex_enabled()
                && ext_id != 0
                && connection.peer_extension_id("ut_pex") == Some(ext_id)
            {
                BtMessageHandler::try_process_pex_during_read(connection, ext_id, &payload);
            }
            None
        }
        BtMessage::Have { piece_index } => {
            connection.update_peer_bitfield(piece_index as usize, 1);
            None
        }
        BtMessage::Bitfield { data } => {
            connection.set_peer_bitfield(&data);
            None
        }
        BtMessage::HaveAll => {
            connection.mark_seeder();
            None
        }
        BtMessage::HaveNone => {
            connection.seeder = false;
            connection.set_peer_bitfield(&[]);
            None
        }
        BtMessage::Choke => {
            connection.stats.peer_choking = true;
            None
        }
        BtMessage::Unchoke => {
            connection.stats.peer_choking = false;
            None
        }
        _ => None,
    }
}

#[derive(Default)]
struct AttemptOutcome {
    data: Option<Vec<u8>>,
    peer_bytes: Vec<PeerDownloadBytes>,
    failed_peers: Vec<SocketAddr>,
}

fn select_peer(
    cursor: &mut usize,
    in_flight: &[usize],
    dead: &[bool],
    has_piece: &[bool],
) -> Option<usize> {
    if in_flight.is_empty() {
        return None;
    }

    let prefer_known_piece = has_piece
        .iter()
        .enumerate()
        .any(|(index, &known)| known && !dead[index]);

    for step in 0..in_flight.len() {
        let index = (*cursor + step) % in_flight.len();
        if dead[index] || in_flight[index] >= DEFAULT_MAX_OUTSTANDING_REQUEST {
            continue;
        }
        if prefer_known_piece && !has_piece[index] {
            continue;
        }
        *cursor = (index + 1) % in_flight.len();
        return Some(index);
    }

    None
}

#[allow(clippy::too_many_arguments)]
async fn fill_request_window(
    workers: &mut PeerWorkers<'_>,
    remaining: &mut VecDeque<BlockRequest>,
    pending: &mut HashMap<(u32, u32), PendingRequest>,
    in_flight: &mut [usize],
    dead: &mut [bool],
    has_piece: &[bool],
    peer_cursor: &mut usize,
    piece_index: u32,
    peer_addresses: &[Option<SocketAddr>],
    failed_peers: &mut Vec<SocketAddr>,
) {
    while let Some(request) = remaining.pop_front() {
        let Some(peer_index) = select_peer(peer_cursor, in_flight, dead, has_piece) else {
            remaining.push_front(request);
            break;
        };

        let Some(sender) = workers
            .senders
            .get(peer_index)
            .and_then(Option::as_ref)
            .cloned()
        else {
            dead[peer_index] = true;
            remaining.push_front(request);
            continue;
        };

        if sender
            .send(PeerCommand::Request {
                piece_index,
                request,
            })
            .await
            .is_err()
        {
            dead[peer_index] = true;
            workers.stop_peer(peer_index, &[], piece_index);
            if let Some(address) = peer_addresses[peer_index]
                && !failed_peers.contains(&address)
            {
                failed_peers.push(address);
            }
            remaining.push_front(request);
            continue;
        }

        in_flight[peer_index] += 1;
        pending.insert(
            (piece_index, request.offset),
            PendingRequest {
                request,
                peer_index,
                sent_at: Instant::now(),
            },
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn mark_peer_failed(
    peer_index: usize,
    workers: &mut PeerWorkers<'_>,
    pending: &mut HashMap<(u32, u32), PendingRequest>,
    remaining: &mut VecDeque<BlockRequest>,
    in_flight: &mut [usize],
    dead: &mut [bool],
    piece_index: u32,
    peer_address: Option<SocketAddr>,
    failed_peers: &mut Vec<SocketAddr>,
) {
    if dead[peer_index] {
        return;
    }
    dead[peer_index] = true;

    let entries = std::mem::take(pending);
    let mut retry = Vec::new();
    for (key, entry) in entries {
        if entry.peer_index == peer_index {
            retry.push(entry.request);
        } else {
            pending.insert(key, entry);
        }
    }
    retry.sort_by_key(|request| request.block_index);
    in_flight[peer_index] = 0;
    for request in retry.iter().rev() {
        remaining.push_front(*request);
    }
    workers.stop_peer(peer_index, &retry, piece_index);

    if let Some(address) = peer_address
        && !failed_peers.contains(&address)
    {
        failed_peers.push(address);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_attempt(
    workers: &mut PeerWorkers<'_>,
    event_rx: &mut mpsc::Receiver<PeerEvent>,
    piece_index: u32,
    piece_length: u32,
    num_blocks: u32,
    peer_addresses: &[Option<SocketAddr>],
    has_piece: &[bool],
    network_activity: Option<&AtomicProgress>,
    request_timeout: Duration,
) -> AttemptOutcome {
    let block_count = piece_length.div_ceil(BLOCK_SIZE);
    if num_blocks != block_count {
        warn!(
            piece_index,
            requested_blocks = num_blocks,
            actual_blocks = block_count,
            "BT block count disagrees with piece length; using the actual layout"
        );
    }

    let mut remaining = (0..block_count)
        .map(|block_index| {
            let offset = block_index * BLOCK_SIZE;
            BlockRequest {
                block_index,
                offset,
                length: (piece_length - offset).min(BLOCK_SIZE),
            }
        })
        .collect::<VecDeque<_>>();
    let mut pending = HashMap::<(u32, u32), PendingRequest>::new();
    let mut in_flight = vec![0usize; peer_addresses.len()];
    let mut dead = vec![false; peer_addresses.len()];
    let mut completed = vec![false; block_count as usize];
    let mut piece_data = vec![0u8; piece_length as usize];
    let mut peer_cursor = 0usize;
    let mut completed_blocks = 0u32;
    let mut peer_bytes = Vec::new();
    let mut failed_peers = Vec::new();

    loop {
        fill_request_window(
            workers,
            &mut remaining,
            &mut pending,
            &mut in_flight,
            &mut dead,
            has_piece,
            &mut peer_cursor,
            piece_index,
            peer_addresses,
            &mut failed_peers,
        )
        .await;

        if completed_blocks == block_count {
            return AttemptOutcome {
                data: Some(piece_data),
                peer_bytes,
                failed_peers,
            };
        }

        if pending.is_empty()
            && (remaining.is_empty()
                || select_peer(&mut peer_cursor, &in_flight, &dead, has_piece).is_none())
        {
            break;
        }

        let next_deadline = pending
            .values()
            .map(|request| block_request_deadline(request.sent_at, request_timeout))
            .min()
            .unwrap_or_else(|| block_request_deadline(Instant::now(), request_timeout));
        let wait = next_deadline.saturating_duration_since(Instant::now());
        let workers_active = !workers.workers.is_empty();

        tokio::select! {
            event = event_rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    PeerEvent::Message { peer_index, message } => {
                        use aria2_protocol::bittorrent::message::types::BtMessage;
                        match message {
                            BtMessage::Piece { index, begin, data } if index == piece_index => {
                                let key = (index, begin);
                                let Some(entry) = pending.remove(&key) else {
                                    debug!(piece_index = index, offset = begin, "Ignoring unsolicited BT piece block");
                                    continue;
                                };
                                if entry.peer_index != peer_index {
                                    pending.insert(key, entry);
                                    continue;
                                }
                                in_flight[peer_index] = in_flight[peer_index].saturating_sub(1);
                                if data.len() != entry.request.length as usize {
                                    warn!(
                                        piece_index,
                                        offset = begin,
                                        expected = entry.request.length,
                                        actual = data.len(),
                                        "Discarding BT block with unexpected length"
                                    );
                                    remaining.push_front(entry.request);
                                    mark_peer_failed(
                                        peer_index,
                                        workers,
                                        &mut pending,
                                        &mut remaining,
                                        &mut in_flight,
                                        &mut dead,
                                        piece_index,
                                        peer_addresses[peer_index],
                                        &mut failed_peers,
                                    );
                                    continue;
                                }
                                let start = entry.request.offset as usize;
                                let end = start + data.len();
                                if end > piece_data.len() || completed[entry.request.block_index as usize] {
                                    continue;
                                }
                                if !data.is_empty()
                                    && let Some(progress) = network_activity
                                {
                                    progress.record_network_activity();
                                }
                                piece_data[start..end].copy_from_slice(&data);
                                completed[entry.request.block_index as usize] = true;
                                completed_blocks += 1;

                                if let Some(address) = peer_addresses[peer_index] {
                                    let bytes = data.len() as u64;
                                    if let Some(entry) = peer_bytes.iter_mut().find(|entry| entry.peer_index == peer_index) {
                                        entry.bytes += bytes;
                                    } else {
                                        peer_bytes.push(PeerDownloadBytes { peer_index, peer: address, bytes });
                                    }
                                }
                            }
                            BtMessage::Reject { index, offset, .. }
                                if index == piece_index
                                    && pending.contains_key(&(index, offset)) =>
                            {
                                mark_peer_failed(
                                    peer_index,
                                    workers,
                                    &mut pending,
                                    &mut remaining,
                                    &mut in_flight,
                                    &mut dead,
                                    piece_index,
                                    peer_addresses[peer_index],
                                    &mut failed_peers,
                                );
                            }
                            _ => {}
                        }
                    }
                    PeerEvent::RequestFailed { peer_index, request } => {
                        debug!(peer_index, offset = request.offset, "BT request send failed");
                        mark_peer_failed(
                            peer_index,
                            workers,
                            &mut pending,
                            &mut remaining,
                            &mut in_flight,
                            &mut dead,
                            piece_index,
                            peer_addresses[peer_index],
                            &mut failed_peers,
                        );
                    }
                    PeerEvent::Disconnected { peer_index } => {
                        debug!(peer_index, "BT peer disconnected during pipelined piece download");
                        mark_peer_failed(
                            peer_index,
                            workers,
                            &mut pending,
                            &mut remaining,
                            &mut in_flight,
                            &mut dead,
                            piece_index,
                            peer_addresses[peer_index],
                            &mut failed_peers,
                        );
                    }
                }
            }
            worker = workers.workers.next(), if workers_active => {
                if let Some(peer_index) = worker {
                    mark_peer_failed(
                        peer_index,
                        workers,
                        &mut pending,
                        &mut remaining,
                        &mut in_flight,
                        &mut dead,
                        piece_index,
                        peer_addresses[peer_index],
                        &mut failed_peers,
                    );
                }
            }
            _ = tokio::time::sleep(wait) => {
                let now = Instant::now();
                let expired = pending
                    .values()
                    .filter(|request| now >= block_request_deadline(request.sent_at, request_timeout))
                    .map(|request| request.peer_index)
                    .collect::<HashSet<_>>();
                for peer_index in expired {
                    warn!(peer_index, "BT block request window timed out");
                    mark_peer_failed(
                        peer_index,
                        workers,
                        &mut pending,
                        &mut remaining,
                        &mut in_flight,
                        &mut dead,
                        piece_index,
                        peer_addresses[peer_index],
                        &mut failed_peers,
                    );
                }
            }
        }
    }

    AttemptOutcome {
        data: None,
        peer_bytes,
        failed_peers,
    }
}

impl BtMessageHandler {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn download_piece_blocks_pipelined_with_sources_and_activity(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        network_activity: Option<&AtomicProgress>,
        request_timeout: Duration,
        max_attempts: u32,
    ) -> Result<PieceDownloadResult> {
        let mut attempts: u32 = 0;
        loop {
            attempts = attempts.saturating_add(1);
            info!(
                "[BT] Pipelined piece download attempt {} for piece {}",
                attempts, piece_index
            );

            let peer_addresses = connections
                .iter()
                .map(|connection| {
                    connection.remote_endpoint().or_else(|| {
                        let ip = connection.ip_addr.parse().ok()?;
                        Some(SocketAddr::new(ip, connection.port))
                    })
                })
                .collect::<Vec<_>>();
            let has_piece = connections
                .iter()
                .map(|connection| connection.seeder || connection.has_piece(piece_index as usize))
                .collect::<Vec<_>>();
            let channel_capacity = connections
                .len()
                .saturating_mul(DEFAULT_MAX_OUTSTANDING_REQUEST + 4)
                .max(64);
            let (event_tx, mut event_rx) = mpsc::channel(channel_capacity);
            let mut workers = PeerWorkers::new(connections, event_tx, dht_engine.clone());
            let outcome = run_attempt(
                &mut workers,
                &mut event_rx,
                piece_index,
                piece_length,
                num_blocks,
                &peer_addresses,
                &has_piece,
                network_activity,
                request_timeout,
            )
            .await;
            workers.shutdown(&mut event_rx).await;
            drop(workers);

            if let Some(data) = outcome.data {
                info!(
                    piece_index,
                    blocks = num_blocks,
                    bytes = data.len(),
                    "BT pipelined piece completed"
                );
                return Ok(PieceDownloadResult {
                    data,
                    peer_bytes: outcome.peer_bytes,
                    failed_peers: outcome.failed_peers,
                });
            }

            if !piece_attempt_budget_exhausted(attempts, max_attempts) {
                tokio::time::sleep(Duration::from_millis(constants::BT_RETRY_DELAY_MS)).await;
            } else {
                break;
            }
        }

        Err(Aria2Error::Fatal(FatalError::Config(format!(
            "Failed to download piece {} after {} pipelined attempts",
            piece_index,
            if max_attempts == 0 {
                attempts
            } else {
                max_attempts
            }
        ))))
    }
}

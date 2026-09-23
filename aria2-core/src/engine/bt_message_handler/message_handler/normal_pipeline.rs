//! Request-window scheduling for ordinary BT pieces.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use crate::engine::choking_algorithm::{ChokingAlgorithm, PeerIdentity};
use crate::request::request_group::AtomicProgress;

use super::super::types::{BLOCK_SIZE, DEFAULT_MAX_OUTSTANDING_REQUEST, PeerDownloadBytes};
use super::peer_worker::{PeerCommand, PeerEvent, PeerWorkers, apply_interest_change};
use super::pipelined::BlockRequest;

#[derive(Debug, Clone, Copy)]
struct PendingRequest {
    request: BlockRequest,
    peer_index: usize,
    sent_at: Instant,
}

pub(super) fn block_request_deadline(sent_at: Instant, timeout: Duration) -> Instant {
    sent_at + timeout
}

pub(super) fn piece_attempt_budget_exhausted(attempts: u32, max_attempts: u32) -> bool {
    max_attempts != 0 && attempts >= max_attempts
}

#[derive(Default)]
pub(super) struct AttemptOutcome {
    pub(super) data: Option<Vec<u8>>,
    pub(super) peer_bytes: Vec<PeerDownloadBytes>,
    pub(super) failed_peers: Vec<SocketAddr>,
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
pub(super) async fn run_attempt(
    workers: &mut PeerWorkers<'_>,
    event_rx: &mut mpsc::Receiver<PeerEvent>,
    piece_index: u32,
    piece_length: u32,
    num_blocks: u32,
    peer_addresses: &[Option<SocketAddr>],
    has_piece: &[bool],
    peer_indices: &HashMap<PeerIdentity, usize>,
    choking_algo: Option<&mut ChokingAlgorithm>,
    network_activity: Option<&AtomicProgress>,
    request_timeout: Duration,
) -> AttemptOutcome {
    let mut choking_algo = choking_algo;
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
                    PeerEvent::UploadBytes { actor_id, .. } => {
                        if workers.peer_index(actor_id).is_none() {
                            continue;
                        }
                    }
                    PeerEvent::InterestChanged { actor_id, snapshot } => {
                        if workers.peer_index(actor_id).is_none() {
                            continue;
                        }
                        apply_interest_change(
                            workers,
                            peer_indices,
                            choking_algo.as_deref_mut(),
                            *snapshot,
                        ).await;
                    }
                    PeerEvent::Message { actor_id, message } => {
                        let Some(peer_index) = workers.peer_index(actor_id) else {
                            continue;
                        };
                        trace!(actor_id = actor_id.0, peer_index, "Received BT peer event");
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
                    PeerEvent::RequestFailed { actor_id, request } => {
                        let Some(peer_index) = workers.peer_index(actor_id) else {
                            continue;
                        };
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
                    PeerEvent::Disconnected { actor_id } => {
                        let Some(peer_index) = workers.peer_index(actor_id) else {
                            continue;
                        };
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
                if let Some(actor_id) = worker
                    && let Some(peer_index) = workers.peer_index(actor_id)
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

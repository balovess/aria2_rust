//! Request-window scheduling for ordinary BT pieces.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tracing::{debug, trace, warn};

use crate::engine::choking_algorithm::ChokingAlgorithm;
use crate::request::request_group::AtomicProgress;

use super::super::types::{BLOCK_SIZE, DEFAULT_MAX_OUTSTANDING_REQUEST, PeerDownloadBytes};
use super::peer_actor::{PeerEvent, PeerGeneration, apply_choke_round, apply_interest_change};
use super::peer_registry::PeerSwarmEventLease;
use super::peer_snapshot::PeerSchedulingSnapshot;
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
    peers: &PeerSchedulingSnapshot,
) -> Option<usize> {
    if in_flight.is_empty() {
        return None;
    }

    let prefer_known_piece = peers
        .peers()
        .iter()
        .enumerate()
        .any(|(index, peer)| peer.has_piece && !dead[index]);

    for step in 0..in_flight.len() {
        let index = (*cursor + step) % in_flight.len();
        if dead[index] || in_flight[index] >= DEFAULT_MAX_OUTSTANDING_REQUEST {
            continue;
        }
        if prefer_known_piece && !peers.peers()[index].has_piece {
            continue;
        }
        *cursor = (index + 1) % in_flight.len();
        return Some(index);
    }

    None
}

#[allow(clippy::too_many_arguments)]
async fn fill_request_window(
    workers: &mut PeerGeneration,
    remaining: &mut VecDeque<BlockRequest>,
    pending: &mut HashMap<(u32, u32), PendingRequest>,
    in_flight: &mut [usize],
    dead: &mut [bool],
    peers: &PeerSchedulingSnapshot,
    peer_cursor: &mut usize,
    piece_index: u32,
    failed_peers: &mut Vec<SocketAddr>,
) {
    while let Some(request) = remaining.pop_front() {
        let Some(peer_index) = select_peer(peer_cursor, in_flight, dead, peers) else {
            remaining.push_front(request);
            break;
        };

        let Some(actor_id) = peers.actor_id(peer_index) else {
            dead[peer_index] = true;
            remaining.push_front(request);
            continue;
        };

        if !workers.request(actor_id, piece_index, request).await {
            dead[peer_index] = true;
            workers.cancel_peer_requests(actor_id, &[], piece_index);
            if let Some(address) = peers.peer(peer_index).and_then(|peer| peer.address)
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
    actor_id: crate::engine::bt_peer_connection::PeerActorId,
    workers: &mut PeerGeneration,
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
    workers.cancel_peer_requests(actor_id, &retry, piece_index);

    if let Some(address) = peer_address
        && !failed_peers.contains(&address)
    {
        failed_peers.push(address);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_attempt(
    workers: &mut PeerGeneration,
    event_rx: &mut PeerSwarmEventLease<'_>,
    piece_index: u32,
    piece_length: u32,
    num_blocks: u32,
    peers: &mut PeerSchedulingSnapshot,
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
    let mut in_flight = vec![0usize; peers.len()];
    let mut dead = vec![false; peers.len()];
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
            peers,
            &mut peer_cursor,
            piece_index,
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
                || select_peer(&mut peer_cursor, &in_flight, &dead, peers).is_none())
        {
            break;
        }

        let next_request_deadline = pending
            .values()
            .map(|request| block_request_deadline(request.sent_at, request_timeout))
            .min()
            .unwrap_or_else(|| block_request_deadline(Instant::now(), request_timeout));
        let choke_deadline = choking_algo
            .as_deref()
            .and_then(ChokingAlgorithm::next_choke_rotation_deadline);
        let next_deadline = choke_deadline.map_or(next_request_deadline, |deadline| {
            deadline.min(next_request_deadline)
        });
        let wait = next_deadline.saturating_duration_since(Instant::now());
        tokio::select! {
            event = event_rx.recv() => {
                let Some(event) = event else { break };
                match event {
                    PeerEvent::UploadBytes { actor_id, .. } => {
                        if peers.peer_index_by_actor_id(actor_id).is_none() {
                            continue;
                        }
                    }
                    PeerEvent::UploadQueueChanged { actor_id, .. } => {
                        if peers.peer_index_by_actor_id(actor_id).is_none() {
                            continue;
                        }
                    }
                    PeerEvent::ChokeStateChanged { actor_id, .. } => {
                        if peers.peer_index_by_actor_id(actor_id).is_none() {
                            continue;
                        }
                    }
                    PeerEvent::PeerChokingChanged { actor_id, .. } => {
                        if peers.peer_index_by_actor_id(actor_id).is_none() {
                            continue;
                        }
                    }
                    PeerEvent::AvailabilityChanged {
                        actor_id,
                        generation,
                        has_piece,
                    } => {
                        if generation == workers.generation() {
                            peers.update_peer_availability(actor_id, piece_index, has_piece);
                        }
                    }
                    PeerEvent::PeerAvailabilityChanged {
                        actor_id,
                        piece_index: changed_piece,
                        has_piece,
                    } => {
                        workers.record_availability_change(actor_id);
                        peers.update_peer_availability(actor_id, changed_piece, has_piece);
                    }
                    PeerEvent::PeerAvailabilitySnapshot {
                        actor_id, bitfield, ..
                    } => {
                        workers.record_availability_change(actor_id);
                        peers.update_peer_bitfield(actor_id, &bitfield);
                    }
                    PeerEvent::PexPeers { peers, .. } => workers.record_pex_peers(peers),
                    PeerEvent::InterestChanged { actor_id, snapshot } => {
                        if peers.peer_index_by_actor_id(actor_id).is_none() {
                            continue;
                        }
                        apply_interest_change(
                            workers,
                            peers,
                            choking_algo.as_deref_mut(),
                            *snapshot,
                        ).await;
                    }
                    PeerEvent::Message {
                        actor_id,
                        generation,
                        message,
                        ..
                    } => {
                        if generation != workers.generation() {
                            continue;
                        }
                        let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                            continue;
                        };
                        trace!(actor_id = actor_id.0, peer_index, "Received BT peer event");
                        use aria2_protocol::bittorrent::message::types::BtMessage;
                        if let BtMessage::Piece { data, .. } = &message
                            && let Some(identity) = peers.peer_identity(peer_index)
                            && let Some(algo) = choking_algo.as_deref_mut()
                        {
                            algo.on_data_received_by_identity(identity, data.len() as u64);
                        }
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
                                        peers.actor_id(peer_index).expect("peer snapshot has stable ID"),
                                        workers,
                                        &mut pending,
                                        &mut remaining,
                                        &mut in_flight,
                                        &mut dead,
                                        piece_index,
                                        peers.peer(peer_index).and_then(|peer| peer.address),
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

                                if let Some(address) = peers.peer(peer_index).and_then(|peer| peer.address) {
                                    let bytes = data.len() as u64;
                                    if let Some(entry) = peer_bytes.iter_mut().find(|entry| entry.peer_index == peer_index) {
                                        entry.bytes += bytes;
                                    } else {
                                        peer_bytes.push(PeerDownloadBytes {
                                            peer_index,
                                            peer: address,
                                            bytes,
                                        });
                                    }
                                }
                            }
                            BtMessage::Reject { index, offset, .. }
                                if index == piece_index
                                    && pending.contains_key(&(index, offset)) =>
                            {
                                mark_peer_failed(
                                    peer_index,
                                    peers.actor_id(peer_index).expect("peer snapshot has stable ID"),
                                    workers,
                                    &mut pending,
                                    &mut remaining,
                                    &mut in_flight,
                                    &mut dead,
                                    piece_index,
                                    peers.peer(peer_index).and_then(|peer| peer.address),
                                    &mut failed_peers,
                                );
                            }
                            _ => {}
                        }
                    }
                    PeerEvent::RequestFailed {
                        actor_id,
                        generation,
                        request,
                    } => {
                        if generation != workers.generation() {
                            continue;
                        }
                        let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                            continue;
                        };
                        debug!(peer_index, offset = request.offset, "BT request send failed");
                        mark_peer_failed(
                            peer_index,
                            actor_id,
                            workers,
                            &mut pending,
                            &mut remaining,
                            &mut in_flight,
                            &mut dead,
                            piece_index,
                            peers.peer(peer_index).and_then(|peer| peer.address),
                            &mut failed_peers,
                        );
                    }
                    PeerEvent::Disconnected { actor_id } => {
                        let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                            continue;
                        };
                        debug!(peer_index, "BT peer disconnected during pipelined piece download");
                        mark_peer_failed(
                            peer_index,
                            actor_id,
                            workers,
                            &mut pending,
                            &mut remaining,
                            &mut in_flight,
                            &mut dead,
                            piece_index,
                            peers.peer(peer_index).and_then(|peer| peer.address),
                            &mut failed_peers,
                        );
                    }
                }
            }
            _ = tokio::time::sleep(wait) => {
                let now = Instant::now();
                if choking_algo
                    .as_deref()
                    .is_some_and(|algo| algo.choke_rotation_due(now))
                {
                    apply_choke_round(workers, peers, choking_algo.as_deref_mut()).await;
                }
                let expired = pending
                    .values()
                    .filter(|request| now >= block_request_deadline(request.sent_at, request_timeout))
                    .map(|request| request.peer_index)
                    .collect::<HashSet<_>>();
                for peer_index in expired {
                    warn!(peer_index, "BT block request window timed out");
                    mark_peer_failed(
                        peer_index,
                        peers.actor_id(peer_index).expect("peer snapshot has stable ID"),
                        workers,
                        &mut pending,
                        &mut remaining,
                        &mut in_flight,
                        &mut dead,
                        piece_index,
                        peers.peer(peer_index).and_then(|peer| peer.address),
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

//! Peer request-window admission and retry bookkeeping for ordinary pieces.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use super::super::peer_actor::{PeerGeneration, TryRequestError};
use super::super::peer_snapshot::PeerSchedulingSnapshot;
use super::super::pipelined::BlockRequest;
use super::{BatchPieceState, BatchSchedule, PendingRequest};

fn select_peer(
    cursor: &mut usize,
    in_flight: &[usize],
    dead: &[bool],
    queue_blocked: &[bool],
    peers: &PeerSchedulingSnapshot,
    piece_index: u32,
) -> Option<usize> {
    if in_flight.is_empty() {
        return None;
    }

    for step in 0..in_flight.len() {
        let index = (*cursor + step) % in_flight.len();
        if dead[index] || queue_blocked[index] || in_flight[index] >= peers.request_window(index) {
            continue;
        }
        if !peers.has_piece(index, piece_index) || !peers.peer_can_request(index, piece_index) {
            continue;
        }
        *cursor = (index + 1) % in_flight.len();
        return Some(index);
    }

    None
}

pub(super) fn take_peer_pending_batch(
    peer_index: usize,
    pending: &mut HashMap<(u32, u32), PendingRequest>,
    in_flight: &mut [usize],
    mut include_piece: impl FnMut(u32) -> bool,
) -> Vec<(u32, BlockRequest)> {
    let keys = pending
        .iter()
        .filter_map(|(key, request)| {
            (request.peer_index == peer_index && include_piece(key.0)).then_some(*key)
        })
        .collect::<Vec<_>>();
    let mut requests = Vec::with_capacity(keys.len());
    for key @ (piece_index, _) in keys {
        if let Some(request) = pending.remove(&key) {
            in_flight[peer_index] = in_flight[peer_index].saturating_sub(1);
            requests.push((piece_index, request.request));
        }
    }
    requests.sort_by_key(|(piece_index, request)| (*piece_index, request.block_index));
    requests
}

pub(super) fn requeue_batch_requests(
    requests: &[(u32, BlockRequest)],
    states: &mut HashMap<u32, BatchPieceState>,
) {
    for (piece_index, request) in requests.iter().rev() {
        if let Some(state) = states.get_mut(piece_index) {
            state.remaining.push_front(*request);
        }
    }
}

pub(super) fn cancel_batch_requests(
    actor_id: crate::engine::bittorrent::peer::connection::PeerActorId,
    requests: &[(u32, BlockRequest)],
    workers: &PeerGeneration,
) {
    let mut by_piece = HashMap::<u32, Vec<BlockRequest>>::new();
    for &(piece_index, request) in requests {
        by_piece.entry(piece_index).or_default().push(request);
    }
    for (piece_index, requests) in by_piece {
        workers.cancel_peer_requests(actor_id, &requests, piece_index);
    }
}

fn add_failed_peer(state: &mut BatchPieceState, address: Option<SocketAddr>) {
    if let Some(address) = address
        && !state.failed_peers.contains(&address)
    {
        state.failed_peers.push(address);
    }
}

pub(super) fn mark_batch_peer_failed(
    peer_index: usize,
    actor_id: crate::engine::bittorrent::peer::connection::PeerActorId,
    target_piece: Option<u32>,
    workers: &PeerGeneration,
    schedule: &mut BatchSchedule,
    peers: &PeerSchedulingSnapshot,
) {
    let failed_address = peers.peer(peer_index).and_then(|peer| peer.address);
    if !schedule.dead[peer_index] {
        schedule.dead[peer_index] = true;
        let retry = take_peer_pending_batch(
            peer_index,
            &mut schedule.pending,
            &mut schedule.in_flight,
            |_| true,
        );
        requeue_batch_requests(&retry, &mut schedule.pieces);
        cancel_batch_requests(actor_id, &retry, workers);
        for (piece_index, _) in &retry {
            if let Some(state) = schedule.pieces.get_mut(piece_index) {
                add_failed_peer(state, failed_address);
            }
        }
    }
    if let Some(piece_index) = target_piece
        && let Some(state) = schedule.pieces.get_mut(&piece_index)
    {
        add_failed_peer(state, failed_address);
    } else if target_piece.is_none() {
        for state in schedule.pieces.values_mut() {
            add_failed_peer(state, failed_address);
        }
    }
}

pub(super) fn fill_batch_request_windows(
    workers: &mut PeerGeneration,
    schedule: &mut BatchSchedule,
    peers: &PeerSchedulingSnapshot,
) -> Vec<(
    crate::engine::bittorrent::peer::connection::PeerActorId,
    tokio::sync::watch::Receiver<u64>,
)> {
    if schedule.piece_order.is_empty() {
        return Vec::new();
    }

    let mut queue_blocked = vec![false; peers.len()];
    let mut queue_waiters = Vec::new();
    let mut stalled_pieces = 0usize;
    while stalled_pieces < schedule.piece_order.len() {
        let order_index = schedule.piece_cursor % schedule.piece_order.len();
        let piece_index = schedule.piece_order[order_index];
        schedule.piece_cursor = (order_index + 1) % schedule.piece_order.len();
        let Some(request) = schedule
            .pieces
            .get_mut(&piece_index)
            .and_then(|state| state.remaining.pop_front())
        else {
            stalled_pieces += 1;
            continue;
        };
        let Some(peer_index) = select_peer(
            &mut schedule.peer_cursor,
            &schedule.in_flight,
            &schedule.dead,
            &queue_blocked,
            peers,
            piece_index,
        ) else {
            if let Some(state) = schedule.pieces.get_mut(&piece_index) {
                state.remaining.push_front(request);
            }
            stalled_pieces += 1;
            continue;
        };
        let Some(actor_id) = peers.actor_id(peer_index) else {
            schedule.dead[peer_index] = true;
            if let Some(state) = schedule.pieces.get_mut(&piece_index) {
                state.remaining.push_front(request);
            }
            stalled_pieces = 0;
            continue;
        };
        match workers.try_request(actor_id, piece_index, request) {
            Ok(()) => {
                schedule.in_flight[peer_index] += 1;
                schedule.pending.insert(
                    (piece_index, request.offset),
                    PendingRequest {
                        request,
                        peer_index,
                        sent_at: Instant::now(),
                    },
                );
                stalled_pieces = 0;
            }
            Err(TryRequestError::Full(capacity_updates)) => {
                queue_blocked[peer_index] = true;
                queue_waiters.push((actor_id, capacity_updates));
                if let Some(state) = schedule.pieces.get_mut(&piece_index) {
                    state.remaining.push_front(request);
                }
            }
            Err(TryRequestError::Closed) => {
                mark_batch_peer_failed(
                    peer_index,
                    actor_id,
                    Some(piece_index),
                    workers,
                    schedule,
                    peers,
                );
                if let Some(state) = schedule.pieces.get_mut(&piece_index) {
                    state.remaining.push_front(request);
                }
                stalled_pieces = 0;
            }
        }
    }
    queue_waiters
}

//! Bounded request-window bookkeeping for endgame block transfers.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::Instant;

use crate::engine::bt_download_execute::{EndgameState, types::PeerKey};

use super::peer_actor::{PeerGeneration, TryRequestError};
use super::peer_snapshot::PeerSchedulingSnapshot;
use super::pipelined::BlockRequest;

#[derive(Clone, Copy)]
pub(super) struct PendingRequest {
    pub(super) request: BlockRequest,
    pub(super) sent_at: Instant,
}

pub(super) fn record_failed_peer(
    peer_index: usize,
    live: &mut [bool],
    peers: &PeerSchedulingSnapshot,
    failed_peers: &mut Vec<SocketAddr>,
) {
    live[peer_index] = false;
    if let Some(address) = peers.peer(peer_index).and_then(|peer| peer.address)
        && !failed_peers.contains(&address)
    {
        failed_peers.push(address);
    }
}

pub(super) fn take_peer_pending(
    peer_index: usize,
    pending: &mut HashMap<(u32, usize), PendingRequest>,
    in_flight: &mut [usize],
    piece_index: u32,
    endgame_state: &mut EndgameState,
    peers: &PeerSchedulingSnapshot,
) -> Vec<BlockRequest> {
    let block_indices = pending
        .keys()
        .filter_map(|(block_index, requested_peer)| {
            (*requested_peer == peer_index).then_some(*block_index)
        })
        .collect::<Vec<_>>();
    let address = peers.peer(peer_index).and_then(|peer| peer.address);
    let mut requests = Vec::with_capacity(block_indices.len());
    for block_index in block_indices {
        if let Some(entry) = pending.remove(&(block_index, peer_index)) {
            in_flight[peer_index] = in_flight[peer_index].saturating_sub(1);
            if let Some(address) = address {
                endgame_state.remove_peer_request(
                    piece_index,
                    entry.request.offset,
                    entry.request.length,
                    PeerKey::new(address),
                );
            }
            requests.push(entry.request);
        }
    }
    requests.sort_by_key(|request| request.block_index);
    requests
}

#[allow(clippy::too_many_arguments)]
pub(super) fn fill_request_windows(
    workers: &mut PeerGeneration,
    block_requests: &[BlockRequest],
    completed: &[bool],
    rejected: &[HashSet<u32>],
    pending: &mut HashMap<(u32, usize), PendingRequest>,
    in_flight: &mut [usize],
    live: &mut [bool],
    peers: &PeerSchedulingSnapshot,
    next_block: &mut [usize],
    piece_index: u32,
    endgame_state: &mut EndgameState,
    failed_peers: &mut Vec<SocketAddr>,
) -> bool {
    if block_requests.is_empty() {
        return false;
    }

    let mut queue_blocked = false;
    for peer_index in 0..peers.len() {
        if !live[peer_index]
            || !peers.peers()[peer_index].has_piece
            || !peers.peer_can_request(peer_index)
        {
            continue;
        }
        let Some(actor_id) = peers.actor_id(peer_index) else {
            record_failed_peer(peer_index, live, peers, failed_peers);
            continue;
        };
        let address = peers.peer(peer_index).and_then(|peer| peer.address);

        while in_flight[peer_index] < peers.request_window(peer_index) {
            let start = next_block[peer_index] % block_requests.len();
            let candidate = (0..block_requests.len())
                .map(|step| (start + step) % block_requests.len())
                .find(|block_index| {
                    let block_index_u32 = *block_index as u32;
                    let request = block_requests[*block_index];
                    !completed[*block_index]
                        && !rejected[peer_index].contains(&block_index_u32)
                        && !pending.contains_key(&(block_index_u32, peer_index))
                        && !address.is_some_and(|address| {
                            endgame_state.has_peer_request(
                                piece_index,
                                request.offset,
                                request.length,
                                PeerKey::new(address),
                            )
                        })
                });
            let Some(block_index) = candidate else {
                break;
            };
            let request = block_requests[block_index];
            match workers.try_request(actor_id, piece_index, request) {
                Ok(()) => {
                    pending.insert(
                        (block_index as u32, peer_index),
                        PendingRequest {
                            request,
                            sent_at: Instant::now(),
                        },
                    );
                    in_flight[peer_index] += 1;
                    next_block[peer_index] = (block_index + 1) % block_requests.len();
                    if let Some(address) = address {
                        endgame_state.track_request(
                            piece_index,
                            request.offset,
                            request.length,
                            PeerKey::new(address),
                        );
                    }
                }
                Err(TryRequestError::Full) => {
                    queue_blocked = true;
                    break;
                }
                Err(TryRequestError::Closed) => {
                    let cancelled = take_peer_pending(
                        peer_index,
                        pending,
                        in_flight,
                        piece_index,
                        endgame_state,
                        peers,
                    );
                    workers.cancel_peer_requests(actor_id, &cancelled, piece_index);
                    record_failed_peer(peer_index, live, peers, failed_peers);
                    break;
                }
            }
        }
    }
    queue_blocked
}

#[allow(clippy::too_many_arguments)]
pub(super) fn cancel_completed_block_duplicates(
    block_index: u32,
    winner_index: usize,
    piece_index: u32,
    request: BlockRequest,
    workers: &mut PeerGeneration,
    peers: &PeerSchedulingSnapshot,
    pending: &mut HashMap<(u32, usize), PendingRequest>,
    in_flight: &mut [usize],
    endgame_state: &mut EndgameState,
) {
    let winner = peers
        .peer(winner_index)
        .and_then(|peer| peer.address)
        .map(PeerKey::new);
    let cancel_targets = if let Some(winner) = winner {
        endgame_state.take_cancel_targets(piece_index, request.offset, request.length, winner)
    } else {
        let targets = endgame_state.get_cancel_targets(piece_index, request.offset, request.length);
        endgame_state.remove_request(piece_index, request.offset, request.length);
        targets
    };

    for target in cancel_targets {
        if let Some(peer_index) = peers.peer_index_at(target.address())
            && peer_index != winner_index
            && pending.remove(&(block_index, peer_index)).is_some()
        {
            in_flight[peer_index] = in_flight[peer_index].saturating_sub(1);
            if let Some(actor_id) = peers.actor_id(peer_index) {
                workers.cancel_peer_requests(actor_id, &[request], piece_index);
            }
        }
    }

    let remaining_peers = pending
        .keys()
        .filter_map(|(pending_block, peer_index)| {
            (*pending_block == block_index && *peer_index != winner_index).then_some(*peer_index)
        })
        .collect::<Vec<_>>();
    for peer_index in remaining_peers {
        if pending.remove(&(block_index, peer_index)).is_some() {
            in_flight[peer_index] = in_flight[peer_index].saturating_sub(1);
            if let Some(actor_id) = peers.actor_id(peer_index) {
                workers.cancel_peer_requests(actor_id, &[request], piece_index);
            }
        }
    }
    if pending.remove(&(block_index, winner_index)).is_some() {
        in_flight[winner_index] = in_flight[winner_index].saturating_sub(1);
    }
    endgame_state.remove_request(piece_index, request.offset, request.length);
}

pub(super) fn cancel_attempt_requests(
    piece_index: u32,
    workers: &mut PeerGeneration,
    peers: &PeerSchedulingSnapshot,
    pending: &mut HashMap<(u32, usize), PendingRequest>,
    in_flight: &mut [usize],
    endgame_state: &mut EndgameState,
    block_requests: &[BlockRequest],
) {
    let entries = std::mem::take(pending);
    let mut requests_by_peer = HashMap::<usize, Vec<BlockRequest>>::new();
    for ((_, peer_index), entry) in entries {
        if let Some(address) = peers.peer(peer_index).and_then(|peer| peer.address) {
            endgame_state.remove_peer_request(
                piece_index,
                entry.request.offset,
                entry.request.length,
                PeerKey::new(address),
            );
        }
        requests_by_peer
            .entry(peer_index)
            .or_default()
            .push(entry.request);
    }
    in_flight.fill(0);
    for (peer_index, mut requests) in requests_by_peer {
        requests.sort_by_key(|request| request.block_index);
        if let Some(actor_id) = peers.actor_id(peer_index) {
            workers.cancel_peer_requests(actor_id, &requests, piece_index);
        }
    }
    for request in block_requests {
        endgame_state.remove_request(piece_index, request.offset, request.length);
    }
}

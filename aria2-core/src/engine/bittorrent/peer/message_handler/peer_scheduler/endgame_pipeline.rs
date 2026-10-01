//! Event-driven endgame block scheduling over torrent-owned peer actors.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use tracing::{trace, warn};

use crate::engine::bittorrent::download::execute::{EndgameState, types::PeerKey};
use crate::engine::bittorrent::peer::choking_algorithm::ChokingAlgorithm;
use crate::error::{Aria2Error, FatalError, Result};

use super::super::types::{
    ActorAwarePieceDownloadResult, BLOCK_SIZE, DEFAULT_MAX_OUTSTANDING_REQUEST,
    MAX_OUTSTANDING_REQUEST, PeerDownloadBytes, PieceDownloadResult, ReceivedPieceBlock,
};
use super::endgame_requests::{
    PendingRequest, cancel_attempt_requests, cancel_completed_block_duplicates,
    fill_request_windows, record_failed_peer, take_peer_pending,
};
use super::normal_pipeline::piece_attempt_budget_exhausted;
use super::peer_actor::{
    PeerEvent, PeerGeneration, apply_choke_round, apply_interest_change,
    rebalance_upload_slots_after_peer_disconnect,
};
use super::peer_registry::PeerSwarm;
use super::peer_snapshot::PeerSchedulingSnapshot;
use super::pipelined::BlockRequest;
use super::wait_for_deadline;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn download_piece_blocks_endgame(
    swarm: &mut PeerSwarm,
    piece_index: u32,
    piece_length: u32,
    num_blocks: u32,
    endgame_state: &mut EndgameState,
    request_timeout: Duration,
    max_attempts: u32,
    mut choking_algo: Option<&mut ChokingAlgorithm>,
    resume_blocks: &[Option<bytes::Bytes>],
    block_sink: Option<&tokio::sync::mpsc::Sender<ReceivedPieceBlock>>,
) -> Result<ActorAwarePieceDownloadResult> {
    let block_count = piece_length.div_ceil(BLOCK_SIZE);
    if num_blocks != block_count {
        warn!(
            piece_index,
            requested_blocks = num_blocks,
            actual_blocks = block_count,
            "BT endgame block count disagrees with piece length; using the actual layout"
        );
    }

    let block_requests = (0..block_count)
        .map(|block_index| {
            let offset = block_index * BLOCK_SIZE;
            BlockRequest {
                block_index,
                offset,
                length: (piece_length - offset).min(BLOCK_SIZE),
            }
        })
        .collect::<Vec<_>>();
    let mut attempts = 0u32;
    let mut availability_changed_actor_ids = HashSet::new();
    let mut tracker_peers = Vec::new();
    let mut peers = PeerSchedulingSnapshot::capture(swarm);
    let mut workers = PeerGeneration::from_swarm(swarm, &[piece_index]);
    let Some(mut event_rx) = swarm.lease_event_receiver() else {
        return Err(Aria2Error::Fatal(FatalError::Config(
            "torrent peer event receiver is already leased".to_string(),
        )));
    };

    loop {
        attempts = attempts.saturating_add(1);
        let mut live = (0..peers.len())
            .map(|index| {
                peers
                    .actor_id(index)
                    .is_some_and(|actor_id| workers.has_peer(actor_id))
            })
            .collect::<Vec<_>>();
        let mut piece_data = vec![0u8; piece_length as usize];
        let mut peer_bytes = Vec::<PeerDownloadBytes>::new();
        let mut failed_peers = Vec::new();
        let mut pending = HashMap::<(u32, usize), PendingRequest>::new();
        let mut in_flight = vec![0usize; peers.len()];
        let mut rejected = (0..peers.len())
            .map(|_| HashSet::<u32>::new())
            .collect::<Vec<_>>();
        let mut completed = vec![false; block_count as usize];
        let mut completed_blocks = 0u32;
        for block_index in 0..block_count as usize {
            let offset = block_index as u32 * BLOCK_SIZE;
            let length = (piece_length - offset).min(BLOCK_SIZE) as usize;
            if let Some(Some(data)) = resume_blocks.get(block_index)
                && data.len() == length
            {
                piece_data[offset as usize..offset as usize + length].copy_from_slice(data);
                completed[block_index] = true;
                completed_blocks += 1;
            }
        }
        let mut next_block = (0..peers.len())
            .map(|peer_index| {
                peer_index.saturating_mul(DEFAULT_MAX_OUTSTANDING_REQUEST)
                    % block_requests.len().max(1)
            })
            .collect::<Vec<_>>();
        let mut responses_since_window_growth = vec![0usize; peers.len()];
        let mut last_activity = Instant::now();
        let mut complete = false;

        loop {
            let queue_waiters = fill_request_windows(
                &mut workers,
                &block_requests,
                &completed,
                &rejected,
                &mut pending,
                &mut in_flight,
                &mut live,
                &peers,
                &mut next_block,
                piece_index,
                endgame_state,
                &mut failed_peers,
            );
            let queue_blocked = !queue_waiters.is_empty();
            let mut queue_ready = FuturesUnordered::new();
            for (actor_id, mut capacity_updates) in queue_waiters {
                queue_ready.push(async move {
                    let _ = capacity_updates.changed().await;
                    actor_id
                });
            }
            if completed_blocks == block_count {
                complete = true;
                break;
            }

            let request_deadline = pending
                .values()
                .map(|request| request.sent_at + request_timeout)
                .min()
                .or_else(|| (!queue_blocked).then_some(last_activity + request_timeout));
            let choke_deadline = choking_algo
                .as_deref()
                .and_then(ChokingAlgorithm::next_choke_rotation_deadline);
            let mut wake_deadline = request_deadline;
            if let Some(choke_deadline) = choke_deadline {
                wake_deadline = Some(
                    wake_deadline.map_or(choke_deadline, |deadline| deadline.min(choke_deadline)),
                );
            }
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(event) = event else { break };
                    match event {
                        PeerEvent::UploadBytes { actor_id, .. }
                        | PeerEvent::UploadQueueChanged { actor_id, .. }
                        | PeerEvent::ChokeStateChanged { actor_id, .. } => {
                            if peers.peer_index_by_actor_id(actor_id).is_none() {
                                continue;
                            }
                        }
                        PeerEvent::PeerChokingChanged { actor_id, peer_choking } => {
                            let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                                continue;
                            };
                            peers.update_peer_choking(actor_id, peer_choking);
                            if peer_choking && !peers.peer_can_request(peer_index, piece_index) {
                                let cancelled = take_peer_pending(
                                    peer_index,
                                    &mut pending,
                                    &mut in_flight,
                                    piece_index,
                                    endgame_state,
                                    &peers,
                                );
                                workers.cancel_peer_requests(actor_id, &cancelled, piece_index);
                            } else if !peer_choking {
                                rejected[peer_index].clear();
                            }
                            last_activity = Instant::now();
                        }
                        PeerEvent::AllowedFast { actor_id, piece_index: allowed_piece } => {
                            let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                                continue;
                            };
                            peers.add_peer_allowed_fast(actor_id, allowed_piece);
                            if peers.peer_can_request(peer_index, piece_index) {
                                rejected[peer_index].clear();
                            }
                            last_activity = Instant::now();
                        }
                        PeerEvent::PeerAvailabilityChanged { actor_id, piece_index: changed_piece, has_piece } => {
                            workers.record_availability_change(actor_id);
                            peers.update_peer_availability(actor_id, changed_piece, has_piece);
                            if changed_piece == piece_index {
                                if let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) {
                                    if has_piece {
                                        rejected[peer_index].clear();
                                    } else {
                                        let cancelled = take_peer_pending(
                                            peer_index,
                                            &mut pending,
                                            &mut in_flight,
                                            piece_index,
                                            endgame_state,
                                            &peers,
                                        );
                                        workers.cancel_peer_requests(actor_id, &cancelled, piece_index);
                                    }
                                }
                                last_activity = Instant::now();
                            }
                        }
                        PeerEvent::PeerAvailabilitySnapshot { actor_id, bitfield, seeder } => {
                            workers.record_availability_change(actor_id);
                            peers.update_peer_bitfield(actor_id, &bitfield);
                            peers.update_peer_seeder(actor_id, seeder);
                            if let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) {
                                if peers.has_piece(peer_index, piece_index) {
                                    rejected[peer_index].clear();
                                } else {
                                    let cancelled = take_peer_pending(
                                        peer_index,
                                        &mut pending,
                                        &mut in_flight,
                                        piece_index,
                                        endgame_state,
                                        &peers,
                                    );
                                    if let Some(actor_id) = peers.actor_id(peer_index) {
                                        workers.cancel_peer_requests(actor_id, &cancelled, piece_index);
                                    }
                                }
                                last_activity = Instant::now();
                            }
                        }
                        PeerEvent::PexPeers { peers, .. } => workers.record_pex_peers(peers),
                        PeerEvent::TrackerPeers { peers } => tracker_peers.extend(peers),
                        PeerEvent::ExtensionHandshakeReceived { .. } => {}
                        PeerEvent::MetadataMessage { .. } => {}
                        PeerEvent::InterestChanged { actor_id, snapshot } => {
                            if peers.peer_index_by_actor_id(actor_id).is_none() {
                                continue;
                            }
                            apply_interest_change(
                                &mut workers,
                                &peers,
                                choking_algo.as_deref_mut(),
                                *snapshot,
                            );
                        }
                        PeerEvent::AmInterestChanged { .. } => {}
                        PeerEvent::Message { actor_id, generation, message, .. } => {
                            if generation != workers.generation() {
                                continue;
                            }
                            let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                                continue;
                            };
                            use aria2_protocol::bittorrent::message::types::BtMessage;
                            if let BtMessage::Piece { data, .. } = &message
                                && let Some(identity) = peers.peer_identity(peer_index)
                                && let Some(algo) = choking_algo.as_deref_mut()
                            {
                                algo.on_data_received_by_identity(identity, data.len() as u64);
                            }
                            match message {
                                BtMessage::Piece { index, begin, data } if index == piece_index => {
                                    let block_index = begin / BLOCK_SIZE;
                                    let Some(request) = block_requests.get(block_index as usize).copied()
                                        .filter(|request| request.offset == begin)
                                    else {
                                        continue;
                                    };
                                    if completed[block_index as usize]
                                        || !pending.contains_key(&(block_index, peer_index))
                                    {
                                        continue;
                                    }
                                    if data.len() != request.length as usize {
                                        warn!(piece_index, offset = begin, expected = request.length, actual = data.len(), "Discarding malformed BT endgame block");
                                        let cancelled = take_peer_pending(
                                            peer_index,
                                            &mut pending,
                                            &mut in_flight,
                                            piece_index,
                                            endgame_state,
                                            &peers,
                                        );
                                        workers.cancel_peer_requests(actor_id, &cancelled, piece_index);
                                        record_failed_peer(peer_index, &mut live, &peers, &mut failed_peers);
                                        last_activity = Instant::now();
                                        continue;
                                    }

                                    let start = request.offset as usize;
                                    let data = bytes::Bytes::from(data);
                                    piece_data[start..start + data.len()].copy_from_slice(&data);
                                    completed[block_index as usize] = true;
                                    completed_blocks += 1;
                                    if let Some(block_sink) = block_sink {
                                        let _ = block_sink
                                            .send(ReceivedPieceBlock {
                                                piece_index,
                                                block_index,
                                                offset: request.offset,
                                                data: data.clone(),
                                            })
                                            .await;
                                    }
                                    cancel_completed_block_duplicates(
                                        block_index,
                                        peer_index,
                                        piece_index,
                                        request,
                                        &mut workers,
                                        &peers,
                                        &mut pending,
                                        &mut in_flight,
                                        endgame_state,
                                    );

                                    if let Some(address) = peers.peer(peer_index).and_then(|peer| peer.address) {
                                        let bytes = data.len() as u64;
                                        if let Some(entry) = peer_bytes.iter_mut().find(|entry| entry.peer_index == peer_index) {
                                            entry.bytes += bytes;
                                        } else {
                                            peer_bytes.push(PeerDownloadBytes { peer_index, peer: address, bytes });
                                        }
                                    }

                                    responses_since_window_growth[peer_index] += 1;
                                    let current_window = peers.request_window(peer_index);
                                    let growth_threshold = current_window.div_ceil(4);
                                    if current_window < MAX_OUTSTANDING_REQUEST
                                        && responses_since_window_growth[peer_index] >= growth_threshold
                                        && let Some(new_window) = event_rx.increase_request_window(actor_id)
                                    {
                                        peers.update_request_window(actor_id, new_window);
                                        responses_since_window_growth[peer_index] = 0;
                                        trace!(actor_id = actor_id.0, previous_window = current_window, new_window, "Increased BT endgame peer request window after successful blocks");
                                    }
                                    last_activity = Instant::now();
                                }
                                BtMessage::Reject { index, offset, .. } if index == piece_index => {
                                    let block_index = offset / BLOCK_SIZE;
                                    if block_requests.get(block_index as usize).is_some_and(|request| request.offset == offset)
                                        && let Some(entry) = pending.remove(&(block_index, peer_index))
                                    {
                                        in_flight[peer_index] = in_flight[peer_index].saturating_sub(1);
                                        if let Some(address) = peers.peer(peer_index).and_then(|peer| peer.address) {
                                            endgame_state.remove_peer_request(piece_index, entry.request.offset, entry.request.length, PeerKey::new(address));
                                        }
                                        rejected[peer_index].insert(block_index);
                                        last_activity = Instant::now();
                                    }
                                }
                                _ => {}
                            }
                        }
                        PeerEvent::RequestFailed { actor_id, generation, piece_index: failed_piece, request } => {
                            if generation != workers.generation() || failed_piece != piece_index {
                                continue;
                            }
                            let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                                continue;
                            };
                            trace!(peer_index, offset = request.offset, "BT endgame request send failed");
                            let cancelled = take_peer_pending(
                                peer_index,
                                &mut pending,
                                &mut in_flight,
                                piece_index,
                                endgame_state,
                                &peers,
                            );
                            workers.cancel_peer_requests(actor_id, &cancelled, piece_index);
                            record_failed_peer(peer_index, &mut live, &peers, &mut failed_peers);
                            last_activity = Instant::now();
                        }
                        PeerEvent::Disconnected { actor_id }
                        | PeerEvent::GracefulDisconnected { actor_id } => {
                            let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                                continue;
                            };
                            let identity = peers.peer_identity(peer_index);
                            let cancelled = take_peer_pending(
                                peer_index,
                                &mut pending,
                                &mut in_flight,
                                piece_index,
                                endgame_state,
                                &peers,
                            );
                            workers.cancel_peer_requests(actor_id, &cancelled, piece_index);
                            record_failed_peer(peer_index, &mut live, &peers, &mut failed_peers);
                            if let Some(identity) = identity {
                                rebalance_upload_slots_after_peer_disconnect(
                                    &mut workers,
                                    &peers,
                                    choking_algo.as_deref_mut(),
                                    identity,
                                );
                            }
                            last_activity = Instant::now();
                        }
                    }
                }
                Some(_) = queue_ready.next(), if !queue_ready.is_empty() => {}
                _ = wait_for_deadline(wake_deadline) => {
                    let now = Instant::now();
                    if choking_algo
                        .as_deref()
                        .is_some_and(|algo| algo.choke_rotation_due(now))
                    {
                        apply_choke_round(
                            &mut workers,
                            &peers,
                            choking_algo.as_deref_mut(),
                        );
                    }
                    let expired_peers = pending
                        .iter()
                        .filter_map(|((_, peer_index), request)| {
                            (now >= request.sent_at + request_timeout).then_some(*peer_index)
                        })
                        .collect::<HashSet<_>>();
                    for peer_index in expired_peers {
                        warn!(peer_index, "BT endgame request window timed out");
                        let cancelled = take_peer_pending(
                            peer_index,
                            &mut pending,
                            &mut in_flight,
                            piece_index,
                            endgame_state,
                            &peers,
                        );
                        if let Some(actor_id) = peers.actor_id(peer_index) {
                            workers.cancel_peer_requests(actor_id, &cancelled, piece_index);
                        }
                        record_failed_peer(peer_index, &mut live, &peers, &mut failed_peers);
                    }
                    if pending.is_empty()
                        && !queue_blocked
                        && now >= last_activity + request_timeout
                    {
                        break;
                    }
                }
            }
        }

        if complete {
            workers.finish_generation(&mut event_rx).await;
            availability_changed_actor_ids.extend(workers.take_availability_changes());
            let peer_actor_ids = peer_bytes
                .iter()
                .map(|peer| {
                    peers
                        .actor_id(peer.peer_index)
                        .expect("peer attribution index must remain in the captured snapshot")
                })
                .collect();
            return Ok(ActorAwarePieceDownloadResult {
                piece: Ok(PieceDownloadResult {
                    data: piece_data,
                    peer_bytes,
                    failed_peers,
                }),
                peer_actor_ids,
                availability_changed_actor_ids: availability_changed_actor_ids
                    .into_iter()
                    .collect(),
                pex_peers: workers.take_pex_peers(),
                tracker_peers,
            });
        }

        cancel_attempt_requests(
            piece_index,
            &mut workers,
            &peers,
            &mut pending,
            &mut in_flight,
            endgame_state,
            &block_requests,
        );
        if piece_attempt_budget_exhausted(attempts, max_attempts) {
            workers.finish_generation(&mut event_rx).await;
            availability_changed_actor_ids.extend(workers.take_availability_changes());
            break;
        }
        workers.advance_generations();
    }

    Ok(ActorAwarePieceDownloadResult {
        piece: Err(Aria2Error::Network(format!(
            "Failed to download piece {} after {} endgame attempts",
            piece_index,
            if max_attempts == 0 {
                attempts
            } else {
                max_attempts
            }
        ))),
        peer_actor_ids: Vec::new(),
        availability_changed_actor_ids: availability_changed_actor_ids.into_iter().collect(),
        pex_peers: workers.take_pex_peers(),
        tracker_peers,
    })
}

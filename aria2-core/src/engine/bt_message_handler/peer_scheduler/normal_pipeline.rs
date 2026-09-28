//! Request-window scheduling for ordinary BT pieces.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tracing::{debug, trace, warn};

use crate::engine::choking_algorithm::ChokingAlgorithm;
use crate::request::request_group::AtomicProgress;

use super::super::types::{
    BLOCK_SIZE, MAX_OUTSTANDING_REQUEST, PeerDownloadBytes, PieceRequestPlan,
};
use super::download_speed::DownloadSpeedSampler;
use super::peer_actor::{
    PeerEvent, PeerGeneration, apply_choke_round, apply_interest_change,
    rebalance_upload_slots_after_peer_disconnect,
};
use super::peer_registry::PeerSwarmEventLease;
use super::peer_snapshot::PeerSchedulingSnapshot;
use super::pipelined::BlockRequest;

struct BatchPieceState {
    remaining: VecDeque<BlockRequest>,
    completed: Vec<bool>,
    piece_data: Vec<u8>,
    completed_blocks: u32,
    peer_bytes: Vec<PeerDownloadBytes>,
    failed_peers: Vec<SocketAddr>,
}

impl BatchPieceState {
    fn new(plan: PieceRequestPlan) -> Self {
        let block_count = plan.piece_length.div_ceil(BLOCK_SIZE);
        let remaining = (0..block_count)
            .map(|block_index| {
                let offset = block_index * BLOCK_SIZE;
                BlockRequest {
                    block_index,
                    offset,
                    length: (plan.piece_length - offset).min(BLOCK_SIZE),
                }
            })
            .collect::<VecDeque<_>>();
        Self {
            remaining,
            completed: vec![false; block_count as usize],
            piece_data: vec![0; plan.piece_length as usize],
            completed_blocks: 0,
            peer_bytes: Vec::new(),
            failed_peers: Vec::new(),
        }
    }
}

struct BatchSchedule {
    pieces: HashMap<u32, BatchPieceState>,
    piece_order: Vec<u32>,
    pending: HashMap<(u32, u32), PendingRequest>,
    in_flight: Vec<usize>,
    dead: Vec<bool>,
    responses_since_window_growth: Vec<usize>,
    peer_cursor: usize,
    piece_cursor: usize,
}

pub(super) struct BatchAttemptOutcome {
    pub(super) completed: HashMap<u32, AttemptOutcome>,
    pub(super) failed_peers: HashMap<u32, Vec<SocketAddr>>,
    pub(super) tracker_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
}

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
    piece_index: u32,
) -> Option<usize> {
    if in_flight.is_empty() {
        return None;
    }

    for step in 0..in_flight.len() {
        let index = (*cursor + step) % in_flight.len();
        if dead[index] || in_flight[index] >= peers.request_window(index) {
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

fn take_peer_pending_batch(
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

fn requeue_batch_requests(
    requests: &[(u32, BlockRequest)],
    states: &mut HashMap<u32, BatchPieceState>,
) {
    for (piece_index, request) in requests.iter().rev() {
        if let Some(state) = states.get_mut(piece_index) {
            state.remaining.push_front(*request);
        }
    }
}

fn cancel_batch_requests(
    actor_id: crate::engine::bt_peer_connection::PeerActorId,
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

fn mark_batch_peer_failed(
    peer_index: usize,
    actor_id: crate::engine::bt_peer_connection::PeerActorId,
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

async fn fill_batch_request_windows(
    workers: &mut PeerGeneration,
    schedule: &mut BatchSchedule,
    peers: &PeerSchedulingSnapshot,
) {
    if schedule.piece_order.is_empty() {
        return;
    }

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
        if !workers.request(actor_id, piece_index, request).await {
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
            continue;
        }
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
}

pub(super) async fn run_attempt_batch(
    workers: &mut PeerGeneration,
    event_rx: &mut PeerSwarmEventLease<'_>,
    plans: &[PieceRequestPlan],
    peers: &mut PeerSchedulingSnapshot,
    mut choking_algo: Option<&mut ChokingAlgorithm>,
    network_activity: Option<&AtomicProgress>,
    request_timeout: Duration,
) -> BatchAttemptOutcome {
    let mut pieces = HashMap::<u32, BatchPieceState>::with_capacity(plans.len());
    let mut piece_order = Vec::with_capacity(plans.len());
    for plan in plans {
        let actual_blocks = plan.piece_length.div_ceil(BLOCK_SIZE);
        if plan.num_blocks != actual_blocks {
            warn!(
                piece_index = plan.piece_index,
                requested_blocks = plan.num_blocks,
                actual_blocks,
                "BT block count disagrees with piece length; using the actual layout"
            );
        }
        if pieces
            .insert(plan.piece_index, BatchPieceState::new(*plan))
            .is_none()
        {
            piece_order.push(plan.piece_index);
        }
    }
    if pieces.is_empty() {
        return BatchAttemptOutcome {
            completed: HashMap::new(),
            failed_peers: HashMap::new(),
            tracker_peers: Vec::new(),
        };
    }

    let mut schedule = BatchSchedule {
        pieces,
        piece_order,
        pending: HashMap::new(),
        in_flight: vec![0usize; peers.len()],
        dead: vec![false; peers.len()],
        responses_since_window_growth: vec![0usize; peers.len()],
        peer_cursor: 0,
        piece_cursor: 0,
    };
    let mut completed = HashMap::<u32, AttemptOutcome>::with_capacity(plans.len());
    let mut tracker_peers = Vec::new();
    let mut download_speed = DownloadSpeedSampler::new();

    loop {
        fill_batch_request_windows(workers, &mut schedule, peers).await;

        if schedule.pieces.is_empty() {
            break;
        }

        let next_request_deadline = schedule
            .pending
            .values()
            .map(|request| block_request_deadline(request.sent_at, request_timeout))
            .min()
            .unwrap_or_else(|| block_request_deadline(Instant::now(), request_timeout));
        let choke_deadline = choking_algo
            .as_deref()
            .and_then(ChokingAlgorithm::next_choke_rotation_deadline);
        let mut next_deadline = choke_deadline.map_or(next_request_deadline, |deadline| {
            deadline.min(next_request_deadline)
        });
        if network_activity.is_some()
            && let Some(deadline) = download_speed.next_deadline()
        {
            next_deadline = next_deadline.min(deadline);
        }
        let wait = next_deadline.saturating_duration_since(Instant::now());
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
                        let retry = take_peer_pending_batch(
                            peer_index,
                            &mut schedule.pending,
                            &mut schedule.in_flight,
                            |piece_index| !peers.peer_can_request(peer_index, piece_index),
                        );
                        requeue_batch_requests(&retry, &mut schedule.pieces);
                        cancel_batch_requests(actor_id, &retry, workers);
                    }
                    PeerEvent::AllowedFast { actor_id, piece_index } => {
                        peers.add_peer_allowed_fast(actor_id, piece_index);
                    }
                    PeerEvent::PeerAvailabilityChanged { actor_id, piece_index, has_piece } => {
                        workers.record_availability_change(actor_id);
                        peers.update_peer_availability(actor_id, piece_index, has_piece);
                        if !has_piece {
                            let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                                continue;
                            };
                            let retry = take_peer_pending_batch(
                                peer_index,
                                &mut schedule.pending,
                                &mut schedule.in_flight,
                                |requested_piece| requested_piece == piece_index,
                            );
                            requeue_batch_requests(&retry, &mut schedule.pieces);
                            cancel_batch_requests(actor_id, &retry, workers);
                        }
                    }
                    PeerEvent::PeerAvailabilitySnapshot { actor_id, bitfield, seeder } => {
                        workers.record_availability_change(actor_id);
                        peers.update_peer_bitfield(actor_id, &bitfield);
                        peers.update_peer_seeder(actor_id, seeder);
                        let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                            continue;
                        };
                        let retry = take_peer_pending_batch(
                            peer_index,
                            &mut schedule.pending,
                            &mut schedule.in_flight,
                            |piece_index| !peers.has_piece(peer_index, piece_index),
                        );
                        requeue_batch_requests(&retry, &mut schedule.pieces);
                        cancel_batch_requests(actor_id, &retry, workers);
                    }
                    PeerEvent::PexPeers { peers, .. } => workers.record_pex_peers(peers),
                    PeerEvent::TrackerPeers { peers } => tracker_peers.extend(peers),
                    PeerEvent::PexNegotiated { .. } | PeerEvent::AmInterestChanged { .. } => {}
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
                            BtMessage::Piece { index, begin, data } => {
                                let key = (index, begin);
                                let Some(entry) = schedule.pending.remove(&key) else {
                                    trace!(piece_index = index, offset = begin, "Ignoring unsolicited BT piece block");
                                    continue;
                                };
                                if entry.peer_index != peer_index {
                                    schedule.pending.insert(key, entry);
                                    continue;
                                }
                                schedule.in_flight[peer_index] = schedule.in_flight[peer_index].saturating_sub(1);
                                if data.len() != entry.request.length as usize {
                                    warn!(piece_index = index, offset = begin, expected = entry.request.length, actual = data.len(), "Discarding BT block with unexpected length");
                                    if let Some(state) = schedule.pieces.get_mut(&index) {
                                        state.remaining.push_front(entry.request);
                                    }
                                    mark_batch_peer_failed(
                                        peer_index,
                                        actor_id,
                                        Some(index),
                                        workers,
                                        &mut schedule,
                                        peers,
                                    );
                                    continue;
                                }
                                let Some(state) = schedule.pieces.get_mut(&index) else {
                                    continue;
                                };
                                let start = entry.request.offset as usize;
                                let end = start.saturating_add(data.len());
                                let block_index = entry.request.block_index as usize;
                                if end > state.piece_data.len() || state.completed.get(block_index).copied().unwrap_or(true) {
                                    continue;
                                }
                                if !data.is_empty()
                                    && let Some(progress) = network_activity
                                {
                                    progress.record_network_activity();
                                    download_speed.record(data.len() as u64);
                                }
                                state.piece_data[start..end].copy_from_slice(&data);
                                state.completed[block_index] = true;
                                state.completed_blocks += 1;
                                if let Some(address) = peers.peer(peer_index).and_then(|peer| peer.address) {
                                    let bytes = data.len() as u64;
                                    if let Some(entry) = state.peer_bytes.iter_mut().find(|entry| entry.peer_index == peer_index) {
                                        entry.bytes += bytes;
                                    } else {
                                        state.peer_bytes.push(PeerDownloadBytes { peer_index, peer: address, bytes });
                                    }
                                }
                                schedule.responses_since_window_growth[peer_index] += 1;
                                let current_window = peers.request_window(peer_index);
                                let growth_threshold = current_window.div_ceil(4);
                                if current_window < MAX_OUTSTANDING_REQUEST
                                    && schedule.responses_since_window_growth[peer_index] >= growth_threshold
                                    && let Some(new_window) = event_rx.increase_request_window(actor_id)
                                {
                                    peers.update_request_window(actor_id, new_window);
                                    schedule.responses_since_window_growth[peer_index] = 0;
                                }
                                let piece_complete = state.completed_blocks as usize == state.completed.len();
                                if piece_complete {
                                    let state = schedule.pieces.remove(&index).expect("completed batch piece must remain registered");
                                    workers.finish_piece_generation(index).await;
                                    completed.insert(index, AttemptOutcome {
                                        data: Some(state.piece_data),
                                        peer_bytes: state.peer_bytes,
                                        failed_peers: state.failed_peers,
                                    });
                                }
                            }
                            BtMessage::Reject { index, offset, .. }
                                if schedule.pending.contains_key(&(index, offset)) =>
                            {
                                mark_batch_peer_failed(
                                    peer_index,
                                    actor_id,
                                    Some(index),
                                    workers,
                                    &mut schedule,
                                    peers,
                                );
                            }
                            _ => {}
                        }
                    }
                    PeerEvent::RequestFailed { actor_id, generation, piece_index, request } => {
                        if generation != workers.generation() || !schedule.pieces.contains_key(&piece_index) {
                            continue;
                        }
                        let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                            continue;
                        };
                        debug!(peer_index, piece_index, offset = request.offset, "BT request send failed");
                        mark_batch_peer_failed(
                            peer_index,
                            actor_id,
                            Some(piece_index),
                            workers,
                            &mut schedule,
                            peers,
                        );
                    }
                    PeerEvent::Disconnected { actor_id } => {
                        let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                            continue;
                        };
                        let identity = peers.peer_identity(peer_index);
                        mark_batch_peer_failed(
                            peer_index,
                            actor_id,
                            None,
                            workers,
                            &mut schedule,
                            peers,
                        );
                        if let Some(identity) = identity {
                            rebalance_upload_slots_after_peer_disconnect(
                                workers,
                                peers,
                                choking_algo.as_deref_mut(),
                                identity,
                            )
                            .await;
                        }
                    }
                }
            }
            _ = tokio::time::sleep(wait) => {
                let now = Instant::now();
                if let Some(progress) = network_activity
                    && download_speed.next_deadline().is_some_and(|deadline| now >= deadline)
                {
                    progress.set_download_speed(download_speed.sample(now));
                }
                if choking_algo
                    .as_deref()
                    .is_some_and(|algo| algo.choke_rotation_due(now))
                {
                    apply_choke_round(workers, peers, choking_algo.as_deref_mut()).await;
                }
                let expired = schedule.pending
                    .values()
                    .filter(|request| now >= block_request_deadline(request.sent_at, request_timeout))
                    .map(|request| request.peer_index)
                    .collect::<HashSet<_>>();
                if schedule.pending.is_empty() {
                    break;
                }
                for peer_index in expired {
                    warn!(peer_index, "BT block request window timed out");
                    if let Some(actor_id) = peers.actor_id(peer_index) {
                        mark_batch_peer_failed(
                            peer_index,
                            actor_id,
                            None,
                            workers,
                            &mut schedule,
                            peers,
                        );
                    }
                }
            }
        }
    }

    let failed_peers = schedule
        .pieces
        .into_iter()
        .map(|(piece_index, state)| (piece_index, state.failed_peers))
        .collect();
    BatchAttemptOutcome {
        completed,
        failed_peers,
        tracker_peers,
    }
}

//! Event-driven endgame block scheduling over torrent-owned peer actors.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use tracing::warn;

use crate::constants;
use crate::engine::bt_download_execute::{EndgameState, types::PeerKey};
use crate::engine::choking_algorithm::ChokingAlgorithm;
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::AtomicProgress;

use super::super::types::{
    ActorAwarePieceDownloadResult, BLOCK_SIZE, PeerDownloadBytes, PieceDownloadResult,
};
use super::normal_pipeline::piece_attempt_budget_exhausted;
use super::peer_actor::{PeerEvent, PeerGeneration, apply_choke_round, apply_interest_change};
use super::peer_registry::PeerSwarm;
use super::peer_snapshot::PeerSchedulingSnapshot;
use super::pipelined::BlockRequest;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn download_piece_blocks_endgame(
    swarm: &mut PeerSwarm,
    piece_index: u32,
    piece_length: u32,
    num_blocks: u32,
    endgame_state: &mut EndgameState,
    network_activity: Option<&AtomicProgress>,
    request_timeout: Duration,
    max_attempts: u32,
    mut choking_algo: Option<&mut ChokingAlgorithm>,
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

    let mut attempts = 0u32;
    let mut availability_changed_actor_ids = HashSet::new();
    let mut peers = PeerSchedulingSnapshot::capture(swarm, piece_index);
    let mut workers = PeerGeneration::from_swarm(swarm, piece_index).await;
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
        let mut complete = true;

        for block_index in 0..block_count {
            let offset = block_index * BLOCK_SIZE;
            let request = BlockRequest {
                block_index,
                offset,
                length: (piece_length - offset).min(BLOCK_SIZE),
            };
            let mut requested = vec![false; peers.len()];
            let mut rejected = vec![false; peers.len()];

            for peer_index in 0..peers.len() {
                if !live[peer_index] {
                    continue;
                }
                let sent = peers
                    .actor_id(peer_index)
                    .is_some_and(|actor_id| workers.try_request(actor_id, piece_index, request));
                if sent {
                    requested[peer_index] = true;
                    if let Some(address) = peers.peer(peer_index).and_then(|peer| peer.address) {
                        endgame_state.track_request(
                            piece_index,
                            request.offset,
                            request.length,
                            PeerKey::new(address),
                        );
                    }
                } else {
                    live[peer_index] = false;
                    if let Some(address) = peers.peer(peer_index).and_then(|peer| peer.address)
                        && !failed_peers.contains(&address)
                    {
                        failed_peers.push(address);
                    }
                }
            }

            let mut winner = None;
            let deadline = Instant::now() + request_timeout;
            while winner.is_none() {
                if !(0..live.len()).any(|index| live[index] && requested[index] && !rejected[index])
                {
                    break;
                }
                let choke_deadline = choking_algo
                    .as_deref()
                    .and_then(ChokingAlgorithm::next_choke_rotation_deadline);
                let wake_deadline =
                    choke_deadline.map_or(deadline, |choke_deadline| deadline.min(choke_deadline));
                let wait = wake_deadline.saturating_duration_since(Instant::now());
                if wait.is_zero() {
                    break;
                }
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
                                    peers.update_peer_availability(
                                        actor_id,
                                        piece_index,
                                        has_piece,
                                    );
                                }
                            }
                            PeerEvent::PeerAvailabilityChanged {
                                actor_id,
                                piece_index: changed_piece,
                                has_piece,
                            } => {
                                workers.record_availability_change(actor_id);
                                peers.update_peer_availability(
                                    actor_id,
                                    changed_piece,
                                    has_piece,
                                );
                            }
                            PeerEvent::PeerAvailabilitySnapshot {
                                actor_id, bitfield, ..
                            } => {
                                workers.record_availability_change(actor_id);
                                peers.update_peer_bitfield(actor_id, &bitfield);
                            }
                            PeerEvent::PexPeers { peers, .. } => {
                                workers.record_pex_peers(peers);
                            }
                            PeerEvent::InterestChanged { actor_id, snapshot } => {
                                if peers.peer_index_by_actor_id(actor_id).is_none() {
                                    continue;
                                }
                                apply_interest_change(
                                    &mut workers,
                                    &peers,
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
                                tracing::trace!(actor_id = actor_id.0, peer_index, "Received BT peer event");
                                use aria2_protocol::bittorrent::message::types::BtMessage;
                                if let BtMessage::Piece { data, .. } = &message
                                    && let Some(identity) = peers.peer_identity(peer_index)
                                    && let Some(algo) = choking_algo.as_deref_mut()
                                {
                                    algo.on_data_received_by_identity(
                                        identity,
                                        data.len() as u64,
                                    );
                                }
                                match message {
                                    BtMessage::Piece { index, begin, data }
                                        if index == piece_index
                                            && begin == request.offset
                                            && requested.get(peer_index).copied().unwrap_or(false) =>
                                    {
                                        if data.len() == request.length as usize {
                                            winner = Some((peer_index, data));
                                        } else {
                                            rejected[peer_index] = true;
                                            warn!(piece_index, offset = begin, expected = request.length,
                                                actual = data.len(), "Discarding malformed BT endgame block");
                                        }
                                    }
                                    BtMessage::Reject { index, offset, .. }
                                        if index == piece_index && offset == request.offset => {
                                        rejected[peer_index] = true;
                                    }
                                    _ => {}
                                }
                            }
                            PeerEvent::RequestFailed {
                                actor_id,
                                generation,
                                ..
                            } => {
                                if generation != workers.generation() {
                                    continue;
                                }
                                let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                                    continue;
                                };
                                live[peer_index] = false;
                                if let Some(address) = peers.peer(peer_index).and_then(|peer| peer.address)
                                    && !failed_peers.contains(&address)
                                {
                                    failed_peers.push(address);
                                }
                            }
                            PeerEvent::Disconnected { actor_id } => {
                                let Some(peer_index) = peers.peer_index_by_actor_id(actor_id) else {
                                    continue;
                                };
                                live[peer_index] = false;
                                if let Some(address) = peers.peer(peer_index).and_then(|peer| peer.address)
                                    && !failed_peers.contains(&address)
                                {
                                    failed_peers.push(address);
                                }
                            }
                        }
                    }
                    _ = tokio::time::sleep(wait) => {
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        if choking_algo
                            .as_deref()
                            .is_some_and(|algo| algo.choke_rotation_due(now))
                        {
                            apply_choke_round(
                                &mut workers,
                                &peers,
                                choking_algo.as_deref_mut(),
                            )
                            .await;
                        }
                    },
                }
            }

            if let Some((winner_index, data)) = winner {
                if !data.is_empty()
                    && let Some(progress) = network_activity
                {
                    progress.record_network_activity();
                }
                let start = request.offset as usize;
                piece_data[start..start + data.len()].copy_from_slice(&data);
                if let Some(address) = peers.peer(winner_index).and_then(|peer| peer.address) {
                    let bytes = data.len() as u64;
                    if let Some(entry) = peer_bytes
                        .iter_mut()
                        .find(|entry| entry.peer_index == winner_index)
                    {
                        entry.bytes += bytes;
                    } else {
                        peer_bytes.push(PeerDownloadBytes {
                            peer_index: winner_index,
                            peer: address,
                            bytes,
                        });
                    }
                }
                let winner_key = peers
                    .peer(winner_index)
                    .and_then(|peer| peer.address)
                    .map(PeerKey::new);
                let cancel_targets = winner_key
                    .map(|key| {
                        endgame_state.take_cancel_targets(
                            piece_index,
                            request.offset,
                            request.length,
                            key,
                        )
                    })
                    .unwrap_or_default();
                for key in cancel_targets {
                    if let Some(peer_index) = peers.peer_index_at(key.address())
                        && peer_index != winner_index
                        && let Some(actor_id) = peers.actor_id(peer_index)
                    {
                        let _ = workers.cancel(actor_id, piece_index, request).await;
                    }
                }
            } else {
                let targets =
                    endgame_state.get_cancel_targets(piece_index, request.offset, request.length);
                endgame_state.remove_request(piece_index, request.offset, request.length);
                for key in targets {
                    if let Some(peer_index) = peers.peer_index_at(key.address())
                        && let Some(actor_id) = peers.actor_id(peer_index)
                    {
                        let _ = workers.cancel(actor_id, piece_index, request).await;
                    }
                }
                complete = false;
                break;
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
                piece: PieceDownloadResult {
                    data: piece_data,
                    peer_bytes,
                    failed_peers,
                },
                peer_actor_ids,
                availability_changed_actor_ids: availability_changed_actor_ids
                    .into_iter()
                    .collect(),
                pex_peers: workers.take_pex_peers(),
            });
        }
        if piece_attempt_budget_exhausted(attempts, max_attempts) {
            workers.finish_generation(&mut event_rx).await;
            availability_changed_actor_ids.extend(workers.take_availability_changes());
            break;
        }
        tokio::time::sleep(Duration::from_millis(constants::BT_RETRY_DELAY_MS)).await;
        workers.advance_generation(piece_index).await;
    }

    Err(Aria2Error::Fatal(FatalError::Config(format!(
        "Failed to download piece {} after {} endgame attempts",
        piece_index,
        if max_attempts == 0 {
            attempts
        } else {
            max_attempts
        }
    ))))
}

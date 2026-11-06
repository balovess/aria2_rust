//! Public orchestration for normal pipelined BT piece downloads.

use std::collections::HashMap;
use std::time::Duration;

use tracing::info;

#[cfg(test)]
use super::super::types::ActorAwarePieceDownloadResult;
use super::super::types::{
    ActorAwarePieceBatchEntry, ActorAwarePieceBatchResult, PieceDownloadResult, PieceRequestPlan,
    ReceivedPieceBlock,
};
use super::normal_pipeline::run_attempt_batch;
use super::peer_actor::PeerGeneration;
use super::peer_registry::PeerSwarm;
use super::peer_snapshot::PeerSchedulingSnapshot;
use crate::engine::bittorrent::peer::choking_algorithm::ChokingAlgorithm;
use crate::error::{Aria2Error, FatalError, Result};

#[cfg(test)]
#[path = "pipelined/tests.rs"]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BlockRequest {
    pub(super) block_index: u32,
    pub(super) offset: u32,
    pub(super) length: u32,
}

impl BlockRequest {
    pub(super) fn message(
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

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) async fn download_piece_blocks(
    swarm: &mut PeerSwarm,
    piece_index: u32,
    piece_length: u32,
    num_blocks: u32,
    request_timeout: Duration,
    max_attempts: u32,
    choking_algo: Option<&mut ChokingAlgorithm>,
) -> Result<ActorAwarePieceDownloadResult> {
    let mut batch = download_piece_blocks_batch(
        swarm,
        &[PieceRequestPlan {
            piece_index,
            piece_length,
            num_blocks,
            resume_blocks: Vec::new(),
        }],
        request_timeout,
        max_attempts,
        choking_algo,
        None,
    )
    .await?;
    let entry = batch
        .pieces
        .pop()
        .expect("single-piece batch always returns its requested piece");
    Ok(ActorAwarePieceDownloadResult {
        piece: entry.result.map_err(Aria2Error::Network),
        peer_actor_ids: entry.peer_actor_ids,
        availability_changed_actor_ids: batch.availability_changed_actor_ids,
        pex_peers: batch.pex_peers,
        tracker_peers: batch.tracker_peers,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn download_piece_blocks_batch(
    swarm: &mut PeerSwarm,
    plans: &[PieceRequestPlan],
    request_timeout: Duration,
    max_attempts: u32,
    mut choking_algo: Option<&mut ChokingAlgorithm>,
    block_sink: Option<&tokio::sync::mpsc::Sender<ReceivedPieceBlock>>,
) -> Result<ActorAwarePieceBatchResult> {
    let piece_indices = plans
        .iter()
        .map(|plan| plan.piece_index)
        .collect::<Vec<_>>();
    let mut peers = PeerSchedulingSnapshot::capture(swarm);
    let mut workers = PeerGeneration::from_swarm(swarm, &piece_indices);
    let Some(mut event_lease) = swarm.lease_event_receiver() else {
        return Err(Aria2Error::Fatal(FatalError::Config(
            "torrent peer event receiver is already leased".to_string(),
        )));
    };

    let mut attempts = HashMap::<u32, u32>::with_capacity(plans.len());
    let mut completed = HashMap::<u32, PieceDownloadResult>::with_capacity(plans.len());
    let mut failures = HashMap::<u32, Vec<std::net::SocketAddr>>::with_capacity(plans.len());
    let mut errors = HashMap::<u32, String>::new();
    let mut tracker_peers = Vec::new();
    let mut pex_peers = Vec::new();
    loop {
        let pending_plans = plans
            .iter()
            .filter(|plan| {
                !completed.contains_key(&plan.piece_index)
                    && !errors.contains_key(&plan.piece_index)
            })
            .filter(|plan| {
                max_attempts == 0
                    || attempts.get(&plan.piece_index).copied().unwrap_or_default() < max_attempts
            })
            .cloned()
            .collect::<Vec<_>>();
        for plan in &pending_plans {
            let count = attempts.entry(plan.piece_index).or_default();
            *count = count.saturating_add(1);
        }
        for plan in plans {
            if !completed.contains_key(&plan.piece_index)
                && !errors.contains_key(&plan.piece_index)
                && max_attempts != 0
                && attempts.get(&plan.piece_index).copied().unwrap_or_default() >= max_attempts
            {
                errors.insert(
                    plan.piece_index,
                    format!(
                        "Failed to download piece {} after {} pipelined attempts",
                        plan.piece_index, max_attempts
                    ),
                );
            }
        }
        if pending_plans.is_empty() {
            break;
        }

        info!(
            pieces = pending_plans.len(),
            attempt = pending_plans
                .iter()
                .filter_map(|plan| attempts.get(&plan.piece_index))
                .copied()
                .max()
                .unwrap_or_default(),
            "Starting shared-window BT piece request round"
        );
        let outcome = run_attempt_batch(
            &mut workers,
            &mut event_lease,
            &pending_plans,
            &mut peers,
            choking_algo.as_deref_mut(),
            request_timeout,
            block_sink,
        )
        .await;
        let discovery_pending = outcome.discovery_pending;
        tracker_peers.extend(outcome.tracker_peers);
        pex_peers.extend(workers.take_pex_peers());
        for (piece_index, peer_failures) in outcome.failed_peers {
            let accumulated = failures.entry(piece_index).or_default();
            for peer in peer_failures {
                if !accumulated.contains(&peer) {
                    accumulated.push(peer);
                }
            }
        }
        for (piece_index, mut outcome) in outcome.completed {
            let Some(data) = outcome.data.take() else {
                continue;
            };
            let accumulated = failures.entry(piece_index).or_default();
            for peer in outcome.failed_peers {
                if !accumulated.contains(&peer) {
                    accumulated.push(peer);
                }
            }
            completed.insert(
                piece_index,
                PieceDownloadResult {
                    data,
                    peer_bytes: outcome.peer_bytes,
                    failed_peers: failures.remove(&piece_index).unwrap_or_default(),
                },
            );
        }
        if plans.iter().all(|plan| {
            completed.contains_key(&plan.piece_index) || errors.contains_key(&plan.piece_index)
        }) {
            break;
        }
        if discovery_pending {
            break;
        }
        workers.advance_generations();
    }

    workers.finish_generation(&mut event_lease).await;
    let availability_changed_actor_ids = workers
        .take_availability_changes()
        .into_iter()
        .collect::<Vec<_>>();
    pex_peers.extend(workers.take_pex_peers());
    let pieces = plans
        .iter()
        .map(|plan| {
            let result = if let Some(piece) = completed.remove(&plan.piece_index) {
                let peer_actor_ids = piece
                    .peer_bytes
                    .iter()
                    .filter_map(|peer| peers.actor_id(peer.peer_index))
                    .collect();
                return ActorAwarePieceBatchEntry {
                    piece_index: plan.piece_index,
                    result: Ok(piece),
                    peer_actor_ids,
                };
            } else {
                Err(errors
                    .remove(&plan.piece_index)
                    .unwrap_or_else(|| format!("Failed to download piece {}", plan.piece_index)))
            };
            ActorAwarePieceBatchEntry {
                piece_index: plan.piece_index,
                result,
                peer_actor_ids: Vec::new(),
            }
        })
        .collect();
    Ok(ActorAwarePieceBatchResult {
        pieces,
        availability_changed_actor_ids,
        pex_peers,
        tracker_peers,
    })
}

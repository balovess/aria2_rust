//! Public orchestration for normal pipelined BT piece downloads.

use std::collections::HashSet;
use std::time::Duration;

use tracing::info;

use crate::constants;
use crate::engine::choking_algorithm::ChokingAlgorithm;
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::AtomicProgress;

use super::super::types::{ActorAwarePieceDownloadResult, PieceDownloadResult};
use super::normal_pipeline::{piece_attempt_budget_exhausted, run_attempt};
use super::peer_actor::PeerGeneration;
use super::peer_registry::{PeerSwarm, PeerSwarmEventLease};
use super::peer_snapshot::PeerSchedulingSnapshot;

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
pub(crate) async fn download_piece_blocks(
    swarm: &mut PeerSwarm,
    piece_index: u32,
    piece_length: u32,
    num_blocks: u32,
    network_activity: Option<&AtomicProgress>,
    request_timeout: Duration,
    max_attempts: u32,
    choking_algo: Option<&mut ChokingAlgorithm>,
) -> Result<ActorAwarePieceDownloadResult> {
    let peers = PeerSchedulingSnapshot::capture(swarm, piece_index);
    let mut workers = PeerGeneration::from_swarm(swarm, piece_index).await;
    let Some(mut event_lease) = swarm.lease_event_receiver() else {
        return Err(Aria2Error::Fatal(FatalError::Config(
            "torrent peer event receiver is already leased".to_string(),
        )));
    };
    run_pipelined_attempts(
        peers,
        &mut event_lease,
        &mut workers,
        piece_index,
        piece_length,
        num_blocks,
        network_activity,
        request_timeout,
        max_attempts,
        choking_algo,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_pipelined_attempts(
    mut peers: PeerSchedulingSnapshot,
    event_rx: &mut PeerSwarmEventLease<'_>,
    workers: &mut PeerGeneration,
    piece_index: u32,
    piece_length: u32,
    num_blocks: u32,
    network_activity: Option<&AtomicProgress>,
    request_timeout: Duration,
    max_attempts: u32,
    mut choking_algo: Option<&mut ChokingAlgorithm>,
) -> Result<ActorAwarePieceDownloadResult> {
    let mut attempts = 0u32;
    let mut availability_changed_actor_ids = HashSet::new();
    loop {
        attempts = attempts.saturating_add(1);
        info!(
            "[BT] Pipelined piece download attempt {} for piece {}",
            attempts, piece_index
        );

        let outcome = run_attempt(
            workers,
            event_rx,
            piece_index,
            piece_length,
            num_blocks,
            &mut peers,
            choking_algo.as_deref_mut(),
            network_activity,
            request_timeout,
        )
        .await;

        if let Some(data) = outcome.data {
            workers.finish_generation(event_rx).await;
            availability_changed_actor_ids.extend(workers.take_availability_changes());
            let peer_actor_ids = outcome
                .peer_bytes
                .iter()
                .map(|peer| {
                    peers
                        .actor_id(peer.peer_index)
                        .expect("peer attribution index must remain in the captured snapshot")
                })
                .collect();
            info!(
                piece_index,
                blocks = num_blocks,
                bytes = data.len(),
                "BT pipelined piece completed"
            );
            return Ok(ActorAwarePieceDownloadResult {
                piece: PieceDownloadResult {
                    data,
                    peer_bytes: outcome.peer_bytes,
                    failed_peers: outcome.failed_peers,
                },
                peer_actor_ids,
                availability_changed_actor_ids: availability_changed_actor_ids
                    .into_iter()
                    .collect(),
                pex_peers: workers.take_pex_peers(),
            });
        }

        if piece_attempt_budget_exhausted(attempts, max_attempts) {
            workers.finish_generation(event_rx).await;
            availability_changed_actor_ids.extend(workers.take_availability_changes());
            break;
        }
        tokio::time::sleep(Duration::from_millis(constants::BT_RETRY_DELAY_MS)).await;
        workers.advance_generation(piece_index).await;
    }

    Err(Aria2Error::Network(format!(
        "Failed to download piece {} after {} pipelined attempts",
        piece_index,
        if max_attempts == 0 {
            attempts
        } else {
            max_attempts
        }
    )))
}

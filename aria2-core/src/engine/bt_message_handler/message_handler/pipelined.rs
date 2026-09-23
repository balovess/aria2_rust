//! Public orchestration for normal pipelined BT piece downloads.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::info;

use crate::constants;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::choking_algorithm::{ChokingAlgorithm, PeerIdentity};
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::AtomicProgress;

use super::super::types::{DEFAULT_MAX_OUTSTANDING_REQUEST, PieceDownloadResult};
use super::BtMessageHandler;
use super::normal_pipeline::{piece_attempt_budget_exhausted, run_attempt};
use super::peer_worker::PeerWorkers;

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

impl BtMessageHandler {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn download_piece_blocks_pipelined_with_sources_and_activity(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        dht_engine: Option<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        upload_provider: Option<Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>>,
        network_activity: Option<&AtomicProgress>,
        request_timeout: Duration,
        max_attempts: u32,
        mut choking_algo: Option<&mut ChokingAlgorithm>,
    ) -> Result<PieceDownloadResult> {
        let mut attempts = 0u32;
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
            let peer_indices = connections
                .iter()
                .enumerate()
                .map(|(index, connection)| (PeerIdentity::from(&connection.stats), index))
                .collect::<HashMap<_, _>>();
            let channel_capacity = connections
                .len()
                .saturating_mul(DEFAULT_MAX_OUTSTANDING_REQUEST + 4)
                .max(64);
            let (event_tx, mut event_rx) = mpsc::channel(channel_capacity);
            let mut workers = PeerWorkers::new(
                connections,
                event_tx,
                dht_engine.clone(),
                upload_provider.clone(),
            );
            let outcome = run_attempt(
                &mut workers,
                &mut event_rx,
                piece_index,
                piece_length,
                num_blocks,
                &peer_addresses,
                &has_piece,
                &peer_indices,
                choking_algo.as_deref_mut(),
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

            if piece_attempt_budget_exhausted(attempts, max_attempts) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(constants::BT_RETRY_DELAY_MS)).await;
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

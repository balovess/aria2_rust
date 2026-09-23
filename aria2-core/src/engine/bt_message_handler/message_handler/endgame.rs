//! Endgame-mode block download interface for `BtMessageHandler`.

use std::time::Duration;

use crate::engine::bt_download_execute::EndgameState;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::Result;
use crate::request::request_group::AtomicProgress;

use super::super::types::{BLOCK_REQUEST_TIMEOUT_SECS, MAX_RETRIES, PieceDownloadResult};
use super::BtMessageHandler;

impl BtMessageHandler {
    /// Download a piece by duplicating each block request across connected peers.
    ///
    /// A per-peer worker owns every connection read and write. The first valid
    /// response wins, and cancellation of redundant requests is sent through
    /// the same worker command channel.
    pub async fn download_piece_blocks_endgame_with_sources(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        endgame_state: &mut EndgameState,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    ) -> Result<PieceDownloadResult> {
        Self::download_piece_blocks_endgame_with_sources_and_activity(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            endgame_state,
            dht_engine,
            None,
        )
        .await
    }

    pub async fn download_piece_blocks_endgame_with_sources_and_activity(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        endgame_state: &mut EndgameState,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        network_activity: Option<&AtomicProgress>,
    ) -> Result<PieceDownloadResult> {
        Self::download_piece_blocks_endgame_with_sources_and_activity_with_timeout(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            endgame_state,
            dht_engine,
            network_activity,
            Duration::from_secs(BLOCK_REQUEST_TIMEOUT_SECS),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn download_piece_blocks_endgame_with_sources_and_activity_with_timeout(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        endgame_state: &mut EndgameState,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        network_activity: Option<&AtomicProgress>,
        request_timeout: Duration,
    ) -> Result<PieceDownloadResult> {
        Self::download_piece_blocks_endgame_with_sources_and_activity_with_timeout_and_max_attempts(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            endgame_state,
            dht_engine,
            network_activity,
            request_timeout,
            MAX_RETRIES,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn download_piece_blocks_endgame_with_sources_and_activity_with_timeout_and_max_attempts(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        endgame_state: &mut EndgameState,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        network_activity: Option<&AtomicProgress>,
        request_timeout: Duration,
        max_attempts: u32,
    ) -> Result<PieceDownloadResult> {
        Self::download_piece_blocks_endgame_with_sources_and_activity_with_timeout_and_max_attempts_and_provider(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            endgame_state,
            dht_engine,
            None,
            network_activity,
            request_timeout,
            max_attempts,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn download_piece_blocks_endgame_with_sources_and_activity_with_timeout_and_max_attempts_and_provider(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        endgame_state: &mut EndgameState,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        upload_provider: Option<
            std::sync::Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>,
        >,
        network_activity: Option<&AtomicProgress>,
        request_timeout: Duration,
        max_attempts: u32,
    ) -> Result<PieceDownloadResult> {
        Self::download_piece_blocks_endgame_with_sources_and_activity_with_timeout_and_max_attempts_and_provider_and_choking(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            endgame_state,
            dht_engine,
            upload_provider,
            network_activity,
            request_timeout,
            max_attempts,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn download_piece_blocks_endgame_with_sources_and_activity_with_timeout_and_max_attempts_and_provider_and_choking(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        endgame_state: &mut EndgameState,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        upload_provider: Option<
            std::sync::Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>,
        >,
        network_activity: Option<&AtomicProgress>,
        request_timeout: Duration,
        max_attempts: u32,
        choking_algo: Option<&mut crate::engine::choking_algorithm::ChokingAlgorithm>,
    ) -> Result<PieceDownloadResult> {
        Self::download_piece_blocks_endgame_pipelined_with_sources_and_activity(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            endgame_state,
            dht_engine,
            upload_provider,
            network_activity,
            request_timeout,
            max_attempts,
            choking_algo,
        )
        .await
    }

    pub async fn download_piece_blocks_endgame(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        endgame_state: &mut EndgameState,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    ) -> Result<Vec<u8>> {
        Ok(Self::download_piece_blocks_endgame_with_sources(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            endgame_state,
            dht_engine,
        )
        .await?
        .data)
    }
}

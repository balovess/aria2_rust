//! Normal-mode block request and download methods for BtMessageHandler.

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::Result;
use crate::request::request_group::AtomicProgress;
use tracing::{debug, trace};

use super::super::types::{BLOCK_REQUEST_TIMEOUT_SECS, MAX_RETRIES};
use super::BtMessageHandler;

impl BtMessageHandler {
    /// Try to decode a ut_pex Extended message received during block read.
    ///
    /// On success, discovered peers are appended to `conn.pending_pex_peers`
    /// for the download loop to drain and connect. On parse failure, the
    /// message is silently ignored (it might be ut_metadata or another
    /// extension we don't handle here).
    pub(crate) fn try_process_pex_during_read(conn: &mut BtPeerConn, ext_id: u8, payload: &[u8]) {
        if !conn.is_pex_enabled() {
            return;
        }

        use aria2_protocol::bittorrent::message::extension::UtPexMessage;
        use aria2_protocol::bittorrent::peer::connection::PeerAddr;

        match UtPexMessage::from_payload(payload) {
            Ok(pex_msg) => {
                // Convert compact IPv4 peers to PeerAddr
                for compact in &pex_msg.added {
                    let ip = std::net::Ipv4Addr::from(*compact.ip());
                    let addr = PeerAddr::new(&ip.to_string(), compact.port());
                    conn.pending_pex_peers.push(addr);
                }

                // Convert compact IPv6 peers to PeerAddr
                for compact in &pex_msg.added6 {
                    let ip = std::net::Ipv6Addr::from(*compact.ip());
                    let addr = PeerAddr::new(&ip.to_string(), compact.port());
                    conn.pending_pex_peers.push(addr);
                }

                if !pex_msg.added.is_empty() || !pex_msg.added6.is_empty() {
                    debug!(
                        "[BT] PEX during block read: ext_id={}, {} v4 + {} v6 peers (buffered for download loop)",
                        ext_id,
                        pex_msg.added.len(),
                        pex_msg.added6.len()
                    );
                }
            }
            Err(_) => {
                // Not a valid PEX payload — likely ut_metadata or another
                // extension. Silently ignore; no harm done.
                trace!(
                    "[BT] Extended message ext_id={} not recognized as PEX during block read",
                    ext_id
                );
            }
        }
    }

    /// Download all blocks for a piece with retry logic
    ///
    /// Coordinates the download of all blocks that make up a piece,
    /// implementing retry logic for failed pieces.
    ///
    /// # Arguments
    /// * `connections` - Mutable slice of active peer connections
    /// * `piece_index` - Index of the piece to download
    /// * `piece_length` - Total length of this piece in bytes
    /// * `num_blocks` - Number of blocks in this piece
    ///
    /// # Returns
    /// * `Ok(Vec<u8>)` - Complete piece data if all blocks downloaded successfully
    /// * `Err(Aria2Error)` - If piece download fails after all retries
    pub async fn download_piece_blocks_with_sources(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    ) -> Result<super::super::types::PieceDownloadResult> {
        Self::download_piece_blocks_with_sources_and_activity(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            dht_engine,
            None,
        )
        .await
    }

    pub async fn download_piece_blocks_with_sources_and_activity(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        network_activity: Option<&AtomicProgress>,
    ) -> Result<super::super::types::PieceDownloadResult> {
        Self::download_piece_blocks_with_sources_and_activity_with_timeout(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            dht_engine,
            network_activity,
            std::time::Duration::from_secs(BLOCK_REQUEST_TIMEOUT_SECS),
        )
        .await
    }

    pub async fn download_piece_blocks_with_sources_and_activity_with_timeout(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        network_activity: Option<&AtomicProgress>,
        request_timeout: std::time::Duration,
    ) -> Result<super::super::types::PieceDownloadResult> {
        Self::download_piece_blocks_with_sources_and_activity_with_timeout_and_max_attempts(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            dht_engine,
            network_activity,
            request_timeout,
            MAX_RETRIES,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn download_piece_blocks_with_sources_and_activity_with_timeout_and_max_attempts(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        network_activity: Option<&AtomicProgress>,
        request_timeout: std::time::Duration,
        max_attempts: u32,
    ) -> Result<super::super::types::PieceDownloadResult> {
        Self::download_piece_blocks_with_sources_and_activity_with_timeout_and_max_attempts_and_provider(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            dht_engine,
            None,
            network_activity,
            request_timeout,
            max_attempts,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn download_piece_blocks_with_sources_and_activity_with_timeout_and_max_attempts_and_provider(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        upload_provider: Option<
            std::sync::Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>,
        >,
        network_activity: Option<&AtomicProgress>,
        request_timeout: std::time::Duration,
        max_attempts: u32,
    ) -> Result<super::super::types::PieceDownloadResult> {
        Self::download_piece_blocks_pipelined_with_sources_and_activity(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            dht_engine,
            upload_provider,
            network_activity,
            request_timeout,
            max_attempts,
        )
        .await
    }

    pub async fn download_piece_blocks(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    ) -> Result<Vec<u8>> {
        Ok(Self::download_piece_blocks_with_sources(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            dht_engine,
        )
        .await?
        .data)
    }
}

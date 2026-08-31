//! Normal-mode block request and download methods for BtMessageHandler.

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::request::request_group::AtomicProgress;
use tracing::{debug, trace, warn};

use super::super::types::{
    BLOCK_REQUEST_TIMEOUT_SECS, BLOCK_SIZE, BlockDownloadResult, MAX_BLOCK_READ_MESSAGES,
    MAX_RETRIES,
};
use super::BtMessageHandler;

impl BtMessageHandler {
    /// Request and receive a single block from available peers
    ///
    /// This method implements the core block request/receive loop:
    /// 1. Send the block request to a peer
    /// 2. Wait for the response with timeout
    /// 3. Handle various message types while waiting
    /// 4. Return the block data on success
    ///
    /// # Arguments
    /// * `connections` - Mutable slice of active peer connections
    /// * `piece_index` - The index of the piece this block belongs to
    /// * `block_offset` - The byte offset within the piece
    /// * `block_length` - The length of this block in bytes
    ///
    /// # Returns
    /// * `Ok(BlockDownloadResult)` - Result containing success status and data
    /// * `Err(Aria2Error)` - If all peers fail to respond
    pub async fn request_block(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        block_offset: u32,
        block_length: u32,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    ) -> Result<BlockDownloadResult> {
        Self::request_block_with_timeout(
            connections,
            piece_index,
            block_offset,
            block_length,
            dht_engine,
            std::time::Duration::from_secs(BLOCK_REQUEST_TIMEOUT_SECS),
        )
        .await
    }

    pub async fn request_block_with_timeout(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        block_offset: u32,
        block_length: u32,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        request_timeout: std::time::Duration,
    ) -> Result<BlockDownloadResult> {
        let req = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
            index: piece_index,
            begin: block_offset,
            length: block_length,
        };

        debug!(
            "[BT] Requesting block {} offset={} len={}",
            block_offset / BLOCK_SIZE,
            block_offset,
            block_length
        );

        let mut total_bytes = 0u64;
        let mut failed_peers = Vec::new();

        // Try each peer in order until we get the block
        for (conn_idx, conn) in connections.iter_mut().enumerate() {
            debug!("[BT] Trying peer {} for block request", conn_idx);

            // Send request to this peer
            if conn.send_request(req.clone()).await.is_err() {
                warn!("[BT] Failed to send request to peer {}", conn_idx);
                if let Ok(addr) = format!("{}:{}", conn.ip_addr, conn.port).parse() {
                    failed_peers.push(addr);
                }
                continue;
            }

            // Wait for response with timeout
            match tokio::time::timeout(
                request_timeout,
                Self::wait_for_piece_block(conn, piece_index, block_offset, dht_engine.clone()),
            )
            .await
            {
                Ok(Ok(data)) => {
                    debug!(
                        "[BT] Got block {} data len={} from peer {}",
                        block_offset / BLOCK_SIZE,
                        data.len(),
                        conn_idx
                    );
                    total_bytes += data.len() as u64;

                    return Ok(BlockDownloadResult {
                        success: true,
                        data: Some(data),
                        peer_index: Some(conn_idx),
                        bytes_received: total_bytes,
                        failed_peers,
                    });
                }
                Ok(Err(e)) => {
                    warn!(
                        "[BT] No PIECE message received from peer {}: {}",
                        conn_idx, e
                    );
                    if let Ok(addr) = format!("{}:{}", conn.ip_addr, conn.port).parse() {
                        failed_peers.push(addr);
                    }
                }
                Err(_) => {
                    warn!(
                        "[BT] Block request timed out after {}s",
                        request_timeout.as_secs()
                    );
                    if let Ok(addr) = format!("{}:{}", conn.ip_addr, conn.port).parse() {
                        failed_peers.push(addr);
                    }
                }
            }
        }

        // All peers failed
        warn!("[BT] Failed to get block from any peer");
        Ok(BlockDownloadResult {
            success: false,
            data: None,
            peer_index: None,
            bytes_received: total_bytes,
            failed_peers,
        })
    }

    /// Wait for a specific PIECE message from a peer
    ///
    /// Reads messages from the connection until we receive the expected
    /// piece block or exhaust our message limit. While waiting, processes
    /// other message types including BEP 10/11 Extended messages (PEX).
    pub(crate) async fn wait_for_piece_block(
        conn: &mut BtPeerConn,
        expected_index: u32,
        expected_begin: u32,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    ) -> Result<bytes::Bytes> {
        for _ in 0..MAX_BLOCK_READ_MESSAGES {
            match conn.read_message().await {
                Ok(Some(msg)) => {
                    use aria2_protocol::bittorrent::message::types::BtMessage;

                    match msg {
                        BtMessage::Piece { index, begin, data } => {
                            if index == expected_index && begin == expected_begin {
                                return Ok(data);
                            }
                            // Not the block we're waiting for, continue reading
                            debug!(
                                "[BT] Received unexpected PIECE (index={}, begin={}), waiting for ({}, {})",
                                index, begin, expected_index, expected_begin
                            );
                        }
                        BtMessage::Port { port } => {
                            // BEP 5: peer tells us its DHT port. Add
                            // (peer_ip, port) as a DHT node candidate.
                            // add_node pings synchronously, so spawn it.
                            if port != 0 && !conn.ip_addr.is_empty() {
                                let addr = format!("{}:{}", conn.ip_addr, port).parse();
                                if let Ok(addr) = addr
                                    && let Some(eng) = dht_engine.clone()
                                {
                                    trace!("[BT] DHT port message: adding node {}", addr);
                                    tokio::spawn(async move {
                                        eng.add_node(addr).await;
                                    });
                                }
                            } else {
                                debug!("[BT] Received Port(0) or unknown peer ip, ignoring");
                            }
                        }
                        BtMessage::Extended {
                            ext_id,
                            ref payload,
                        } => {
                            // BEP 10/11: process Extended messages received
                            // during block reads. For ut_pex (BEP 11), decode
                            // the compact peers and stash them on the connection
                            // for the download loop to drain later.
                            if ext_id == 0 {
                                // Extension handshake — log only; the full
                                // handshake is handled by BtPeerInteractive.
                                trace!(
                                    "[BT] Received Extension Handshake during block read (payload_len={})",
                                    payload.len()
                                );
                            } else {
                                // Only the peer's negotiated ut_pex ID can carry
                                // PEX data. Unknown extension IDs must not be
                                // classified by payload shape because they may be
                                // ut_metadata or another BEP 10 extension.
                                if conn.peer_extension_id("ut_pex") == Some(ext_id) {
                                    Self::try_process_pex_during_read(conn, ext_id, payload);
                                } else {
                                    trace!(
                                        "[BT] Ignoring non-ut_pex extension during block read: ext_id={}",
                                        ext_id
                                    );
                                }
                            }
                        }
                        other => {
                            use aria2_protocol::bittorrent::message::types::BtMessage;
                            match &other {
                                BtMessage::AllowedFast { index } => {
                                    debug!("[BT] Received AllowedFast for piece {}", index);
                                    conn.add_allowed_fast(*index);
                                }
                                BtMessage::Reject {
                                    index,
                                    offset,
                                    length,
                                } => {
                                    debug!(
                                        "[BT] Received Reject for piece {} offset {} len {}",
                                        index, offset, length
                                    );
                                }
                                BtMessage::Suggest { index } => {
                                    debug!("[BT] Received Suggest for piece {}", index);
                                    debug!(
                                        "[BT] Suggest received for piece {} — would boost priority",
                                        index
                                    );
                                }
                                BtMessage::HaveAll => {
                                    debug!("[BT] Received HaveAll");
                                }
                                BtMessage::HaveNone => {
                                    debug!("[BT] Received HaveNone");
                                }
                                _ => {
                                    debug!(
                                        "[BT] Received non-PIECE message while waiting: {:?}",
                                        other
                                    );
                                }
                            }
                        }
                    }
                }
                Ok(None) => {
                    warn!("[BT] Connection closed by peer while waiting for block");
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: "Peer connection closed".into(),
                        },
                    ));
                }
                Err(e) => {
                    warn!("[BT] Error reading from peer: {}", e);
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: format!("Read error: {}", e),
                        },
                    ));
                }
            }
        }

        Err(Aria2Error::Recoverable(
            RecoverableError::TemporaryNetworkFailure {
                message: format!(
                    "Exceeded max messages ({}) without receiving expected block",
                    MAX_BLOCK_READ_MESSAGES
                ),
            },
        ))
    }

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
        Self::download_piece_blocks_pipelined_with_sources_and_activity(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            dht_engine,
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

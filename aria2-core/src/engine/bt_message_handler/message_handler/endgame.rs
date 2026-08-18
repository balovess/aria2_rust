//! Endgame-mode block request and download methods for BtMessageHandler.

use futures::{StreamExt, stream::FuturesUnordered};

use crate::engine::bt_download_execute::EndgameState;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use tracing::{debug, info, warn};

use super::super::types::{
    BLOCK_REQUEST_TIMEOUT_SECS, BLOCK_SIZE, BlockDownloadResult, MAX_RETRIES, PeerDownloadBytes,
    PieceDownloadResult,
};
use super::BtMessageHandler;

async fn wait_for_piece_block_from_peer(
    connection: &mut BtPeerConn,
    conn_idx: usize,
    expected_index: u32,
    expected_begin: u32,
    dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
) -> Result<(Vec<u8>, usize)> {
    loop {
        match connection.read_message().await {
            Ok(Some(msg)) => {
                use aria2_protocol::bittorrent::message::types::BtMessage;

                match msg {
                    BtMessage::Piece { index, begin, data } => {
                        if index == expected_index && begin == expected_begin {
                            return Ok((data, conn_idx));
                        }
                        debug!(
                            "[BT] Endgame: Received unexpected PIECE (index={}, begin={}) from peer {}, waiting for ({}, {})",
                            index, begin, conn_idx, expected_index, expected_begin
                        );
                    }
                    BtMessage::AllowedFast { index } => {
                        debug!("[BT] Received AllowedFast for piece {}", index);
                        connection.add_allowed_fast(index);
                    }
                    BtMessage::Port { port } => {
                        // BEP 5: add (peer_ip, port) as a DHT node.
                        if port != 0 && !connection.ip_addr.is_empty() {
                            let addr = format!("{}:{}", connection.ip_addr, port).parse();
                            if let Ok(addr) = addr
                                && let Some(engine) = dht_engine.clone()
                            {
                                tokio::spawn(async move {
                                    engine.add_node(addr).await;
                                });
                            }
                        }
                    }
                    other => {
                        debug!(
                            "[BT] Endgame: Received non-PIECE message while waiting: {:?}",
                            other
                        );
                    }
                }
            }
            Ok(None) => {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!(
                            "BT peer {} closed while waiting for a piece block",
                            conn_idx
                        ),
                    },
                ));
            }
            Err(error) => return Err(error),
        }
    }
}

impl BtMessageHandler {
    /// Download all blocks for a piece using endgame mode (duplicate request strategy).
    ///
    /// In endgame mode, when few pieces remain, we request each block from ALL available
    /// peers simultaneously. When any peer responds first, we immediately send Cancel
    /// messages to the other peers to stop them from sending redundant data.
    ///
    /// # Phase 14 - B1/B2: Endgame Duplicate Request Strategy + Cancel on Block Arrival
    ///
    /// # Arguments
    /// * `connections` - Mutable slice of active peer connections
    /// * `piece_index` - Index of the piece to download
    /// * `piece_length` - Total length of this piece in bytes
    /// * `num_blocks` - Number of blocks in this piece
    /// * `endgame_state` - Mutable reference to EndgameState for tracking duplicate requests
    ///
    /// # Returns
    /// * `Ok(Vec<u8>)` - Complete piece data if all blocks downloaded successfully
    /// * `Err(Aria2Error)` - If piece download fails after all retries
    pub async fn download_piece_blocks_endgame_with_sources(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        endgame_state: &mut EndgameState,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    ) -> Result<PieceDownloadResult> {
        let mut peer_bytes = Vec::with_capacity(num_blocks as usize);
        let mut failed_peers = Vec::new();
        let data = Self::download_piece_blocks_endgame_inner(
            connections,
            piece_index,
            piece_length,
            num_blocks,
            endgame_state,
            dht_engine,
            &mut peer_bytes,
            &mut failed_peers,
        )
        .await?;
        Ok(PieceDownloadResult {
            data,
            peer_bytes,
            failed_peers,
        })
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

    #[allow(clippy::too_many_arguments)]
    async fn download_piece_blocks_endgame_inner(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        endgame_state: &mut EndgameState,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        peer_bytes: &mut Vec<PeerDownloadBytes>,
        failed_peers: &mut Vec<std::net::SocketAddr>,
    ) -> Result<Vec<u8>> {
        // Retry the entire piece multiple times (same as normal mode)
        for _retry in 0..MAX_RETRIES {
            peer_bytes.clear();
            failed_peers.clear();
            info!(
                "[BT] Endgame piece download attempt {} for piece {} ({} peers)",
                _retry + 1,
                piece_index,
                connections.len()
            );

            let mut piece_data = Vec::with_capacity(piece_length as usize);
            piece_data.clear();
            let mut all_blocks_ok = true;

            // Download each block using endgame strategy
            for block_idx in 0..num_blocks {
                let offset = block_idx * BLOCK_SIZE;
                let len = if offset + BLOCK_SIZE > piece_length {
                    piece_length - offset
                } else {
                    BLOCK_SIZE
                };

                debug!(
                    "[BT] Endgame: requesting block {}/{} (offset={}, len={}) from all {} peers",
                    block_idx + 1,
                    num_blocks,
                    offset,
                    len,
                    connections.len()
                );

                // Phase 14 - B1: Request this block from ALL peers and track duplicates
                match Self::request_block_endgame(
                    connections,
                    piece_index,
                    offset,
                    len,
                    endgame_state,
                    dht_engine.clone(),
                )
                .await
                {
                    Ok(result) if result.success => {
                        failed_peers.extend(result.failed_peers);
                        if let Some(data) = result.data {
                            if let Some(peer_index) = result.peer_index
                                && let Some(peer) = connections.get(peer_index)
                                && let Ok(ip) = peer.ip_addr.parse()
                            {
                                let address = std::net::SocketAddr::new(ip, peer.port);
                                let bytes = result.bytes_received;
                                if let Some(entry) = peer_bytes
                                    .iter_mut()
                                    .find(|item| item.peer_index == peer_index)
                                {
                                    entry.bytes += bytes;
                                } else {
                                    peer_bytes.push(PeerDownloadBytes {
                                        peer_index,
                                        peer: address,
                                        bytes,
                                    });
                                }
                            }
                            // Phase 14 - B2: Cancel redundant requests now that we have the block
                            Self::cancel_redundant_requests(
                                connections,
                                piece_index,
                                offset,
                                len,
                                endgame_state,
                            )
                            .await;

                            piece_data.extend_from_slice(&data);
                        } else {
                            all_blocks_ok = false;
                            break;
                        }
                    }
                    Ok(result) => {
                        failed_peers.extend(result.failed_peers);
                        warn!("[BT] Endgame: Block {} request returned no data", block_idx);
                        all_blocks_ok = false;
                        break;
                    }
                    Err(e) => {
                        warn!("[BT] Endgame: Block {} request error: {}", block_idx, e);
                        all_blocks_ok = false;
                        break;
                    }
                }
            }

            // Check if we got all blocks
            if all_blocks_ok && piece_data.len() == piece_length as usize {
                info!(
                    "[BT] Endgame: All {} blocks downloaded for piece {} ({} bytes)",
                    num_blocks,
                    piece_index,
                    piece_data.len()
                );
                return Ok(piece_data);
            }

            warn!(
                "[BT] Endgame: Incomplete piece {} (attempt {}/{}), retrying...",
                piece_index,
                _retry + 1,
                MAX_RETRIES
            );

            // Small delay before retry
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        Err(Aria2Error::Fatal(FatalError::Config(format!(
            "Failed to download piece {} in endgame mode after {} attempts",
            piece_index, MAX_RETRIES
        ))))
    }

    /// Request a single block from all peers during endgame mode.
    ///
    /// Sends the same block request to every connected peer simultaneously.
    /// Tracks each request in the EndgameState so we can cancel redundant ones later.
    ///
    /// # Phase 14 - B1: Endgame Duplicate Request Strategy
    pub(crate) async fn request_block_endgame(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        block_offset: u32,
        block_length: u32,
        endgame_state: &mut EndgameState,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    ) -> Result<BlockDownloadResult> {
        let req = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
            index: piece_index,
            begin: block_offset,
            length: block_length,
        };

        let mut total_bytes = 0u64;
        let mut failed_peers = Vec::new();

        // Phase 14 - B1: Send request to ALL peers (not just one)
        for (conn_idx, conn) in connections.iter_mut().enumerate() {
            debug!(
                "[BT] Endgame: Sending duplicate request for block {} to peer {}",
                block_offset / BLOCK_SIZE,
                conn_idx
            );

            // Send request to this peer
            if conn.send_request(req.clone()).await.is_err() {
                warn!(
                    "[BT] Endgame: Failed to send request to peer {}, skipping",
                    conn_idx
                );
                if let Ok(addr) = format!("{}:{}", conn.ip_addr, conn.port).parse() {
                    failed_peers.push(addr);
                }
                continue;
            }

            // Track this duplicate request in endgame state
            let peer_key = format!("{}:{}", conn.ip_addr, conn.port)
                .parse()
                .map(crate::engine::bt_download_execute::types::PeerKey::new)
                .map_err(|_| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: "invalid peer address".to_string(),
                    })
                })?;
            endgame_state.track_request(piece_index, block_offset, block_length, peer_key);
        }

        // Now wait for the FIRST response from any peer (others will be cancelled later)
        match tokio::time::timeout(
            std::time::Duration::from_secs(BLOCK_REQUEST_TIMEOUT_SECS),
            Self::wait_for_any_piece_block(
                connections,
                piece_index,
                block_offset,
                dht_engine.clone(),
            ),
        )
        .await
        {
            Ok(Ok((data, _peer_idx))) => {
                debug!(
                    "[BT] Endgame: Got block {} data len={} (will cancel {} duplicates)",
                    block_offset / BLOCK_SIZE,
                    data.len(),
                    endgame_state
                        .get_cancel_targets(piece_index, block_offset, block_length)
                        .len()
                        .saturating_sub(1)
                );
                total_bytes += data.len() as u64;

                return Ok(BlockDownloadResult {
                    success: true,
                    data: Some(data),
                    peer_index: Some(_peer_idx),
                    bytes_received: total_bytes,
                    failed_peers,
                });
            }
            Ok(Err(e)) => {
                warn!(
                    "[BT] Endgame: No PIECE message received from any peer: {}",
                    e
                );
            }
            Err(_) => {
                warn!(
                    "[BT] Endgame: Block request timed out after {}s",
                    BLOCK_REQUEST_TIMEOUT_SECS
                );
            }
        }

        // All peers failed or timed out
        warn!("[BT] Endgame: Failed to get block from any peer");
        Ok(BlockDownloadResult {
            success: false,
            data: None,
            peer_index: None,
            bytes_received: total_bytes,
            failed_peers,
        })
    }

    /// Wait for a specific PIECE message from ANY peer.
    ///
    /// Unlike `wait_for_piece_block` which waits on a single connection,
    /// this polls all connections until the expected block arrives.
    pub(crate) async fn wait_for_any_piece_block(
        connections: &mut [BtPeerConn],
        expected_index: u32,
        expected_begin: u32,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    ) -> Result<(Vec<u8>, usize)> {
        // Keep one read future per peer so a slow connection cannot block a
        // responsive peer. Each future owns the connection borrow until it
        // either produces the expected block or becomes unusable.
        let mut readers = FuturesUnordered::new();
        for (conn_idx, connection) in connections.iter_mut().enumerate() {
            readers.push(wait_for_piece_block_from_peer(
                connection,
                conn_idx,
                expected_index,
                expected_begin,
                dht_engine.clone(),
            ));
        }

        let mut last_error = None;
        while let Some(result) = readers.next().await {
            match result {
                Ok(block) => return Ok(block),
                Err(error) => {
                    debug!("[BT] Endgame peer reader stopped: {}", error);
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "No connected peer returned the expected block".to_string(),
            })
        }))
    }

    /// Cancel redundant requests for a completed block.
    ///
    /// After receiving a block from one peer during endgame mode, sends Cancel
    /// messages to all other peers that were sent duplicate requests for the same block.
    ///
    /// # Phase 14 - B2: Cancel Redundant Requests on Block Arrival
    pub(crate) async fn cancel_redundant_requests(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        offset: u32,
        len: u32,
        endgame_state: &mut EndgameState,
    ) {
        // Get list of peers that have pending requests for this block
        let targets = endgame_state.get_cancel_targets(piece_index, offset, len);

        if targets.is_empty() {
            debug!(
                "[BT] Endgame: No redundant requests to cancel for piece {} block {}",
                piece_index,
                offset / BLOCK_SIZE
            );
            return;
        }

        let cancel_req = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
            index: piece_index,
            begin: offset,
            length: len,
        };

        debug!(
            "[BT] Endgame: Cancelling {} redundant requests for piece {} block offset={}",
            targets.len(),
            piece_index,
            offset
        );

        // Send Cancel to each peer that had a pending request
        for peer_key in targets {
            if let Some(conn) = connections.iter_mut().find(|conn| {
                format!("{}:{}", conn.ip_addr, conn.port)
                    .parse::<std::net::SocketAddr>()
                    .ok()
                    .is_some_and(|address| {
                        crate::engine::bt_download_execute::types::PeerKey::new(address) == peer_key
                    })
            }) {
                match conn.send_cancel(&cancel_req).await {
                    Ok(()) => {
                        debug!(
                            "[BT] Endgame: Sent Cancel to peer {} for piece {} offset={} len={}",
                            peer_key.address(),
                            piece_index,
                            offset,
                            len
                        );
                    }
                    Err(e) => {
                        warn!(
                            "[BT] Endgame: Failed to send Cancel to peer {}: {}",
                            peer_key.address(),
                            e
                        );
                    }
                }
            }
        }

        // Remove the tracked request since we've handled it
        endgame_state.remove_request(piece_index, offset, len);
    }
}

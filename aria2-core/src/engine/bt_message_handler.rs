//! BT Message Handler - Block request and receive logic
//!
//! This module handles the low-level BitTorrent protocol message processing
//! for block requests and data reception during piece download.
//!
//! Extracted from `bt_download_command.rs` to improve modularity and
//! follow the single responsibility principle.
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/BtMessageDispatcher.h` - Message dispatching
//! - `src/PeerInteractionCommand.h` - Peer interaction

use crate::engine::bt_download_execute::EndgameState;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use tracing::{debug, info, warn};

/// Block size for each piece block request (16 KB)
pub const BLOCK_SIZE: u32 = 16384;

/// Maximum number of retries for a failed piece download
pub const MAX_RETRIES: u32 = 3;

/// Timeout for each block request (seconds)
pub const BLOCK_REQUEST_TIMEOUT_SECS: u64 = 3;

/// Maximum messages to read while waiting for a specific block
pub const MAX_BLOCK_READ_MESSAGES: u32 = 10000;

/// Result of a block download attempt
pub struct BlockDownloadResult {
    /// Whether the block was successfully received
    pub success: bool,
    /// The received data (if successful)
    pub data: Option<Vec<u8>>,
    /// Number of bytes received (for statistics)
    pub bytes_received: u64,
}

/// BT Message Handler for block-level operations
///
/// Manages the process of requesting and receiving individual blocks
/// from peers during piece download.
pub struct BtMessageHandler;

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

        // Try each peer in order until we get the block
        for (conn_idx, conn) in connections.iter_mut().enumerate() {
            debug!("[BT] Trying peer {} for block request", conn_idx);

            // Send request to this peer
            if conn.send_request(req.clone()).await.is_err() {
                warn!("[BT] Failed to send request to peer {}", conn_idx);
                continue;
            }

            // Wait for response with timeout
            match tokio::time::timeout(
                std::time::Duration::from_secs(BLOCK_REQUEST_TIMEOUT_SECS),
                Self::wait_for_piece_block(conn, piece_index, block_offset),
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
                        bytes_received: total_bytes,
                    });
                }
                Ok(Err(e)) => {
                    warn!(
                        "[BT] No PIECE message received from peer {}: {}",
                        conn_idx, e
                    );
                }
                Err(_) => {
                    warn!(
                        "[BT] Block request timed out after {}s",
                        BLOCK_REQUEST_TIMEOUT_SECS
                    );
                }
            }
        }

        // All peers failed
        warn!("[BT] Failed to get block from any peer");
        Ok(BlockDownloadResult {
            success: false,
            data: None,
            bytes_received: total_bytes,
        })
    }

    /// Wait for a specific PIECE message from a peer
    ///
    /// Reads messages from the connection until we receive the expected
    /// piece block or exhaust our message limit.
    async fn wait_for_piece_block(
        conn: &mut BtPeerConn,
        expected_index: u32,
        expected_begin: u32,
    ) -> Result<Vec<u8>> {
        for _ in 0..MAX_BLOCK_READ_MESSAGES {
            match conn.read_message().await {
                Ok(Some(msg)) => {
                    use aria2_protocol::bittorrent::message::types::BtMessage;

                    match msg {
                        BtMessage::Piece {
                            index,
                            begin,
                            ref data,
                        } => {
                            if index == expected_index && begin == expected_begin {
                                return Ok(data.clone());
                            }
                            // Not the block we're waiting for, continue reading
                            debug!(
                                "[BT] Received unexpected PIECE (index={}, begin={}), waiting for ({}, {})",
                                index, begin, expected_index, expected_begin
                            );
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
                                    // Note: Priority boost would be applied here if we had
                                    // access to the piece picker. For now, just log it.
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
    pub async fn download_piece_blocks(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
    ) -> Result<Vec<u8>> {
        // Retry the entire piece multiple times
        for _retry in 0..MAX_RETRIES {
            info!(
                "[BT] Piece download attempt {} for piece {}",
                _retry + 1,
                piece_index
            );

            // Ensure clean state for each retry attempt
            let mut piece_data = Vec::with_capacity(piece_length as usize);
            piece_data.clear();
            let mut all_blocks_ok = true;

            // Download each block in sequence
            for block_idx in 0..num_blocks {
                let offset = block_idx * BLOCK_SIZE;
                let len = if offset + BLOCK_SIZE > piece_length {
                    piece_length - offset
                } else {
                    BLOCK_SIZE
                };

                debug!(
                    "[BT] Requesting block {}/{} (offset={}, len={})",
                    block_idx + 1,
                    num_blocks,
                    offset,
                    len
                );

                // Try to get this block from any peer
                match Self::request_block(connections, piece_index, offset, len).await {
                    Ok(result) if result.success => {
                        if let Some(data) = result.data {
                            piece_data.extend_from_slice(&data);
                        } else {
                            all_blocks_ok = false;
                            break;
                        }
                    }
                    Ok(_) => {
                        warn!("[BT] Block {} request returned no data", block_idx);
                        all_blocks_ok = false;
                        break;
                    }
                    Err(e) => {
                        warn!("[BT] Block {} request error: {}", block_idx, e);
                        all_blocks_ok = false;
                        break;
                    }
                }
            }

            // Check if we got all blocks
            if all_blocks_ok && piece_data.len() == piece_length as usize {
                info!(
                    "[BT] All {} blocks downloaded for piece {} ({} bytes)",
                    num_blocks,
                    piece_index,
                    piece_data.len()
                );
                return Ok(piece_data);
            }

            warn!(
                "[BT] Incomplete piece {} (attempt {}/{}), retrying...",
                piece_index,
                _retry + 1,
                MAX_RETRIES
            );

            // Small delay before retry
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        Err(Aria2Error::Fatal(FatalError::Config(format!(
            "Failed to download piece {} after {} attempts",
            piece_index, MAX_RETRIES
        ))))
    }

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
    pub async fn download_piece_blocks_endgame(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        piece_length: u32,
        num_blocks: u32,
        endgame_state: &mut EndgameState,
    ) -> Result<Vec<u8>> {
        // Retry the entire piece multiple times (same as normal mode)
        for _retry in 0..MAX_RETRIES {
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
                )
                .await
                {
                    Ok(result) if result.success => {
                        if let Some(data) = result.data {
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
                    Ok(_) => {
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
    async fn request_block_endgame(
        connections: &mut [BtPeerConn],
        piece_index: u32,
        block_offset: u32,
        block_length: u32,
        endgame_state: &mut EndgameState,
    ) -> Result<BlockDownloadResult> {
        let req = aria2_protocol::bittorrent::message::types::PieceBlockRequest {
            index: piece_index,
            begin: block_offset,
            length: block_length,
        };

        let mut total_bytes = 0u64;

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
                continue;
            }

            // Track this duplicate request in endgame state
            endgame_state.track_request(piece_index, block_offset, block_length, conn_idx);
        }

        // Now wait for the FIRST response from any peer (others will be cancelled later)
        match tokio::time::timeout(
            std::time::Duration::from_secs(BLOCK_REQUEST_TIMEOUT_SECS),
            Self::wait_for_any_piece_block(connections, piece_index, block_offset),
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
                    bytes_received: total_bytes,
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
            bytes_received: total_bytes,
        })
    }

    /// Wait for a specific PIECE message from ANY peer.
    ///
    /// Unlike `wait_for_piece_block` which waits on a single connection,
    /// this polls all connections until the expected block arrives.
    async fn wait_for_any_piece_block(
        connections: &mut [BtPeerConn],
        expected_index: u32,
        expected_begin: u32,
    ) -> Result<(Vec<u8>, usize)> {
        // Poll each connection in round-robin fashion
        for _ in 0..MAX_BLOCK_READ_MESSAGES {
            for (conn_idx, conn) in connections.iter_mut().enumerate() {
                match conn.read_message().await {
                    Ok(Some(msg)) => {
                        use aria2_protocol::bittorrent::message::types::BtMessage;

                        match msg {
                            BtMessage::Piece {
                                index,
                                begin,
                                ref data,
                            } => {
                                if index == expected_index && begin == expected_begin {
                                    return Ok((data.clone(), conn_idx));
                                }
                                // Not the block we're waiting for, continue
                                debug!(
                                    "[BT] Endgame: Received unexpected PIECE (index={}, begin={}) from peer {}, waiting for ({}, {})",
                                    index, begin, conn_idx, expected_index, expected_begin
                                );
                            }
                            BtMessage::AllowedFast { index } => {
                                debug!("[BT] Received AllowedFast for piece {}", index);
                                conn.add_allowed_fast(index);
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
                        // Connection closed by this peer, try next
                        debug!("[BT] Endgame: Peer {} connection closed", conn_idx);
                    }
                    Err(e) => {
                        debug!("[BT] Endgame: Error reading from peer {}: {}", conn_idx, e);
                    }
                }
            }
        }

        Err(Aria2Error::Recoverable(
            RecoverableError::TemporaryNetworkFailure {
                message: format!(
                    "Exceeded max messages ({}) without receiving expected block from any peer",
                    MAX_BLOCK_READ_MESSAGES
                ),
            },
        ))
    }

    /// Cancel redundant requests for a completed block.
    ///
    /// After receiving a block from one peer during endgame mode, sends Cancel
    /// messages to all other peers that were sent duplicate requests for the same block.
    ///
    /// # Phase 14 - B2: Cancel Redundant Requests on Block Arrival
    async fn cancel_redundant_requests(
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
        for peer_id in targets {
            if let Some(conn) = connections.get_mut(peer_id) {
                match conn.send_cancel(&cancel_req).await {
                    Ok(()) => {
                        debug!(
                            "[BT] Endgame: Sent Cancel to peer {} for piece {} offset={} len={}",
                            peer_id, piece_index, offset, len
                        );
                    }
                    Err(e) => {
                        warn!(
                            "[BT] Endgame: Failed to send Cancel to peer {}: {}",
                            peer_id, e
                        );
                    }
                }
            }
        }

        // Remove the tracked request since we've handled it
        endgame_state.remove_request(piece_index, offset, len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_size_constant() {
        assert_eq!(BLOCK_SIZE, 16384);
        assert_eq!(BLOCK_SIZE, 16 * 1024); // 16 KB
    }

    #[test]
    fn test_constants_are_reasonable() {
        const _: () = {
            assert!(MAX_RETRIES >= 1);
            assert!(MAX_RETRIES <= 10);
            assert!(BLOCK_REQUEST_TIMEOUT_SECS >= 1);
            assert!(BLOCK_REQUEST_TIMEOUT_SECS <= 30);
            assert!(MAX_BLOCK_READ_MESSAGES >= 100);
        };
    }

    #[test]
    fn test_block_download_result_default() {
        let result = BlockDownloadResult {
            success: false,
            data: None,
            bytes_received: 0,
        };
        assert!(!result.success);
        assert!(result.data.is_none());
        assert_eq!(result.bytes_received, 0);
    }
}

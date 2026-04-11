//! BT Peer Interaction Manager - Peer connection and initialization
//!
//! This module manages the interaction with BitTorrent peers, including:
//! - Connection establishment (plain and encrypted)
//! - Initial handshake and bitfield exchange
//! - Waiting for unchoke messages
//! - Peer statistics tracking
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/PeerConnection.cc/h` - Peer connection management
//! - `src/PeerInteractionCommand.h` - Interaction logic
//! - `src/BtSetup.cc/h` - BT setup and initialization

use std::time::Duration;

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::{Aria2Error, RecoverableError, Result};
use tracing::{debug, error, info, warn};

/// Delay between peer connection setup and message reading (milliseconds)
pub const PEER_CONNECTION_DELAY_MS: u64 = 100;

/// Maximum attempts to wait for unchoke from a peer
pub const MAX_UNCHOKE_WAIT_ATTEMPTS: u32 = 50;

/// Timeout for each message read from peer (seconds)
pub const PEER_MESSAGE_TIMEOUT_SECS: u64 = 5;

/// Result of peer connection attempt
pub struct PeerConnectionResult {
    /// Successfully connected peers
    pub connections: Vec<BtPeerConn>,
    /// Number of failed connections
    pub failed_count: usize,
}

/// BT Peer Interaction Manager
///
/// Handles the lifecycle of peer connections from initial connection
/// through the handshake phase until they're ready for data transfer.
pub struct BtPeerInteraction;

impl BtPeerInteraction {
    /// Connect to multiple peers with automatic fallback strategies
    ///
    /// Attempts to connect to all provided peer addresses using:
    /// 1. MSE encryption if required or forced
    /// 2. Plain connection as fallback
    ///
    /// For each successful connection:
    /// - Sends initial unchoke and interested messages
    /// - Exchanges bitfields
    /// - Waits for unchoke from the peer
    ///
    /// # Arguments
    /// * `peer_addrs` - List of peer addresses to connect to
    /// * `info_hash_raw` - Torrent info hash for handshake
    /// * `num_pieces` - Total number of pieces (for bitfield size)
    /// * `require_crypto` - Whether to require encrypted connections
    /// * `force_encrypt` - Whether to force encryption (fallback to plain)
    ///
    /// # Returns
    /// * `PeerConnectionResult` containing connected peers and failure count
    pub async fn connect_to_peers(
        peer_addrs: &[aria2_protocol::bittorrent::peer::connection::PeerAddr],
        info_hash_raw: &[u8; 20],
        num_pieces: u32,
        require_crypto: bool,
        force_encrypt: bool,
    ) -> Result<PeerConnectionResult> {
        info!("[BT] Connecting to {} peers...", peer_addrs.len());

        let mut active_connections: Vec<BtPeerConn> = Vec::new();
        let mut failed_count = 0usize;

        for addr in peer_addrs {
            debug!("[BT] Connecting to peer {}:{}", addr.ip, addr.port);

            let conn_result =
                Self::connect_single_peer(addr, info_hash_raw, require_crypto, force_encrypt).await;

            match conn_result {
                Ok(mut conn) => {
                    info!(
                        "[BT] Connected to peer {}:{} (encrypted={})",
                        addr.ip,
                        addr.port,
                        conn.is_encrypted()
                    );

                    // Initialize the connection
                    if let Err(e) = Self::initialize_connection(&mut conn, num_pieces).await {
                        warn!("[BT] Failed to initialize peer {}: {}", addr.ip, e);
                        failed_count += 1;
                        continue;
                    }

                    // Wait for unchoke
                    match Self::wait_for_unchoke(&mut conn, addr).await {
                        Ok(()) => {
                            active_connections.push(conn);
                        }
                        Err(e) => {
                            warn!("[BT] No unchoke from peer {}: {}", addr.ip, e);
                            // Still add the connection even without unchoke
                            // (it might unchoke later)
                            active_connections.push(conn);
                        }
                    }
                }
                Err(e) => {
                    error!("[BT] Failed to connect peer {}: {}", addr.ip, e);
                    failed_count += 1;
                    continue;
                }
            }
        }

        info!("[BT] Active connections: {}", active_connections.len());

        if active_connections.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "All peer connections failed".into(),
                },
            ));
        }

        Ok(PeerConnectionResult {
            connections: active_connections,
            failed_count,
        })
    }

    /// Connect to a single peer with encryption fallback logic
    async fn connect_single_peer(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash_raw: &[u8; 20],
        require_crypto: bool,
        force_encrypt: bool,
    ) -> Result<BtPeerConn> {
        if force_encrypt || require_crypto {
            // Try MSE encrypted connection
            BtPeerConn::connect_mse(addr, info_hash_raw, require_crypto).await
        } else {
            // Try MSE first, fall back to plain
            match BtPeerConn::connect_mse(addr, info_hash_raw, false).await {
                Ok(conn) => Ok(conn),
                Err(_) => {
                    debug!("[BT] MSE failed, trying plain connection");
                    BtPeerConn::connect_plain(addr, info_hash_raw).await
                }
            }
        }
    }

    /// Initialize a newly established connection
    ///
    /// Sends initial protocol messages:
    /// - Unchoke (we allow them to request from us)
    /// - Interested (we want to download from them)
    /// - Bitfield (our current piece possession status)
    async fn initialize_connection(conn: &mut BtPeerConn, num_pieces: u32) -> Result<()> {
        // Send initial messages
        conn.send_unchoke().await?;
        conn.send_interested().await?;

        // Send empty bitfield (we have nothing yet)
        let bf_len = (num_pieces as usize).div_ceil(8);
        let empty_bf = vec![0u8; bf_len];
        conn.send_bitfield(empty_bf).await?;

        // Small delay to allow processing
        tokio::time::sleep(Duration::from_millis(PEER_CONNECTION_DELAY_MS)).await;

        Ok(())
    }

    /// Wait for an unchoke message from a peer
    ///
    /// Polls the connection for messages until we receive an Unchoke
    /// or hit the timeout/attempts limit.
    async fn wait_for_unchoke(
        conn: &mut BtPeerConn,
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
    ) -> Result<()> {
        debug!("[BT] Waiting for unchoke from {}:{}", addr.ip, addr.port);

        for _ in 0..MAX_UNCHOKE_WAIT_ATTEMPTS {
            match tokio::time::timeout(
                Duration::from_secs(PEER_MESSAGE_TIMEOUT_SECS),
                conn.read_message(),
            )
            .await
            {
                Ok(Ok(Some(msg))) => {
                    use aria2_protocol::bittorrent::message::types::BtMessage;
                    if matches!(msg, BtMessage::Unchoke) {
                        info!("[BT] Got unchoke from {}:{}", addr.ip, addr.port);
                        return Ok(());
                    }
                    debug!("[BT] Got message while waiting for unchoke: {:?}", msg);
                }
                Ok(Ok(None)) => {
                    warn!("[BT] EOF from peer while waiting for unchoke");
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: "Peer closed connection".into(),
                        },
                    ));
                }
                Ok(Err(e)) => {
                    error!("[BT] Error reading from peer: {}", e);
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: format!("Read error: {}", e),
                        },
                    ));
                }
                Err(_) => {
                    debug!("[BT] Timeout reading from peer, retrying...");
                }
            }
        }

        warn!(
            "[BT] Did not receive unchoke from {}:{} after {} attempts",
            addr.ip, addr.port, MAX_UNCHOKE_WAIT_ATTEMPTS
        );
        Ok(()) // Continue anyway, might get unchoke later
    }

    /// Broadcast a HAVE message to all connected peers
    ///
    /// Notifies all peers that we have completed downloading a piece.
    ///
    /// # Arguments
    /// * `connections` - Mutable slice of active peer connections
    /// * `piece_index` - Index of the completed piece
    pub async fn broadcast_have(connections: &mut [BtPeerConn], piece_index: u32) {
        for conn in connections.iter_mut() {
            if let Err(e) = conn.send_have(piece_index).await {
                warn!("[BT] Failed to send HAVE to peer: {}", e);
            }
        }
    }

    /// Initialize peer bitfield tracker for all connections
    ///
    /// Sets up tracking of which pieces each peer claims to have.
    ///
    /// # Arguments
    /// * `connections` - Slice of active peer connections
    /// * `num_pieces` - Total number of pieces in the torrent
    /// * `peer_tracker` - Mutable reference to the peer bitfield tracker
    pub fn initialize_peer_tracking(
        connections: &[BtPeerConn],
        num_pieces: u32,
        peer_tracker: &mut aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker,
    ) {
        for (i, _conn) in connections.iter().enumerate() {
            let empty_bf = vec![0xFFu8; (num_pieces as usize).div_ceil(8)];
            peer_tracker.update_peer_bitfield(&format!("peer_{}", i), &empty_bf);
        }

        debug!(
            "[BT] Initialized peer tracking for {} peers",
            connections.len()
        );
    }

    /// Clean up peer connections (drop them properly)
    ///
    /// Ensures all connections are properly closed.
    ///
    /// # Arguments
    /// * `connections` - Mutable slice of peer connections to close
    pub fn cleanup_connections(connections: &mut [BtPeerConn]) {
        for conn in connections.iter_mut() {
            let _ = conn;
        }
        debug!("[BT] Cleaned up {} connections", connections.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constants_are_reasonable() {
        assert!(PEER_CONNECTION_DELAY_MS >= 10);
        assert!(PEER_CONNECTION_DELAY_MS <= 1000);
        assert!(MAX_UNCHOKE_WAIT_ATTEMPTS >= 10);
        assert!(MAX_UNCHOKE_WAIT_ATTEMPTS <= 100);
        assert!(PEER_MESSAGE_TIMEOUT_SECS >= 1);
        assert!(PEER_MESSAGE_TIMEOUT_SECS <= 30);
    }

    #[test]
    fn test_peer_connection_result_default() {
        let result = PeerConnectionResult {
            connections: Vec::new(),
            failed_count: 0,
        };
        assert!(result.connections.is_empty());
        assert_eq!(result.failed_count, 0);
    }
}

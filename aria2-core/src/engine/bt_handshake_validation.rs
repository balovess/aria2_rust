//! Handshake peer ID validation — self-connection and duplicate detection.
//!
//! Mirrors the two abort checks in C++ `DefaultBtInteractive::receiveHandshake()`:
//!
//! 1. Self-connection: remote peer ID matches our own static peer ID
//!    → `DL_ABORT_EX("Drop connection from the same Peer ID")`
//!
//! 2. Duplicate peer: remote peer ID already exists in an active connection
//!    → `DL_ABORT_EX("Same Peer ID has been already seen.")`
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/DefaultBtInteractive.cc` — `receiveHandshake(bool quickReply)`
//! - `src/PeerStorage.h` — `getUsedPeers()` returns active peer set
//!
//! This module provides:
//! - [`HandshakeValidationError`] — Error type for the two abort conditions
//! - [`validate_received_peer_id`] — Standalone validation function
//! - [`filter_duplicate_peer_connections`] — Batch filter for Vec<BtPeerConn>
//!
//! The [`HandshakeResult`] enum in `bt_message_receiver` also includes
//! `SelfConnection` and `DuplicatePeerId` variants for use at the lower level
//! (per-connection handshake receiver).
//!
//! # Self-connection check in C++ vs. Rust
//!
//! C++ uses `bittorrent::getStaticPeerId()` which returns a **session-wide**
//! static peer ID generated once. In Rust, each call to `generate_peer_id()`
//! creates a new random ID. For the self-connection check to work correctly,
//! the caller must ensure they pass the same `local_peer_id` that was used in
//! the outbound handshake. The `filter_duplicate_peer_connections()` function
//! accepts this as an explicit parameter.
//!
//! # Duplicate detection in C++ vs. Rust
//!
//! C++ iterates `peerStorage_->getUsedPeers()` scanning all active peers.
//! In Rust, `filter_duplicate_peer_connections()` scans the provided connection
//! list and removes duplicates. For incoming connections (not yet implemented),
//! the validation would happen in the per-peer interaction command when
//! the handshake completes, checking against a shared peer storage.

use crate::engine::bt_peer_connection::BtPeerConn;

// ---------------------------------------------------------------------------
// HandshakeValidationError
// ---------------------------------------------------------------------------

/// Error from handshake peer ID validation.
///
/// Matches the two abort conditions in C++
/// `DefaultBtInteractive::receiveHandshake()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeValidationError {
    /// Remote peer ID matches our own static peer ID — drop the connection.
    ///
    /// C++: `throw DL_ABORT_EX("Drop connection from the same Peer ID")`
    SelfConnection {
        /// The peer ID that matched our own.
        peer_id: [u8; 20],
    },

    /// Same peer ID already exists in an active connection — drop the connection.
    ///
    /// C++: `throw DL_ABORT_EX("Same Peer ID has been already seen.")`
    DuplicatePeerId {
        /// The peer ID that was already present in another connection.
        peer_id: [u8; 20],
    },
}

impl std::fmt::Display for HandshakeValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SelfConnection { peer_id } => write!(
                f,
                "Drop connection from the same Peer ID: {}",
                hex::encode(peer_id)
            ),
            Self::DuplicatePeerId { peer_id } => write!(
                f,
                "Same Peer ID has been already seen: {}",
                hex::encode(peer_id)
            ),
        }
    }
}

impl std::error::Error for HandshakeValidationError {}

// ---------------------------------------------------------------------------
// Validation function
// ---------------------------------------------------------------------------

/// Validate a received handshake peer ID against self-connection and duplicates.
///
/// Mirrors the two checks in C++ `DefaultBtInteractive::receiveHandshake()`:
///
/// 1. **Self-connection**: If `remote_peer_id == local_peer_id`, returns
///    `Err(HandshakeValidationError::SelfConnection)`. In C++ this throws
///    `DL_ABORT_EX("Drop connection from the same Peer ID")`.
///
/// 2. **Duplicate peer**: If `remote_peer_id` matches any entry in
///    `active_peer_ids`, returns `Err(HandshakeValidationError::DuplicatePeerId)`.
///    In C++ this iterates `peerStorage_->getUsedPeers()` checking
///    `peer->isActive() && memcmp(peer->getPeerId(), ..., PEER_ID_LENGTH) == 0`.
///
/// # Arguments
///
/// * `remote_peer_id` — The 20-byte peer ID received in the handshake.
/// * `local_peer_id` — Our own static peer ID for this session.
/// * `active_peer_ids` — Slice of peer IDs from already-established connections.
///
/// # Returns
///
/// `Ok(())` if the peer ID is valid, `Err(HandshakeValidationError)` otherwise.
pub fn validate_received_peer_id(
    remote_peer_id: &[u8; 20],
    local_peer_id: &[u8; 20],
    active_peer_ids: &[[u8; 20]],
) -> std::result::Result<(), HandshakeValidationError> {
    // Check 1: Self-connection detection
    // Mirrors: memcmp(message->getPeerId(), bittorrent::getStaticPeerId(), 20) == 0
    if remote_peer_id == local_peer_id {
        tracing::warn!(
            "Self-connection detected: remote peer ID matches local peer ID ({})",
            hex::encode(remote_peer_id)
        );
        return Err(HandshakeValidationError::SelfConnection {
            peer_id: *remote_peer_id,
        });
    }

    // Check 2: Duplicate peer ID detection
    // Mirrors: for(auto& peer : peerStorage_->getUsedPeers()) { ... }
    for existing_id in active_peer_ids {
        if remote_peer_id == existing_id {
            tracing::warn!(
                "Duplicate peer connection: peer ID {} already active",
                hex::encode(remote_peer_id)
            );
            return Err(HandshakeValidationError::DuplicatePeerId {
                peer_id: *remote_peer_id,
            });
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Batch filter function
// ---------------------------------------------------------------------------

/// Filter out peer connections with self-connection or duplicate peer IDs.
///
/// Takes a list of connected peers and removes (disconnects) any that:
/// - Have a peer ID matching our own (self-connection)
/// - Have a peer ID that appears in more than one connection (duplicate)
///
/// This is a higher-level convenience function that applies
/// [`validate_received_peer_id`] to a batch of connections.
///
/// # Arguments
///
/// * `connections` — Mutable list of peer connections to validate and filter.
/// * `local_peer_id` — Our own static peer ID for this session.
///
/// # Returns
///
/// The number of connections that were removed (dropped due to validation
/// failure). The `connections` vector is modified in place, retaining only
/// valid connections.
pub fn filter_duplicate_peer_connections(
    connections: &mut Vec<BtPeerConn>,
    local_peer_id: &[u8; 20],
) -> usize {
    let mut validated_ids: Vec<[u8; 20]> = Vec::new();
    let mut indices_to_remove: Vec<usize> = Vec::new();

    for (i, conn) in connections.iter().enumerate() {
        let Some(remote_peer_id) = conn.peer_id else {
            // No peer_id set — connection hasn't completed handshake.
            // Keep it; validation will happen when handshake completes.
            continue;
        };

        // Check 1: Self-connection
        if remote_peer_id == *local_peer_id {
            tracing::warn!(
                "[BT] Dropping self-connection from {}:{} (peer ID matches local)",
                conn.ip_addr,
                conn.port
            );
            indices_to_remove.push(i);
            continue;
        }

        // Check 2: Duplicate among already-validated connections in this batch
        if validated_ids.contains(&remote_peer_id) {
            tracing::warn!(
                "[BT] Dropping duplicate peer connection from {}:{} (peer ID {} already seen)",
                conn.ip_addr,
                conn.port,
                hex::encode(remote_peer_id)
            );
            indices_to_remove.push(i);
            continue;
        }

        validated_ids.push(remote_peer_id);
    }

    let removed_count = indices_to_remove.len();

    // Remove connections in reverse order to preserve indices
    for idx in indices_to_remove.into_iter().rev() {
        connections.remove(idx);
    }

    if removed_count > 0 {
        tracing::info!(
            "[BT] Removed {} invalid peer connections (self/duplicate), {} remaining",
            removed_count,
            connections.len()
        );
    }

    removed_count
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // validate_received_peer_id: self-connection detection
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_self_connection_detected() {
        let local_peer_id = [0x11; 20];
        let remote_peer_id = [0x11; 20]; // Same as local

        let result = validate_received_peer_id(&remote_peer_id, &local_peer_id, &[]);
        assert_eq!(
            result,
            Err(HandshakeValidationError::SelfConnection {
                peer_id: [0x11; 20],
            })
        );
    }

    // -----------------------------------------------------------------------
    // validate_received_peer_id: duplicate peer ID detection
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_duplicate_peer_id_detected() {
        let local_peer_id = [0x01; 20];
        let remote_peer_id = [0x22; 20];
        let active_ids = [[0x22; 20]]; // Same as remote

        let result = validate_received_peer_id(&remote_peer_id, &local_peer_id, &active_ids);
        assert_eq!(
            result,
            Err(HandshakeValidationError::DuplicatePeerId {
                peer_id: [0x22; 20],
            })
        );
    }

    // -----------------------------------------------------------------------
    // validate_received_peer_id: valid peer ID passes
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_valid_peer_id() {
        let local_peer_id = [0x01; 20];
        let remote_peer_id = [0x99; 20];
        let active_ids = [[0x22; 20], [0x33; 20]];

        let result = validate_received_peer_id(&remote_peer_id, &local_peer_id, &active_ids);
        assert_eq!(result, Ok(()));
    }

    // -----------------------------------------------------------------------
    // validate_received_peer_id: empty active list — passes if not self
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_empty_active_list() {
        let local_peer_id = [0x01; 20];
        let remote_peer_id = [0x99; 20];

        let result = validate_received_peer_id(&remote_peer_id, &local_peer_id, &[]);
        assert_eq!(result, Ok(()));
    }

    // -----------------------------------------------------------------------
    // validate_received_peer_id: self-connection takes precedence over duplicate
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_self_connection_precedence() {
        let local_peer_id = [0x11; 20];
        let remote_peer_id = [0x11; 20];
        let active_ids = [[0x11; 20]]; // Also in active list

        // Self-connection check comes first, so SelfConnection is returned
        let result = validate_received_peer_id(&remote_peer_id, &local_peer_id, &active_ids);
        assert_eq!(
            result,
            Err(HandshakeValidationError::SelfConnection {
                peer_id: [0x11; 20],
            })
        );
    }

    // -----------------------------------------------------------------------
    // HandshakeValidationError Display formatting
    // -----------------------------------------------------------------------
    #[test]
    fn test_error_display_self_connection() {
        let error = HandshakeValidationError::SelfConnection {
            peer_id: [0xAB; 20],
        };
        let msg = format!("{}", error);
        assert!(msg.contains("Drop connection from the same Peer ID"));
        assert!(msg.contains("abab"));
    }

    #[test]
    fn test_error_display_duplicate() {
        let error = HandshakeValidationError::DuplicatePeerId {
            peer_id: [0xCD; 20],
        };
        let msg = format!("{}", error);
        assert!(msg.contains("Same Peer ID has been already seen"));
        assert!(msg.contains("cdcd"));
    }

    // -----------------------------------------------------------------------
    // filter_duplicate_peer_connections: removes self-connection
    // -----------------------------------------------------------------------
    #[test]
    fn test_filter_removes_self_connection() {
        let local_peer_id = [0x01; 20];
        let mut connections = vec![BtPeerConn::new_stub(&[0x01; 20])];

        // Set the peer_id to match local (self-connection)
        connections[0].peer_id = Some([0x01; 20]);
        connections[0].ip_addr = "192.168.1.1".to_string();
        connections[0].port = 6881;

        let removed = filter_duplicate_peer_connections(&mut connections, &local_peer_id);
        assert_eq!(removed, 1);
        assert!(connections.is_empty());
    }

    // -----------------------------------------------------------------------
    // filter_duplicate_peer_connections: removes duplicate
    // -----------------------------------------------------------------------
    #[test]
    fn test_filter_removes_duplicate() {
        let local_peer_id = [0x01; 20];
        let mut connections = vec![
            BtPeerConn::new_stub(&[0x22; 20]),
            BtPeerConn::new_stub(&[0x22; 20]), // Duplicate peer ID
        ];

        connections[0].peer_id = Some([0x22; 20]);
        connections[0].ip_addr = "192.168.1.1".to_string();
        connections[0].port = 6881;
        connections[1].peer_id = Some([0x22; 20]);
        connections[1].ip_addr = "192.168.1.2".to_string();
        connections[1].port = 6882;

        let removed = filter_duplicate_peer_connections(&mut connections, &local_peer_id);
        assert_eq!(removed, 1);
        assert_eq!(connections.len(), 1);
    }

    // -----------------------------------------------------------------------
    // filter_duplicate_peer_connections: keeps valid connections
    // -----------------------------------------------------------------------
    #[test]
    fn test_filter_keeps_valid_connections() {
        let local_peer_id = [0x01; 20];
        let mut connections = vec![
            BtPeerConn::new_stub(&[0x22; 20]),
            BtPeerConn::new_stub(&[0x33; 20]),
        ];

        connections[0].peer_id = Some([0x22; 20]);
        connections[0].ip_addr = "192.168.1.1".to_string();
        connections[0].port = 6881;
        connections[1].peer_id = Some([0x33; 20]);
        connections[1].ip_addr = "192.168.1.2".to_string();
        connections[1].port = 6882;

        let removed = filter_duplicate_peer_connections(&mut connections, &local_peer_id);
        assert_eq!(removed, 0);
        assert_eq!(connections.len(), 2);
    }

    // -----------------------------------------------------------------------
    // filter_duplicate_peer_connections: connection without peer_id is kept
    // -----------------------------------------------------------------------
    #[test]
    fn test_filter_keeps_connection_without_peer_id() {
        let local_peer_id = [0x01; 20];
        let mut connections = vec![BtPeerConn::new_stub(&[0x99; 20])];

        // Remove the peer_id to simulate a connection that hasn't
        // completed handshake yet.
        connections[0].peer_id = None;

        let removed = filter_duplicate_peer_connections(&mut connections, &local_peer_id);
        assert_eq!(removed, 0);
        assert_eq!(connections.len(), 1);
    }

    // -----------------------------------------------------------------------
    // filter_duplicate_peer_connections: multiple duplicates and self-connection
    // -----------------------------------------------------------------------
    #[test]
    fn test_filter_mixed_invalid_connections() {
        let local_peer_id = [0x01; 20];
        let mut connections = vec![
            BtPeerConn::new_stub(&[0x22; 20]), // Valid
            BtPeerConn::new_stub(&[0x01; 20]), // Self-connection
            BtPeerConn::new_stub(&[0x22; 20]), // Duplicate of first
            BtPeerConn::new_stub(&[0x33; 20]), // Valid
            BtPeerConn::new_stub(&[0x33; 20]), // Duplicate of fourth
        ];

        connections[0].peer_id = Some([0x22; 20]);
        connections[0].ip_addr = "10.0.0.1".to_string();
        connections[0].port = 6881;

        connections[1].peer_id = Some([0x01; 20]); // Self-connection
        connections[1].ip_addr = "10.0.0.2".to_string();
        connections[1].port = 6882;

        connections[2].peer_id = Some([0x22; 20]); // Duplicate
        connections[2].ip_addr = "10.0.0.3".to_string();
        connections[2].port = 6883;

        connections[3].peer_id = Some([0x33; 20]);
        connections[3].ip_addr = "10.0.0.4".to_string();
        connections[3].port = 6884;

        connections[4].peer_id = Some([0x33; 20]); // Duplicate
        connections[4].ip_addr = "10.0.0.5".to_string();
        connections[4].port = 6885;

        let removed = filter_duplicate_peer_connections(&mut connections, &local_peer_id);
        assert_eq!(removed, 3); // Self-connection + 2 duplicates
        assert_eq!(connections.len(), 2); // 2 valid connections remain

        // Verify the remaining connections have distinct, non-local peer IDs
        let remaining_ids: Vec<[u8; 20]> = connections.iter().filter_map(|c| c.peer_id).collect();
        assert_eq!(remaining_ids, [[0x22; 20], [0x33; 20]]);
    }
}

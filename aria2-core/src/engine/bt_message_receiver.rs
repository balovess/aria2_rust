//! BitTorrent message receiver for handling peer handshakes.
//!
//! Mirrors the C++ `DefaultBtMessageReceiver` which handles:
//! - Receiving and validating BitTorrent handshakes
//! - NAT-checking quick reply optimization (sending our handshake as soon
//!   as we confirm the peer's info_hash matches, without waiting for the
//!   full 68-byte handshake)
//!
//! # NAT-checking / Quick Reply
//!
//! When a tracker performs a NAT check, it connects to a peer and sends a
//! BitTorrent handshake. If we haven't sent our handshake yet and we receive
//! at least 48 bytes (enough to read the info_hash at offset 28), we can
//! immediately validate the hash and send our response. This reduces latency
//! and avoids a round-trip.

use aria2_protocol::bittorrent::message::handshake::Handshake;
use aria2_protocol::bittorrent::message::types::HANDSHAKE_LENGTH;

/// Minimum data length needed to check the info_hash in a handshake.
/// The info_hash field starts at byte 28 and is 20 bytes long (28 + 20 = 48).
const QUICK_CHECK_MIN_LENGTH: usize = 48;

// ---------------------------------------------------------------------------
// HandshakeResult
// ---------------------------------------------------------------------------

/// Result of attempting to receive a BitTorrent handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeResult {
    /// Handshake received and validated successfully.
    Completed {
        /// The 20-byte peer ID from the remote peer.
        peer_id: [u8; 20],
        /// The 8-byte reserved bytes from the handshake (extension flags).
        reserved_bytes: [u8; 8],
    },

    /// Not enough data available yet to complete the handshake.
    NeedMoreData,

    /// The received info_hash does not match the expected hash.
    InfoHashMismatch {
        /// The info_hash received from the peer.
        received: [u8; 20],
    },

    /// Handshake data could not be parsed (invalid protocol, bad length, etc.).
    ParseError {
        /// Description of the parse failure.
        reason: String,
    },

    /// Remote peer ID matches our own static peer ID (self-connection).
    ///
    /// Mirrors C++ `DefaultBtInteractive::receiveHandshake()` which checks:
    /// ```cpp
    /// if(memcmp(message->getPeerId(), bittorrent::getStaticPeerId(),
    ///           PEER_ID_LENGTH) == 0) {
    ///   throw DL_ABORT_EX(fmt("Drop connection from the same Peer ID"));
    /// }
    /// ```
    SelfConnection {
        /// The peer ID that matched our own.
        peer_id: [u8; 20],
    },

    /// Same peer ID already exists in an active connection (duplicate).
    ///
    /// Mirrors C++ `DefaultBtInteractive::receiveHandshake()` which scans:
    /// ```cpp
    /// for(auto& peer : peerStorage_->getUsedPeers()) {
    ///   if(peer->isActive() &&
    ///      memcmp(peer->getPeerId(), message->getPeerId(),
    ///             PEER_ID_LENGTH) == 0) {
    ///     throw DL_ABORT_EX(fmt("Same Peer ID has been already seen."));
    ///   }
    /// }
    /// ```
    DuplicatePeerId {
        /// The peer ID that was already present in another connection.
        peer_id: [u8; 20],
    },
}

// ---------------------------------------------------------------------------
// BtMessageReceiver
// ---------------------------------------------------------------------------

/// Receiver for BitTorrent peer handshake messages.
///
/// Handles the handshake receive logic including the NAT-checking quick-reply
/// optimization from the original C++ aria2 implementation. The quick-reply
/// feature allows checking the info_hash from a partial handshake (48 bytes)
/// and marking the handshake as sent immediately, without waiting for the
/// full 68 bytes.
pub struct BtMessageReceiver {
    /// Whether we have already sent (or committed to sending) our handshake.
    handshake_sent: bool,

    /// The expected info_hash for this download session.
    info_hash: [u8; 20],
}

impl BtMessageReceiver {
    /// Create a new receiver for a download with the given expected info_hash.
    pub fn new(info_hash: [u8; 20]) -> Self {
        Self {
            handshake_sent: false,
            info_hash,
        }
    }

    /// Attempt to receive a full BitTorrent handshake from the given data.
    ///
    /// This is the normal (non-quick-reply) path. It requires the full 68 bytes
    /// to be available before parsing. If the handshake is valid and the
    /// info_hash matches, `handshake_sent` is set to `true`.
    ///
    /// Returns:
    /// - `Completed` if the handshake was parsed and the info_hash matches
    /// - `NeedMoreData` if fewer than 68 bytes are available
    /// - `InfoHashMismatch` if the parsed info_hash does not match
    /// - `ParseError` if the data could not be parsed as a valid handshake
    pub fn receive_handshake(&mut self, data: &[u8]) -> HandshakeResult {
        if data.len() < HANDSHAKE_LENGTH {
            tracing::trace!(
                "Handshake data too short: {} bytes, need {}",
                data.len(),
                HANDSHAKE_LENGTH
            );
            return HandshakeResult::NeedMoreData;
        }

        let handshake = match Handshake::parse(data) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("Handshake parse error: {}", e);
                return HandshakeResult::ParseError { reason: e };
            }
        };

        if handshake.info_hash != self.info_hash {
            tracing::warn!(
                "Info hash mismatch: expected {}, got {}",
                hex::encode(self.info_hash),
                hex::encode(handshake.info_hash)
            );
            return HandshakeResult::InfoHashMismatch {
                received: handshake.info_hash,
            };
        }

        // Valid handshake with matching info_hash
        if !self.handshake_sent {
            tracing::debug!("Handshake received and validated, marking as sent");
            self.handshake_sent = true;
        }

        HandshakeResult::Completed {
            peer_id: handshake.peer_id,
            reserved_bytes: handshake.reserved,
        }
    }

    /// Attempt to receive a handshake with NAT-checking quick-reply support.
    ///
    /// If we haven't sent our handshake yet and at least 48 bytes are available,
    /// this method checks the info_hash immediately (without waiting for the
    /// full 68-byte handshake) and sets `handshake_sent = true` if the hash
    /// matches. This mirrors the C++ NAT-checking optimization: the caller
    /// should send their handshake as soon as `handshake_sent` becomes `true`.
    ///
    /// # Behavior
    ///
    /// | Condition | Result |
    /// |-----------|--------|
    /// | `!handshake_sent && data >= 48`, hash mismatch | `InfoHashMismatch` |
    /// | `!handshake_sent && data >= 48`, hash match, `data < 68` | `NeedMoreData` (handshake_sent = true) |
    /// | `!handshake_sent && data >= 68`, hash match | `Completed` (handshake_sent = true) |
    /// | `handshake_sent \|\| data < 48` | Falls through to `receive_handshake` |
    pub fn receive_handshake_with_quick_reply(&mut self, data: &[u8]) -> HandshakeResult {
        // Quick-reply path: if we haven't sent our handshake and have enough
        // data to check the info_hash (>= 48 bytes)
        if !self.handshake_sent && data.len() >= QUICK_CHECK_MIN_LENGTH {
            // Extract info_hash from bytes 28..48 (same offset as the wire format).
            // Note: this reads the info_hash without validating the protocol string
            // first, matching the C++ NAT-checking behavior.
            let received_hash: [u8; 20] =
                data[28..48].try_into().expect("slice is exactly 20 bytes");

            if received_hash != self.info_hash {
                tracing::warn!(
                    "Quick reply: info hash mismatch, expected {}, got {}",
                    hex::encode(self.info_hash),
                    hex::encode(received_hash)
                );
                return HandshakeResult::InfoHashMismatch {
                    received: received_hash,
                };
            }

            // Info hash matches — mark handshake as sent (caller should send ours now)
            tracing::debug!("Quick reply: info hash matches, marking handshake as sent");
            self.handshake_sent = true;

            // If we have the full handshake, also parse and return it
            if data.len() >= HANDSHAKE_LENGTH {
                return self.parse_handshake_data(data);
            }

            // Partial data: we've confirmed the info_hash but need more for peer_id
            return HandshakeResult::NeedMoreData;
        }

        // Normal path: handshake already sent, or not enough data for quick check
        self.receive_handshake(data)
    }

    /// Whether we have already sent (or committed to sending) our handshake.
    pub fn is_handshake_sent(&self) -> bool {
        self.handshake_sent
    }

    /// Set the handshake-sent flag.
    pub fn set_handshake_sent(&mut self, sent: bool) {
        self.handshake_sent = sent;
    }

    /// Internal helper: parse a full handshake from data that is guaranteed
    /// to be at least `HANDSHAKE_LENGTH` bytes. Does NOT modify `handshake_sent`
    /// (the caller is expected to have already set it if needed).
    fn parse_handshake_data(&self, data: &[u8]) -> HandshakeResult {
        debug_assert!(data.len() >= HANDSHAKE_LENGTH);

        let handshake = match Handshake::parse(data) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("Handshake parse error: {}", e);
                return HandshakeResult::ParseError { reason: e };
            }
        };

        if handshake.info_hash != self.info_hash {
            tracing::warn!(
                "Info hash mismatch after quick check: expected {}, got {}",
                hex::encode(self.info_hash),
                hex::encode(handshake.info_hash)
            );
            return HandshakeResult::InfoHashMismatch {
                received: handshake.info_hash,
            };
        }

        HandshakeResult::Completed {
            peer_id: handshake.peer_id,
            reserved_bytes: handshake.reserved,
        }
    }
}


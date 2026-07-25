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

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use aria2_protocol::bittorrent::message::handshake::Handshake;

    /// Helper: create a valid handshake byte buffer with the given info_hash and peer_id.
    fn make_handshake_bytes(info_hash: &[u8; 20], peer_id: &[u8; 20]) -> [u8; HANDSHAKE_LENGTH] {
        Handshake::new(info_hash, peer_id).to_bytes()
    }

    // -----------------------------------------------------------------------
    // Test 1: New receiver initial state
    // -----------------------------------------------------------------------
    #[test]
    fn test_new_receiver_initial_state() {
        let info_hash = [0xAA; 20];
        let receiver = BtMessageReceiver::new(info_hash);
        assert!(!receiver.is_handshake_sent());
    }

    // -----------------------------------------------------------------------
    // Test 2: Full handshake success
    // -----------------------------------------------------------------------
    #[test]
    fn test_full_handshake_success() {
        let info_hash = [0x11; 20];
        let peer_id = [0x22; 20];
        let mut receiver = BtMessageReceiver::new(info_hash);
        let data = make_handshake_bytes(&info_hash, &peer_id);

        let result = receiver.receive_handshake(&data);
        match result {
            HandshakeResult::Completed {
                peer_id: pid,
                reserved_bytes,
            } => {
                assert_eq!(pid, peer_id);
                // Default Handshake has DHT bit set in reserved[5]
                assert_ne!(reserved_bytes, [0u8; 8]);
            }
            _ => panic!("Expected Completed, got {:?}", result),
        }
        assert!(receiver.is_handshake_sent());
    }

    // -----------------------------------------------------------------------
    // Test 3: Info hash mismatch
    // -----------------------------------------------------------------------
    #[test]
    fn test_info_hash_mismatch() {
        let expected_hash = [0x11; 20];
        let wrong_hash = [0xFF; 20];
        let peer_id = [0x22; 20];
        let mut receiver = BtMessageReceiver::new(expected_hash);

        let data = make_handshake_bytes(&wrong_hash, &peer_id);
        let result = receiver.receive_handshake(&data);

        match result {
            HandshakeResult::InfoHashMismatch { received } => {
                assert_eq!(received, wrong_hash);
            }
            _ => panic!("Expected InfoHashMismatch, got {:?}", result),
        }
        // handshake_sent should NOT be set on mismatch
        assert!(!receiver.is_handshake_sent());
    }

    // -----------------------------------------------------------------------
    // Test 4: Incomplete data (too short)
    // -----------------------------------------------------------------------
    #[test]
    fn test_incomplete_data_too_short() {
        let info_hash = [0x11; 20];
        let mut receiver = BtMessageReceiver::new(info_hash);

        // Empty data
        let result = receiver.receive_handshake(&[]);
        assert_eq!(result, HandshakeResult::NeedMoreData);

        // Partial data (67 bytes — one short)
        let partial = vec![0u8; 67];
        let result = receiver.receive_handshake(&partial);
        assert_eq!(result, HandshakeResult::NeedMoreData);
        assert!(!receiver.is_handshake_sent());
    }

    // -----------------------------------------------------------------------
    // Test 5: Quick reply with valid info_hash (48 bytes, partial)
    // -----------------------------------------------------------------------
    #[test]
    fn test_quick_reply_valid_info_hash_partial() {
        let info_hash = [0x33; 20];
        let peer_id = [0x44; 20];
        let mut receiver = BtMessageReceiver::new(info_hash);

        let full_data = make_handshake_bytes(&info_hash, &peer_id);
        // Only the first 48 bytes (enough for info_hash but not peer_id)
        let partial = &full_data[..48];

        let result = receiver.receive_handshake_with_quick_reply(partial);
        assert_eq!(result, HandshakeResult::NeedMoreData);
        // handshake_sent should be true after quick-reply confirms info_hash
        assert!(receiver.is_handshake_sent());
    }

    // -----------------------------------------------------------------------
    // Test 6: Quick reply with invalid info_hash
    // -----------------------------------------------------------------------
    #[test]
    fn test_quick_reply_invalid_info_hash() {
        let expected_hash = [0x33; 20];
        let wrong_hash = [0xFF; 20];
        let peer_id = [0x44; 20];
        let mut receiver = BtMessageReceiver::new(expected_hash);

        let full_data = make_handshake_bytes(&wrong_hash, &peer_id);
        let partial = &full_data[..48];

        let result = receiver.receive_handshake_with_quick_reply(partial);
        match result {
            HandshakeResult::InfoHashMismatch { received } => {
                assert_eq!(received, wrong_hash);
            }
            _ => panic!("Expected InfoHashMismatch, got {:?}", result),
        }
        // handshake_sent should NOT be set on mismatch
        assert!(!receiver.is_handshake_sent());
    }

    // -----------------------------------------------------------------------
    // Test 7: handshake_sent flag transitions
    // -----------------------------------------------------------------------
    #[test]
    fn test_handshake_sent_flag_transitions() {
        let info_hash = [0x11; 20];
        let mut receiver = BtMessageReceiver::new(info_hash);

        // Initially false
        assert!(!receiver.is_handshake_sent());

        // Explicitly set to true
        receiver.set_handshake_sent(true);
        assert!(receiver.is_handshake_sent());

        // Explicitly set back to false
        receiver.set_handshake_sent(false);
        assert!(!receiver.is_handshake_sent());
    }

    // -----------------------------------------------------------------------
    // Test 8: Roundtrip with Handshake::parse()
    // -----------------------------------------------------------------------
    #[test]
    fn test_roundtrip_with_handshake_parse() {
        let info_hash = [0xAB; 20];
        let peer_id = [0xCD; 20];

        // Create and serialize a handshake
        let handshake = Handshake::new(&info_hash, &peer_id);
        let bytes = handshake.to_bytes();

        // Verify the handshake can also be parsed independently
        let parsed = Handshake::parse(&bytes).unwrap();
        assert_eq!(parsed.info_hash, info_hash);
        assert_eq!(parsed.peer_id, peer_id);

        // Receive it through the receiver
        let mut receiver = BtMessageReceiver::new(info_hash);
        let result = receiver.receive_handshake(&bytes);

        match result {
            HandshakeResult::Completed {
                peer_id: pid,
                reserved_bytes,
            } => {
                assert_eq!(pid, peer_id);
                assert_eq!(reserved_bytes, handshake.reserved);
            }
            _ => panic!("Expected Completed, got {:?}", result),
        }
    }

    // -----------------------------------------------------------------------
    // Test 9: Quick reply with full 68 bytes
    // -----------------------------------------------------------------------
    #[test]
    fn test_quick_reply_full_68_bytes() {
        let info_hash = [0x55; 20];
        let peer_id = [0x66; 20];
        let mut receiver = BtMessageReceiver::new(info_hash);

        let data = make_handshake_bytes(&info_hash, &peer_id);
        let result = receiver.receive_handshake_with_quick_reply(&data);

        match result {
            HandshakeResult::Completed { peer_id: pid, .. } => {
                assert_eq!(pid, peer_id);
            }
            _ => panic!("Expected Completed, got {:?}", result),
        }
        assert!(receiver.is_handshake_sent());
    }

    // -----------------------------------------------------------------------
    // Test 10: Quick reply not triggered when handshake already sent
    // -----------------------------------------------------------------------
    #[test]
    fn test_quick_reply_not_triggered_when_already_sent() {
        let info_hash = [0x55; 20];
        let peer_id = [0x66; 20];
        let mut receiver = BtMessageReceiver::new(info_hash);
        receiver.set_handshake_sent(true);

        let data = make_handshake_bytes(&info_hash, &peer_id);
        // With handshake_sent = true, quick-reply path is skipped
        // but normal path still works
        let result = receiver.receive_handshake_with_quick_reply(&data);

        match result {
            HandshakeResult::Completed { peer_id: pid, .. } => {
                assert_eq!(pid, peer_id);
            }
            _ => panic!("Expected Completed, got {:?}", result),
        }
    }

    // -----------------------------------------------------------------------
    // Test 11: Quick reply not triggered with less than 48 bytes
    // -----------------------------------------------------------------------
    #[test]
    fn test_quick_reply_not_triggered_with_less_than_48_bytes() {
        let info_hash = [0x55; 20];
        let mut receiver = BtMessageReceiver::new(info_hash);

        // 47 bytes — not enough for quick check, falls through to normal path
        let data = vec![0u8; 47];
        let result = receiver.receive_handshake_with_quick_reply(&data);
        assert_eq!(result, HandshakeResult::NeedMoreData);
        assert!(!receiver.is_handshake_sent());
    }

    // -----------------------------------------------------------------------
    // Test 12: Parse error with bad protocol
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_error_bad_protocol() {
        let info_hash = [0x11; 20];
        let mut receiver = BtMessageReceiver::new(info_hash);

        // Create 68 bytes with an invalid protocol string
        let mut bad_data = [0u8; HANDSHAKE_LENGTH];
        bad_data[0] = 19; // pstrlen = 19
        bad_data[1..20].copy_from_slice(b"Invalid protocol!!!"); // wrong protocol

        let result = receiver.receive_handshake(&bad_data);
        match result {
            HandshakeResult::ParseError { reason } => {
                assert!(!reason.is_empty());
            }
            _ => panic!("Expected ParseError, got {:?}", result),
        }
        // handshake_sent should NOT be set on parse error
        assert!(!receiver.is_handshake_sent());
    }

    // -----------------------------------------------------------------------
    // Test 13: Quick reply then full data (two-step handshake)
    // -----------------------------------------------------------------------
    #[test]
    fn test_quick_reply_then_full_data() {
        let info_hash = [0x77; 20];
        let peer_id = [0x88; 20];
        let mut receiver = BtMessageReceiver::new(info_hash);

        let full_data = make_handshake_bytes(&info_hash, &peer_id);

        // Step 1: Quick reply with 48 bytes — confirms info_hash
        let partial = &full_data[..48];
        let result1 = receiver.receive_handshake_with_quick_reply(partial);
        assert_eq!(result1, HandshakeResult::NeedMoreData);
        assert!(receiver.is_handshake_sent());

        // Step 2: Now provide full data (handshake_sent is true, so normal path)
        let result2 = receiver.receive_handshake_with_quick_reply(&full_data);
        match result2 {
            HandshakeResult::Completed { peer_id: pid, .. } => {
                assert_eq!(pid, peer_id);
            }
            _ => panic!("Expected Completed, got {:?}", result2),
        }
    }

    // -----------------------------------------------------------------------
    // Test 14: Reserved bytes preserved in Completed result
    // -----------------------------------------------------------------------
    #[test]
    fn test_reserved_bytes_preserved() {
        let info_hash = [0x99; 20];
        let peer_id = [0xAA; 20];

        // Create handshake with DHT extension enabled
        let handshake = Handshake::new(&info_hash, &peer_id).with_dht(true);
        let bytes = handshake.to_bytes();

        let mut receiver = BtMessageReceiver::new(info_hash);
        let result = receiver.receive_handshake(&bytes);

        match result {
            HandshakeResult::Completed { reserved_bytes, .. } => {
                // Fast Extension bit should be set in reserved[7] (bit 2)
                assert_ne!(reserved_bytes[7] & 0x04, 0);
                // Extended Messaging bit should be set in reserved[5] (bit 4)
                assert_ne!(reserved_bytes[5] & 0x10, 0);
                // DHT bit should be set in reserved[7] (bit 0)
                assert_ne!(reserved_bytes[7] & 0x01, 0);
            }
            _ => panic!("Expected Completed, got {:?}", result),
        }
    }

    // -----------------------------------------------------------------------
    // Test 15: handshake_sent set after successful receive_handshake
    // -----------------------------------------------------------------------
    #[test]
    fn test_handshake_sent_set_after_successful_receive() {
        let info_hash = [0xBB; 20];
        let peer_id = [0xCC; 20];
        let mut receiver = BtMessageReceiver::new(info_hash);

        assert!(!receiver.is_handshake_sent());

        let data = make_handshake_bytes(&info_hash, &peer_id);
        let _ = receiver.receive_handshake(&data);

        assert!(receiver.is_handshake_sent());

        // Calling again should not change the flag (idempotent)
        let _ = receiver.receive_handshake(&data);
        assert!(receiver.is_handshake_sent());
    }

    // -----------------------------------------------------------------------
    // Test 16: Quick reply with data between 48 and 67 bytes
    // -----------------------------------------------------------------------
    #[test]
    fn test_quick_reply_data_between_48_and_67() {
        let info_hash = [0xDD; 20];
        let peer_id = [0xEE; 20];
        let full_data = make_handshake_bytes(&info_hash, &peer_id);

        // Test with exactly 48 bytes
        let mut receiver = BtMessageReceiver::new(info_hash);
        let result48 = receiver.receive_handshake_with_quick_reply(&full_data[..48]);
        assert_eq!(result48, HandshakeResult::NeedMoreData);
        assert!(receiver.is_handshake_sent());

        // Test with 60 bytes
        let mut receiver = BtMessageReceiver::new(info_hash);
        let result60 = receiver.receive_handshake_with_quick_reply(&full_data[..60]);
        assert_eq!(result60, HandshakeResult::NeedMoreData);
        assert!(receiver.is_handshake_sent());

        // Test with 67 bytes (one short)
        let mut receiver = BtMessageReceiver::new(info_hash);
        let result67 = receiver.receive_handshake_with_quick_reply(&full_data[..67]);
        assert_eq!(result67, HandshakeResult::NeedMoreData);
        assert!(receiver.is_handshake_sent());
    }
}

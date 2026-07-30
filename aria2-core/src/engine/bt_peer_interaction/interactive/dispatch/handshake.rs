//! Handshake reception and same-peer-ID duplicate detection for
//! `BtPeerInteractive`.
//!
//! Contains [alidate_handshake_peer_id()] which checks a received
//! handshake against our own peer ID and all currently connected peers.

use aria2_protocol::bittorrent::message::handshake::Handshake;

use crate::error::{Aria2Error, RecoverableError, Result};
use tracing::{info, warn};

use super::super::types::*;
use super::BtPeerInteractive;

impl BtPeerInteractive {
    // -- Handshake reception --------------------------------------------------

    /// Validate a received handshake message by checking the remote peer ID
    /// against our own static peer ID and against all currently connected
    /// peer IDs.
    ///
    /// Mirrors C++ `DefaultBtInteractive::receiveHandshake()`:
    /// `cpp
    /// if (memcmp(message->getPeerId(), bittorrent::getStaticPeerId(),
    ///            PEER_ID_LENGTH) == 0) {
    ///   throw DL_ABORT_EX("Drop connection from the same Peer ID");
    /// }
    /// for (auto& peer : peerStorage_->getUsedPeers()) {
    ///   if (peer->isActive() &&
    ///       memcmp(peer->getPeerId(), message->getPeerId(), PEER_ID_LENGTH) == 0) {
    ///     throw DL_ABORT_EX("Same Peer ID has been already seen.");
    ///   }
    /// }
    /// `
    ///
    /// # Arguments
    ///
    /// * `handshake` - The received handshake message containing the remote
    ///   peer's 20-byte peer ID.
    /// * `our_peer_id` - Our own static peer ID, generated once at startup
    ///   (equivalent to C++ `bittorrent::getStaticPeerId()`).
    /// * `connected_peer_ids` - Iterator yielding the 20-byte peer IDs of
    ///   all currently active peer connections (equivalent to C++
    ///   `peerStorage_->getUsedPeers()` filtered by `isActive()`).
    ///
    /// # Returns
    ///
    /// * `Ok(())` - Peer ID passed both checks; proceed with the handshake.
    /// * `Err(Aria2Error::Recoverable(HandshakeRejection))` - Self-connection
    ///   or duplicate detected; abort this connection.
    pub fn validate_handshake_peer_id<'a>(
        handshake: &Handshake,
        our_peer_id: &[u8; 20],
        connected_peer_ids: impl IntoIterator<Item = &'a [u8; 20]>,
    ) -> Result<()> {
        let result = Self::check_duplicate_peer_id(
            &handshake.peer_id,
            our_peer_id,
            connected_peer_ids,
        );

        match result {
            PeerIdCheckResult::SelfConnection => {
                warn!(
                    "Drop connection from the same Peer ID: {}",
                    handshake.peer_id_str()
                );
                Err(Aria2Error::Recoverable(
                    RecoverableError::HandshakeRejection {
                        reason: "Self-connection: remote peer ID matches our own".into(),
                    },
                ))
            }
            PeerIdCheckResult::DuplicatePeer => {
                info!(
                    "Same Peer ID has been already seen: {}",
                    handshake.peer_id_str()
                );
                Err(Aria2Error::Recoverable(
                    RecoverableError::HandshakeRejection {
                        reason: "Duplicate: peer ID already connected on another peer".into(),
                    },
                ))
            }
            PeerIdCheckResult::Ok => Ok(()),
        }
    }
}

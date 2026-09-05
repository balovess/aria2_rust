//! Extension lookup/resolve logic and message dispatch.

use aria2_protocol::bittorrent::message::extension::{
    ExtensionHandshake, UtMetadataMessage, UtPexMessage,
};
use tracing::{trace, warn};

use super::{ExtensionRegistry, ExtensionUpdate, UT_METADATA_NAME, UT_PEX_NAME};

// ---------------------------------------------------------------------------
// Lookup methods on ExtensionRegistry
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Get the peer's ext_id for ut_metadata, if the peer supports it.
    pub fn peer_ut_metadata_id(&self) -> Option<u8> {
        self.peer_id_to_name
            .iter()
            .find(|(_, name)| name.as_ref() == UT_METADATA_NAME)
            .map(|(&id, _)| id)
    }

    /// Get the peer's ext_id for ut_pex, if the peer supports it.
    pub fn peer_ut_pex_id(&self) -> Option<u8> {
        self.peer_id_to_name
            .iter()
            .find(|(_, name)| name.as_ref() == UT_PEX_NAME)
            .map(|(&id, _)| id)
    }

    /// Get our local ext_id for ut_metadata (always present).
    pub fn local_ut_metadata_id(&self) -> u8 {
        self.local_ut_metadata_id
    }

    /// Get our local ext_id for ut_pex (always present).
    pub fn local_ut_pex_id(&self) -> u8 {
        self.local_ut_pex_id
    }

    /// Check if the peer supports an extension with the given ext_id.
    ///
    /// Returns `true` if the given ext_id appears in the peer's assignments.
    pub fn is_extension_enabled(&self, ext_id: u8) -> bool {
        self.peer_id_to_name.contains_key(&ext_id)
    }

    /// Reverse lookup: given a peer's ext_id, return the extension name.
    ///
    /// Returns `None` if the ext_id is not recognized.
    pub fn extension_name_for_id(&self, ext_id: u8) -> Option<&[u8]> {
        self.peer_id_to_name.get(&ext_id).map(Box::as_ref)
    }

    /// Get the reqq value from the peer's handshake.
    ///
    /// This is the maximum number of outstanding metadata requests
    /// the peer will accept.
    pub fn reqq(&self) -> u32 {
        self.reqq
    }

    /// Check if the peer supports ut_metadata.
    pub fn supports_ut_metadata(&self) -> bool {
        self.peer_ut_metadata_id().is_some()
    }

    /// Check if the peer supports ut_pex.
    pub fn supports_ut_pex(&self) -> bool {
        self.peer_ut_pex_id().is_some()
    }
}

// ---------------------------------------------------------------------------
// Dispatch function
// ---------------------------------------------------------------------------

/// Dispatch a received Extended message and return the resulting update.
///
/// This is the central extension dispatch function. It:
///
/// 1. If `ext_id == 0`: Parses the payload as an `ExtensionHandshake`,
///    updates the registry, and returns `ExtensionUpdate::HandshakeReceived`.
///
/// 2. If `ext_id` matches the peer's ut_metadata ID: Parses the payload
///    as a `UtMetadataMessage` and returns the appropriate variant.
///
/// 3. If `ext_id` matches the peer's ut_pex ID: Parses the payload
///    as a `UtPexMessage` and returns `ExtensionUpdate::PeerExchange`.
///
/// 4. Otherwise: Returns `None` (unknown extension; caller should log a warning).
///
/// # Arguments
///
/// * `registry` — The per-peer extension registry (mutated on handshake).
/// * `ext_id` — The extended message ID from the wire.
/// * `payload` — The bencoded payload bytes after the ext_id byte.
///
/// # Returns
///
/// `Some(ExtensionUpdate)` if the message was recognized and processed,
/// `None` if the ext_id is unknown.
pub fn dispatch_extension_message(
    registry: &mut ExtensionRegistry,
    ext_id: u8,
    payload: &[u8],
) -> Option<ExtensionUpdate> {
    if ext_id == 0 {
        // Extension handshake (BEP 10)
        match ExtensionHandshake::from_bytes(payload) {
            Ok(handshake) => {
                let ut_metadata_id = handshake.ut_metadata_id();
                let ut_pex_id = handshake.ut_pex_id();
                let reqq = handshake.reqq();

                registry.update_from_peer_handshake(&handshake);

                Some(ExtensionUpdate::HandshakeReceived {
                    ut_metadata_id,
                    ut_pex_id,
                    reqq,
                })
            }
            Err(e) => {
                warn!("Failed to parse extension handshake: {}", e);
                None
            }
        }
    } else if let Some(peer_id) = registry.peer_ut_metadata_id() {
        if ext_id == peer_id {
            // ut_metadata message (BEP 9)
            match UtMetadataMessage::from_payload(payload) {
                Ok(msg) => match msg {
                    UtMetadataMessage::Request { piece } => {
                        trace!("ut_metadata Request for piece {}", piece);
                        Some(ExtensionUpdate::MetadataRequest { piece })
                    }
                    UtMetadataMessage::Data {
                        piece,
                        total_size,
                        data,
                    } => {
                        trace!(
                            "ut_metadata Data piece {} (total_size={}, data_len={})",
                            piece,
                            total_size,
                            data.len()
                        );
                        Some(ExtensionUpdate::MetadataPiece {
                            piece,
                            total_size,
                            data,
                        })
                    }
                    UtMetadataMessage::Reject { piece } => {
                        trace!("ut_metadata Reject for piece {}", piece);
                        Some(ExtensionUpdate::MetadataReject { piece })
                    }
                },
                Err(e) => {
                    warn!("Failed to parse ut_metadata payload: {}", e);
                    None
                }
            }
        } else {
            try_dispatch_pex(registry, ext_id, payload)
        }
    } else {
        try_dispatch_pex(registry, ext_id, payload)
    }
}

/// Try to dispatch as a ut_pex message, or return None if unrecognized.
fn try_dispatch_pex(
    registry: &ExtensionRegistry,
    ext_id: u8,
    payload: &[u8],
) -> Option<ExtensionUpdate> {
    let peer_pex_id = registry.peer_ut_pex_id()?;

    if ext_id != peer_pex_id {
        // Unknown extension
        if let Some(name) = registry.extension_name_for_id(ext_id) {
            warn!(
                "Received unhandled extension message: {:?} (ext_id={})",
                String::from_utf8_lossy(name),
                ext_id
            );
        } else {
            warn!("Received unknown extension message with ext_id={}", ext_id);
        }
        return None;
    }

    // ut_pex message (BEP 11)
    match UtPexMessage::from_payload(payload) {
        Ok(msg) => {
            trace!(
                "ut_pex: {} IPv4 added, {} IPv6 added, {} IPv4 dropped, {} IPv6 dropped",
                msg.added.len(),
                msg.added6.len(),
                msg.dropped.len(),
                msg.dropped6.len()
            );
            Some(ExtensionUpdate::PeerExchange {
                added_v4: msg.added,
                added_v6: msg.added6,
                dropped_v4: msg.dropped,
                dropped_v6: msg.dropped6,
            })
        }
        Err(e) => {
            warn!("Failed to parse ut_pex payload: {}", e);
            None
        }
    }
}

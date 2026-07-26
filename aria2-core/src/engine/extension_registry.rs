//! BEP 10 Extension Protocol message registry and dispatch types.
//!
//! Implements the `ExtensionMessageRegistry` + `DefaultExtensionMessageFactory`
//! pattern from the C++ aria2 codebase. The registry tracks per-peer extension
//! ID assignments negotiated via the BEP 10 extension handshake, and provides
//! dispatch logic for ut_metadata (BEP 9) and ut_pex (BEP 11).
//!
//! # Wire Protocol
//!
//! After the BT handshake, both peers send an Extended message with `ext_id = 0`
//! (the extension handshake). The payload is a bencoded dict:
//!
//! ```text
//! d 1:m d 10:ut_metadata i1e 6:ut_pex i2e e 4:reqq i500e e
//! ```
//!
//! The `m` sub-dict maps extension names to the `ext_id` values the sender
//! will use for those extensions in subsequent Extended messages.

use std::collections::HashMap;

use aria2_protocol::bittorrent::message::extension::{
    CompactPeerV4, CompactPeerV6, ExtensionHandshake, UtMetadataMessage, UtPexMessage,
};
use tracing::{debug, trace, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default local ext_id for ut_metadata (matches C++ DefaultExtensionMessageFactory).
const DEFAULT_LOCAL_UT_METADATA_ID: u8 = 1;

/// Default local ext_id for ut_pex (matches C++ DefaultExtensionMessageFactory).
const DEFAULT_LOCAL_UT_PEX_ID: u8 = 2;

/// Default reqq value (max outstanding metadata requests).
const DEFAULT_REQQ: u32 = 500;

/// Extension name for ut_metadata (BEP 9).
const UT_METADATA_NAME: &[u8] = b"ut_metadata";

/// Extension name for ut_pex (BEP 11).
const UT_PEX_NAME: &[u8] = b"ut_pex";

// ---------------------------------------------------------------------------
// ExtensionRegistry
// ---------------------------------------------------------------------------

/// Per-peer registry mapping extension names to their negotiated ext_id values.
///
/// Mirrors C++ `ExtensionMessageRegistry` which stores the mapping between
/// extension names (ut_metadata, ut_pex) and their ext_id values as negotiated
/// in the BEP 10 extension handshake.
///
/// After receiving an Extension Handshake from a peer, the caller populates
/// this registry with the peer's ext_id assignments. Then when an Extended
/// message arrives with a given ext_id, the dispatch logic looks up the
/// registry to determine which extension handler to invoke.
///
/// # Local vs Peer IDs
///
/// - **Local IDs**: The ext_id values *we* told the peer in our handshake.
///   These are fixed for the connection lifetime (ut_metadata=1, ut_pex=2).
/// - **Peer IDs**: The ext_id values the *peer* told us in their handshake.
///   When we receive an Extended message with `ext_id = X`, we match X
///   against the peer's IDs to determine which extension it is.
#[derive(Debug, Clone)]
pub struct ExtensionRegistry {
    /// Our local ext_id assignments (what we told the peer in our handshake).
    /// Key = extension name bytes (e.g. b"ut_metadata"), Value = our ext_id.
    local_extensions: HashMap<Vec<u8>, u8>,

    /// Peer's ext_id assignments (what the peer told us in their handshake).
    /// Key = extension name bytes (e.g. b"ut_metadata"), Value = peer's ext_id.
    peer_extensions: HashMap<Vec<u8>, u8>,

    /// Reverse map: peer's ext_id -> extension name (for dispatch).
    peer_id_to_name: HashMap<u8, Vec<u8>>,

    /// The reqq value from the peer's handshake (max outstanding metadata reqs).
    reqq: u32,
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionRegistry {
    /// Create a new registry with default local ext_id assignments.
    ///
    /// Local assignments: ut_metadata = 1, ut_pex = 2.
    /// Peer assignments are empty until a handshake is received.
    pub fn new() -> Self {
        let mut local_extensions = HashMap::new();
        local_extensions.insert(UT_METADATA_NAME.to_vec(), DEFAULT_LOCAL_UT_METADATA_ID);
        local_extensions.insert(UT_PEX_NAME.to_vec(), DEFAULT_LOCAL_UT_PEX_ID);

        Self {
            local_extensions,
            peer_extensions: HashMap::new(),
            peer_id_to_name: HashMap::new(),
            reqq: DEFAULT_REQQ,
        }
    }

    /// Update peer extension assignments from a received extension handshake.
    ///
    /// Parses the `m` dict from the handshake and populates `peer_extensions`
    /// and the reverse map `peer_id_to_name`. Also stores the `reqq` value.
    ///
    /// This should be called exactly once, when the extension handshake
    /// (ext_id = 0) is received from the peer.
    pub fn update_from_peer_handshake(&mut self, handshake: &ExtensionHandshake) {
        // Clear previous peer mappings (shouldn't happen, but be safe)
        self.peer_extensions.clear();
        self.peer_id_to_name.clear();

        // Walk the m_dict and register each extension
        for (name, value) in handshake.m_dict() {
            if let Some(ext_id) = value.as_int().and_then(|i| u8::try_from(i).ok()) {
                if ext_id == 0 {
                    // ext_id 0 is reserved for the handshake itself
                    warn!(
                        "Peer assigned ext_id=0 to extension {:?}, ignoring",
                        String::from_utf8_lossy(name)
                    );
                    continue;
                }
                trace!(
                    "Peer extension: {:?} -> ext_id={}",
                    String::from_utf8_lossy(name),
                    ext_id
                );
                self.peer_extensions.insert(name.clone(), ext_id);
                self.peer_id_to_name.insert(ext_id, name.clone());
            }
        }

        self.reqq = handshake.reqq();

        debug!(
            "Updated extension registry from peer handshake: ut_metadata={:?}, ut_pex={:?}, reqq={}",
            self.peer_ut_metadata_id(),
            self.peer_ut_pex_id(),
            self.reqq
        );
    }

    /// Get the peer's ext_id for ut_metadata, if the peer supports it.
    pub fn peer_ut_metadata_id(&self) -> Option<u8> {
        self.peer_extensions.get(UT_METADATA_NAME).copied()
    }

    /// Get the peer's ext_id for ut_pex, if the peer supports it.
    pub fn peer_ut_pex_id(&self) -> Option<u8> {
        self.peer_extensions.get(UT_PEX_NAME).copied()
    }

    /// Get our local ext_id for ut_metadata (always present).
    pub fn local_ut_metadata_id(&self) -> u8 {
        self.local_extensions
            .get(UT_METADATA_NAME)
            .copied()
            .unwrap_or(DEFAULT_LOCAL_UT_METADATA_ID)
    }

    /// Get our local ext_id for ut_pex (always present).
    pub fn local_ut_pex_id(&self) -> u8 {
        self.local_extensions
            .get(UT_PEX_NAME)
            .copied()
            .unwrap_or(DEFAULT_LOCAL_UT_PEX_ID)
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
        self.peer_id_to_name.get(&ext_id).map(|v| v.as_slice())
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

    /// Build our local extension handshake to send to the peer.
    ///
    /// Creates an `ExtensionHandshake` with our local ext_id assignments
    /// and the default reqq value.
    pub fn build_local_handshake(&self) -> ExtensionHandshake {
        let mut hs = ExtensionHandshake::new();
        hs.with_ut_metadata(self.local_ut_metadata_id());
        hs.with_ut_pex(self.local_ut_pex_id());
        hs.with_reqq(self.reqq);
        hs
    }
}

// ---------------------------------------------------------------------------
// ExtensionUpdate
// ---------------------------------------------------------------------------

/// Side effect from processing an extension message.
///
/// Returned by [`dispatch_extension_message`] so the interaction loop
/// caller can apply the appropriate side effects (e.g., storing metadata,
/// adding discovered peers, etc.).
#[derive(Debug, Clone)]
pub enum ExtensionUpdate {
    /// Received extension handshake; registry updated.
    HandshakeReceived {
        /// Peer's ext_id for ut_metadata (None if not supported).
        ut_metadata_id: Option<u8>,
        /// Peer's ext_id for ut_pex (None if not supported).
        ut_pex_id: Option<u8>,
        /// Peer's reqq value (max outstanding metadata requests).
        reqq: u32,
    },

    /// Received ut_metadata Data piece.
    MetadataPiece {
        /// Piece index within the metadata.
        piece: u32,
        /// Total metadata size in bytes.
        total_size: u32,
        /// Raw metadata bytes for this piece.
        data: Vec<u8>,
    },

    /// Received ut_metadata Request.
    MetadataRequest {
        /// Piece index being requested.
        piece: u32,
    },

    /// Received ut_metadata Reject.
    MetadataReject {
        /// Piece index that was rejected.
        piece: u32,
    },

    /// Received ut_pex message with new and dropped peers (BEP 11).
    PeerExchange {
        /// Newly discovered IPv4 peers in compact format (6 bytes each).
        added_v4: Vec<CompactPeerV4>,
        /// Newly discovered IPv6 peers in compact format (18 bytes each).
        added_v6: Vec<CompactPeerV6>,
        /// Disconnected IPv4 peers in compact format (6 bytes each).
        dropped_v4: Vec<CompactPeerV4>,
        /// Disconnected IPv6 peers in compact format (18 bytes each).
        dropped_v6: Vec<CompactPeerV6>,
    },
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use std::collections::BTreeMap;

    // ====================== ExtensionRegistry tests ======================

    #[test]
    fn test_registry_new_default_local_ids() {
        let reg = ExtensionRegistry::new();
        assert_eq!(reg.local_ut_metadata_id(), 1);
        assert_eq!(reg.local_ut_pex_id(), 2);
    }

    #[test]
    fn test_registry_new_peer_ids_initially_none() {
        let reg = ExtensionRegistry::new();
        assert!(reg.peer_ut_metadata_id().is_none());
        assert!(reg.peer_ut_pex_id().is_none());
    }

    #[test]
    fn test_registry_default_trait() {
        let reg = ExtensionRegistry::default();
        assert_eq!(reg.local_ut_metadata_id(), 1);
        assert_eq!(reg.local_ut_pex_id(), 2);
    }

    #[test]
    fn test_registry_default_reqq() {
        let reg = ExtensionRegistry::new();
        assert_eq!(reg.reqq(), DEFAULT_REQQ);
    }

    #[test]
    fn test_update_from_peer_handshake_full() {
        let mut reg = ExtensionRegistry::new();
        let hs = ExtensionHandshake::new(); // ut_metadata=1, ut_pex=2, reqq=500
        reg.update_from_peer_handshake(&hs);

        assert_eq!(reg.peer_ut_metadata_id(), Some(1));
        assert_eq!(reg.peer_ut_pex_id(), Some(2));
        assert_eq!(reg.reqq(), 500);
    }

    #[test]
    fn test_update_from_peer_handshake_custom_ids() {
        let mut reg = ExtensionRegistry::new();
        let mut hs = ExtensionHandshake::new();
        hs.with_ut_metadata(3).with_ut_pex(5).with_reqq(1000);
        reg.update_from_peer_handshake(&hs);

        assert_eq!(reg.peer_ut_metadata_id(), Some(3));
        assert_eq!(reg.peer_ut_pex_id(), Some(5));
        assert_eq!(reg.reqq(), 1000);
    }

    #[test]
    fn test_update_from_peer_handshake_missing_ut_metadata() {
        let mut reg = ExtensionRegistry::new();
        // Build a handshake with only ut_pex
        let mut m_dict = BTreeMap::new();
        m_dict.insert(b"ut_pex".to_vec(), BencodeValue::Int(2));
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
        root.insert(b"reqq".to_vec(), BencodeValue::Int(500));
        let bytes = BencodeValue::Dict(root).encode();
        let hs = ExtensionHandshake::from_bytes(&bytes).unwrap();

        reg.update_from_peer_handshake(&hs);
        assert!(reg.peer_ut_metadata_id().is_none());
        assert_eq!(reg.peer_ut_pex_id(), Some(2));
    }

    #[test]
    fn test_update_from_peer_handshake_missing_ut_pex() {
        let mut reg = ExtensionRegistry::new();
        let mut m_dict = BTreeMap::new();
        m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
        root.insert(b"reqq".to_vec(), BencodeValue::Int(500));
        let bytes = BencodeValue::Dict(root).encode();
        let hs = ExtensionHandshake::from_bytes(&bytes).unwrap();

        reg.update_from_peer_handshake(&hs);
        assert_eq!(reg.peer_ut_metadata_id(), Some(1));
        assert!(reg.peer_ut_pex_id().is_none());
    }

    #[test]
    fn test_is_extension_enabled() {
        let mut reg = ExtensionRegistry::new();
        let hs = ExtensionHandshake::new();
        reg.update_from_peer_handshake(&hs);

        // ut_metadata has peer id 1, ut_pex has peer id 2
        assert!(reg.is_extension_enabled(1));
        assert!(reg.is_extension_enabled(2));
        assert!(!reg.is_extension_enabled(3));
        assert!(!reg.is_extension_enabled(0));
    }

    #[test]
    fn test_extension_name_for_id() {
        let mut reg = ExtensionRegistry::new();
        let hs = ExtensionHandshake::new();
        reg.update_from_peer_handshake(&hs);

        assert_eq!(reg.extension_name_for_id(1), Some(&UT_METADATA_NAME[..]));
        assert_eq!(reg.extension_name_for_id(2), Some(&UT_PEX_NAME[..]));
        assert_eq!(reg.extension_name_for_id(3), None);
    }

    #[test]
    fn test_supports_ut_metadata_and_pex() {
        let mut reg = ExtensionRegistry::new();
        assert!(!reg.supports_ut_metadata());
        assert!(!reg.supports_ut_pex());

        let hs = ExtensionHandshake::new();
        reg.update_from_peer_handshake(&hs);
        assert!(reg.supports_ut_metadata());
        assert!(reg.supports_ut_pex());
    }

    #[test]
    fn test_build_local_handshake() {
        let reg = ExtensionRegistry::new();
        let hs = reg.build_local_handshake();
        assert_eq!(hs.ut_metadata_id(), Some(1));
        assert_eq!(hs.ut_pex_id(), Some(2));
        assert_eq!(hs.reqq(), DEFAULT_REQQ);
    }

    #[test]
    fn test_update_from_peer_handshake_overwrites() {
        let mut reg = ExtensionRegistry::new();

        // First handshake
        let mut hs1 = ExtensionHandshake::new();
        hs1.with_ut_metadata(3).with_ut_pex(4);
        reg.update_from_peer_handshake(&hs1);
        assert_eq!(reg.peer_ut_metadata_id(), Some(3));
        assert_eq!(reg.peer_ut_pex_id(), Some(4));

        // Second handshake (should overwrite)
        let mut hs2 = ExtensionHandshake::new();
        hs2.with_ut_metadata(7).with_ut_pex(8);
        reg.update_from_peer_handshake(&hs2);
        assert_eq!(reg.peer_ut_metadata_id(), Some(7));
        assert_eq!(reg.peer_ut_pex_id(), Some(8));
    }

    // ====================== dispatch_extension_message tests ======================

    #[test]
    fn test_dispatch_handshake() {
        let mut reg = ExtensionRegistry::new();
        let hs = ExtensionHandshake::new();
        let payload = hs.to_bytes();

        let result = dispatch_extension_message(&mut reg, 0, &payload);
        assert!(result.is_some());

        match result.unwrap() {
            ExtensionUpdate::HandshakeReceived {
                ut_metadata_id,
                ut_pex_id,
                reqq,
            } => {
                assert_eq!(ut_metadata_id, Some(1));
                assert_eq!(ut_pex_id, Some(2));
                assert_eq!(reqq, 500);
            }
            other => panic!("Expected HandshakeReceived, got {:?}", other),
        }

        // Registry should now be updated
        assert_eq!(reg.peer_ut_metadata_id(), Some(1));
        assert_eq!(reg.peer_ut_pex_id(), Some(2));
    }

    #[test]
    fn test_dispatch_handshake_custom_ids() {
        let mut reg = ExtensionRegistry::new();
        let mut hs = ExtensionHandshake::new();
        hs.with_ut_metadata(5).with_ut_pex(6).with_reqq(200);
        let payload = hs.to_bytes();

        let result = dispatch_extension_message(&mut reg, 0, &payload);
        assert!(result.is_some());

        match result.unwrap() {
            ExtensionUpdate::HandshakeReceived {
                ut_metadata_id,
                ut_pex_id,
                reqq,
            } => {
                assert_eq!(ut_metadata_id, Some(5));
                assert_eq!(ut_pex_id, Some(6));
                assert_eq!(reqq, 200);
            }
            other => panic!("Expected HandshakeReceived, got {:?}", other),
        }
    }

    #[test]
    fn test_dispatch_handshake_invalid_payload() {
        let mut reg = ExtensionRegistry::new();
        let result = dispatch_extension_message(&mut reg, 0, b"not bencoded");
        assert!(result.is_none());
    }

    #[test]
    fn test_dispatch_ut_metadata_request() {
        let mut reg = ExtensionRegistry::new();
        // First, receive a handshake so the registry knows peer's ut_metadata id
        let hs = ExtensionHandshake::new();
        reg.update_from_peer_handshake(&hs);

        // Build a ut_metadata request payload
        let msg = UtMetadataMessage::Request { piece: 0 };
        let payload = msg.to_payload();

        // Peer's ut_metadata id is 1
        let result = dispatch_extension_message(&mut reg, 1, &payload);
        assert!(result.is_some());

        match result.unwrap() {
            ExtensionUpdate::MetadataRequest { piece } => {
                assert_eq!(piece, 0);
            }
            other => panic!("Expected MetadataRequest, got {:?}", other),
        }
    }

    #[test]
    fn test_dispatch_ut_metadata_data() {
        let mut reg = ExtensionRegistry::new();
        let hs = ExtensionHandshake::new();
        reg.update_from_peer_handshake(&hs);

        let metadata = b"hello world".to_vec();
        let msg = UtMetadataMessage::Data {
            piece: 2,
            total_size: 1000,
            data: metadata,
        };
        let payload = msg.to_payload();

        let result = dispatch_extension_message(&mut reg, 1, &payload);
        assert!(result.is_some());

        match result.unwrap() {
            ExtensionUpdate::MetadataPiece {
                piece,
                total_size,
                data,
            } => {
                assert_eq!(piece, 2);
                assert_eq!(total_size, 1000);
                assert_eq!(data, b"hello world");
            }
            other => panic!("Expected MetadataPiece, got {:?}", other),
        }
    }

    #[test]
    fn test_dispatch_ut_metadata_reject() {
        let mut reg = ExtensionRegistry::new();
        let hs = ExtensionHandshake::new();
        reg.update_from_peer_handshake(&hs);

        let msg = UtMetadataMessage::Reject { piece: 7 };
        let payload = msg.to_payload();

        let result = dispatch_extension_message(&mut reg, 1, &payload);
        assert!(result.is_some());

        match result.unwrap() {
            ExtensionUpdate::MetadataReject { piece } => {
                assert_eq!(piece, 7);
            }
            other => panic!("Expected MetadataReject, got {:?}", other),
        }
    }

    #[test]
    fn test_dispatch_ut_pex() {
        let mut reg = ExtensionRegistry::new();
        let hs = ExtensionHandshake::new();
        reg.update_from_peer_handshake(&hs);

        // Build a PEX message
        let mut pex = UtPexMessage::new();
        let mut peer1 = [0u8; 6];
        peer1[..4].copy_from_slice(&[192, 168, 1, 1]);
        peer1[4..6].copy_from_slice(&6881u16.to_be_bytes());
        pex.added.push(CompactPeerV4(peer1));

        let payload = pex.to_payload();

        // Peer's ut_pex id is 2
        let result = dispatch_extension_message(&mut reg, 2, &payload);
        assert!(result.is_some());

        match result.unwrap() {
            ExtensionUpdate::PeerExchange {
                added_v4,
                added_v6,
                dropped_v4,
                dropped_v6,
            } => {
                assert_eq!(added_v4.len(), 1);
                assert!(added_v6.is_empty());
                assert!(dropped_v4.is_empty());
                assert!(dropped_v6.is_empty());
                assert_eq!(added_v4[0], CompactPeerV4(peer1));
            }
            other => panic!("Expected PeerExchange, got {:?}", other),
        }
    }

    #[test]
    fn test_dispatch_unknown_ext_id() {
        let mut reg = ExtensionRegistry::new();
        let hs = ExtensionHandshake::new();
        reg.update_from_peer_handshake(&hs);

        // ext_id 99 is not assigned to any extension
        let result = dispatch_extension_message(&mut reg, 99, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_dispatch_no_peer_handshake_yet() {
        let mut reg = ExtensionRegistry::new();
        // No handshake received yet, so peer IDs are None

        // ext_id 1 with some payload — unknown because no handshake yet
        let result = dispatch_extension_message(&mut reg, 1, &[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_dispatch_ut_metadata_with_custom_peer_id() {
        let mut reg = ExtensionRegistry::new();
        // Peer uses ut_metadata=5, ut_pex=6
        let mut hs = ExtensionHandshake::new();
        hs.with_ut_metadata(5).with_ut_pex(6);
        reg.update_from_peer_handshake(&hs);

        let msg = UtMetadataMessage::Request { piece: 3 };
        let payload = msg.to_payload();

        // Must use peer's id (5), not our local id (1)
        let result = dispatch_extension_message(&mut reg, 5, &payload);
        assert!(result.is_some());

        match result.unwrap() {
            ExtensionUpdate::MetadataRequest { piece } => {
                assert_eq!(piece, 3);
            }
            other => panic!("Expected MetadataRequest, got {:?}", other),
        }

        // Using our local id (1) should not match
        let result2 = dispatch_extension_message(&mut reg, 1, &payload);
        assert!(result2.is_none());
    }

    #[test]
    fn test_dispatch_ut_pex_with_custom_peer_id() {
        let mut reg = ExtensionRegistry::new();
        let mut hs = ExtensionHandshake::new();
        hs.with_ut_metadata(5).with_ut_pex(6);
        reg.update_from_peer_handshake(&hs);

        let pex = UtPexMessage::new();
        let payload = pex.to_payload();

        // Must use peer's id (6)
        let result = dispatch_extension_message(&mut reg, 6, &payload);
        assert!(result.is_some());

        match result.unwrap() {
            ExtensionUpdate::PeerExchange {
                added_v4,
                added_v6,
                dropped_v4,
                dropped_v6,
            } => {
                assert!(added_v4.is_empty());
                assert!(added_v6.is_empty());
                assert!(dropped_v4.is_empty());
                assert!(dropped_v6.is_empty());
            }
            other => panic!("Expected PeerExchange, got {:?}", other),
        }
    }

    #[test]
    fn test_dispatch_invalid_ut_metadata_payload() {
        let mut reg = ExtensionRegistry::new();
        let hs = ExtensionHandshake::new();
        reg.update_from_peer_handshake(&hs);

        let result = dispatch_extension_message(&mut reg, 1, b"garbage");
        assert!(result.is_none());
    }

    #[test]
    fn test_dispatch_invalid_ut_pex_payload() {
        let mut reg = ExtensionRegistry::new();
        let hs = ExtensionHandshake::new();
        reg.update_from_peer_handshake(&hs);

        // Invalid compact peer data length (5 bytes, not multiple of 6)
        let mut dict = BTreeMap::new();
        dict.insert(b"added".to_vec(), BencodeValue::Bytes(vec![1, 2, 3, 4, 5]));
        let payload = BencodeValue::Dict(dict).encode();

        let result = dispatch_extension_message(&mut reg, 2, &payload);
        assert!(result.is_none());
    }

    #[test]
    fn test_dispatch_full_roundtrip_handshake_then_metadata() {
        let mut reg = ExtensionRegistry::new();

        // Step 1: Receive handshake
        let hs = ExtensionHandshake::new();
        let hs_payload = hs.to_bytes();
        let result = dispatch_extension_message(&mut reg, 0, &hs_payload);
        assert!(result.is_some());

        // Step 2: Receive ut_metadata request
        let msg = UtMetadataMessage::Request { piece: 0 };
        let meta_payload = msg.to_payload();
        let result = dispatch_extension_message(&mut reg, 1, &meta_payload);
        assert!(result.is_some());

        // Step 3: Receive ut_pex message
        let pex = UtPexMessage::new();
        let pex_payload = pex.to_payload();
        let result = dispatch_extension_message(&mut reg, 2, &pex_payload);
        assert!(result.is_some());
    }

    #[test]
    fn test_extension_update_debug_format() {
        let update = ExtensionUpdate::HandshakeReceived {
            ut_metadata_id: Some(1),
            ut_pex_id: Some(2),
            reqq: 500,
        };
        let s = format!("{:?}", update);
        assert!(s.contains("HandshakeReceived"));

        let update = ExtensionUpdate::MetadataPiece {
            piece: 0,
            total_size: 1000,
            data: vec![1, 2, 3],
        };
        let s = format!("{:?}", update);
        assert!(s.contains("MetadataPiece"));

        let update = ExtensionUpdate::MetadataRequest { piece: 5 };
        let s = format!("{:?}", update);
        assert!(s.contains("MetadataRequest"));

        let update = ExtensionUpdate::MetadataReject { piece: 5 };
        let s = format!("{:?}", update);
        assert!(s.contains("MetadataReject"));

        let update = ExtensionUpdate::PeerExchange {
            added_v4: Vec::new(),
            added_v6: Vec::new(),
            dropped_v4: Vec::new(),
            dropped_v6: Vec::new(),
        };
        let s = format!("{:?}", update);
        assert!(s.contains("PeerExchange"));
    }

    #[test]
    fn test_registry_roundtrip_with_handshake() {
        let reg = ExtensionRegistry::new();
        let hs = reg.build_local_handshake();
        let bytes = hs.to_bytes();
        let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.ut_metadata_id(), Some(reg.local_ut_metadata_id()));
        assert_eq!(parsed.ut_pex_id(), Some(reg.local_ut_pex_id()));
    }
}

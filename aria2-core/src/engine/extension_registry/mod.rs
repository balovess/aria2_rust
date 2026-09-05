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

pub mod lookup;
pub mod registration;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use aria2_protocol::bittorrent::message::extension::{CompactPeerV4, CompactPeerV6};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default local ext_id for ut_metadata (matches C++ DefaultExtensionMessageFactory).
pub(super) const DEFAULT_LOCAL_UT_METADATA_ID: u8 = 1;

/// Default local ext_id for ut_pex (matches C++ DefaultExtensionMessageFactory).
pub(super) const DEFAULT_LOCAL_UT_PEX_ID: u8 = 2;

/// Default reqq value (max outstanding metadata requests).
pub(super) const DEFAULT_REQQ: u32 = 500;

/// Extension name for ut_metadata (BEP 9).
pub(super) const UT_METADATA_NAME: &[u8] = b"ut_metadata";

/// Extension name for ut_pex (BEP 11).
pub(super) const UT_PEX_NAME: &[u8] = b"ut_pex";

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
    /// Our local ext_id assignments. These are fixed by the protocol
    /// implementation and do not need per-peer hash tables or allocations.
    pub(super) local_ut_metadata_id: u8,
    pub(super) local_ut_pex_id: u8,

    /// Peer's ext_id -> extension name (for dispatch and capability lookup).
    /// A single map avoids storing every negotiated name in both directions.
    pub(super) peer_id_to_name: HashMap<u8, Box<[u8]>>,

    /// The reqq value from the peer's handshake (max outstanding metadata reqs).
    pub(super) reqq: u32,
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ExtensionUpdate
// ---------------------------------------------------------------------------

/// Side effect from processing an extension message.
///
/// Returned by [`dispatch_extension_message`](lookup::dispatch_extension_message)
/// so the interaction loop caller can apply the appropriate side effects
/// (e.g., storing metadata, adding discovered peers, etc.).
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
// Re-exports
// ---------------------------------------------------------------------------

pub use lookup::dispatch_extension_message;

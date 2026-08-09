//! BtPeerMessageHandler — per-peer stateful message handler.
//!
//! Mirrors C++ `DefaultBtInteractive` which owns a `DefaultBtMessageDispatcher`
//! per peer connection. This struct provides:
//!
//! - **Request slot tracking**: Outstanding download requests are tracked via
//!   the embedded [`BtMessageDispatcher`]. Sending a Request creates a slot;
//!   receiving the corresponding Piece removes it.
//!
//! - **Event-driven actions**: Receiving Choke, Cancel, or sending Choke
//!   triggers dispatcher actions that prune/invalidate messages and slots.
//!
//! - **Flooding detection**: Uses [`FloodingStat`] to detect peers that
//!   spam choke/unchoke transitions or keepalive messages.
//!
//! - **Timeout detection**: Periodic `check_request_slots()` identifies
//!   timed-out requests and marks the peer as snubbing.
//!
//! - **Outstanding request limiting**: Enforces a configurable maximum on
//!   concurrent outstanding requests, matching C++ `maxOutstandingRequest_`.

mod choke_state;
mod extension;
mod maintenance;
mod message_handlers;
mod request_lifecycle;

use std::collections::HashSet;

use crate::engine::bt_message_dispatcher::{BtMessageDispatcher, FloodingStat};

use super::types::DEFAULT_MAX_OUTSTANDING_REQUEST;

/// Per-peer stateful BitTorrent message handler.
///
/// Wraps a [`BtMessageDispatcher`] with event-driven actions, flooding
/// detection, request slot tracking, and choking state management.
/// Mirrors C++ `DefaultBtInteractive`.
pub struct BtPeerMessageHandler {
    /// Embedded message dispatcher for outgoing queue + request slots.
    /// `pub(crate)` for test access.
    pub(crate) dispatcher: BtMessageDispatcher,
    /// Anti-flooding stat tracker.
    /// `pub(crate)` for test access.
    pub(crate) flooding_stat: FloodingStat,
    /// Maximum concurrent outstanding download requests.
    pub(crate) max_outstanding_requests: usize,
    /// Whether this peer has been marked as snubbing (timed-out request).
    /// `pub(crate)` for test access.
    pub(crate) peer_snubbing: bool,
    /// Whether we are currently choked by this peer.
    /// `pub(crate)` for test access.
    pub(crate) peer_choking: bool,
    /// Whether the remote peer is interested in our data.
    /// Mirrors C++ `peer->peerInterested`.
    /// `pub(crate)` for test access.
    pub(crate) peer_interested: bool,
    /// Whether we are choking the remote peer.
    /// Mirrors C++ `peer->amChoking()`.
    /// `pub(crate)` for test access.
    pub(crate) am_choking: bool,
    /// Whether the fast extension is enabled for this peer.
    /// When true, Reject/AllowedFast messages are valid.
    /// `pub(crate)` for test access.
    pub(crate) fast_extension_enabled: bool,
    /// Whether we are in metadata-get mode (metadata-only download).
    /// When true, certain side effects are skipped.
    /// Mirrors C++ `isMetadataGetMode_`.
    /// `pub(crate)` for test access.
    pub(crate) metadata_get_mode: bool,
    /// Set of piece indices the peer has allowed us to download even while
    /// choking (fast extension). Mirrors C++ `peer->getPeerAllowedIndexSet()`.
    /// `pub(crate)` for test access.
    pub(crate) peer_allowed_fast_set: HashSet<u32>,
}

impl BtPeerMessageHandler {
    /// Create a new per-peer message handler with default settings.
    ///
    /// # Arguments
    /// * `block_size` — Block size for block index calculation (typically 16384).
    pub fn new(block_size: u32) -> Self {
        Self {
            dispatcher: BtMessageDispatcher::new(block_size),
            flooding_stat: FloodingStat::new(),
            max_outstanding_requests: DEFAULT_MAX_OUTSTANDING_REQUEST,
            peer_snubbing: false,
            peer_choking: true, // Peers start choked
            peer_interested: false,
            am_choking: true, // We start choking the peer
            fast_extension_enabled: false,
            metadata_get_mode: false,
            peer_allowed_fast_set: HashSet::new(),
        }
    }

    /// Create a new handler with a custom max outstanding request count.
    pub fn with_max_outstanding(block_size: u32, max_outstanding: usize) -> Self {
        Self {
            dispatcher: BtMessageDispatcher::new(block_size),
            flooding_stat: FloodingStat::new(),
            max_outstanding_requests: max_outstanding,
            peer_snubbing: false,
            peer_choking: true,
            peer_interested: false,
            am_choking: true,
            fast_extension_enabled: false,
            metadata_get_mode: false,
            peer_allowed_fast_set: HashSet::new(),
        }
    }
}

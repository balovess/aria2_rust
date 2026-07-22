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
//! - `src/DefaultBtMessageDispatcher.h` - Message queue + request slots
//! - `src/DefaultBtInteractive.h` - Per-peer interaction loop
//! - `src/PeerInteractionCommand.h` - Peer interaction
//!
//! # Module Structure
//!
//! - [`BtPeerMessageHandler`] — Per-peer stateful handler wrapping a
//!   [`BtMessageDispatcher`] with event-driven actions, flooding detection,
//!   and request slot tracking. Mirrors C++ `DefaultBtInteractive`.
//! - [`BtMessageHandler`] — Legacy stateless block request/receive utilities
//!   (kept for backward compatibility; prefer `BtPeerMessageHandler`).

use std::collections::HashSet;

use crate::constants;
use crate::engine::bt_download_execute::EndgameState;
use crate::engine::bt_message_dispatcher::{
    BtMessageDispatcher, FloodingStat, RequestSlot, SlotCheckResult,
};
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use tracing::{debug, info, trace, warn};

/// Block size for each piece block request (16 KB)
pub const BLOCK_SIZE: u32 = constants::BT_BLOCK_SIZE as u32;

/// Maximum number of retries for a failed piece download
pub const MAX_RETRIES: u32 = constants::BT_MAX_RETRIES;

/// Timeout for each block request (seconds)
pub const BLOCK_REQUEST_TIMEOUT_SECS: u64 = constants::BT_BLOCK_REQUEST_TIMEOUT_SECS;

/// Maximum messages to read while waiting for a specific block
pub const MAX_BLOCK_READ_MESSAGES: u32 = constants::BT_MAX_BLOCK_READ_MESSAGES as u32;

/// Default maximum outstanding requests per peer.
/// Matches C++ `DEFAULT_MAX_OUTSTANDING_REQUEST = 6` (BtConstants.h).
pub const DEFAULT_MAX_OUTSTANDING_REQUEST: usize =
    constants::BT_DEFAULT_MAX_OUTSTANDING_REQUEST;

/// Upper bound for max outstanding request auto-scaling.
/// Matches C++ `UB_MAX_OUTSTANDING_REQUEST = 256` (BtConstants.h).
pub const UB_MAX_OUTSTANDING_REQUEST: usize = constants::BT_UB_MAX_OUTSTANDING_REQUEST;

// ======================================================================
// BtPeerMessageHandler — per-peer stateful message handler
// ======================================================================

/// Per-peer BitTorrent message handler with dispatcher integration.
///
/// Mirrors C++ `DefaultBtInteractive` which owns a `DefaultBtMessageDispatcher`
/// per peer connection. This struct provides:
///
/// - **Request slot tracking**: Outstanding download requests are tracked via
///   the embedded [`BtMessageDispatcher`]. Sending a Request creates a slot;
///   receiving the corresponding Piece removes it.
///
/// - **Event-driven actions**: Receiving Choke, Cancel, or sending Choke
///   triggers dispatcher actions that prune/invalidate messages and slots.
///
/// - **Flooding detection**: Uses [`FloodingStat`] to detect peers that
///   spam choke/unchoke transitions or keepalive messages.
///
/// - **Timeout detection**: Periodic `check_request_slots()` identifies
///   timed-out requests and marks the peer as snubbing.
///
/// - **Outstanding request limiting**: Enforces a configurable maximum on
///   concurrent outstanding requests, matching C++ `maxOutstandingRequest_`.
/// Side-effect update that the caller must apply to the peer and piece storage.
///
/// The handler does not own `PieceStorage` or `PeerStorage`, so it returns
/// these updates for the caller to apply. This mirrors the C++ pattern where
/// `doReceivedAction()` mutates peer/piece-storage directly.
#[derive(Debug, Clone)]
pub enum PeerStateUpdate {
    /// The peer now has the given piece index (Have message).
    HavePiece { index: u32 },
    /// The peer's bitfield has been set to the given data (Bitfield message).
    SetBitfield { data: Vec<u8> },
    /// The peer is now a seeder — has all pieces (HaveAll message).
    MarkSeeder,
    /// The peer has no pieces (HaveNone message).
    ClearBitfield,
    /// The choking algorithm should be re-evaluated (Interested/NotInterested
    /// when choking state is relevant).
    ExecuteChoke,
    /// Disconnect: the peer is a seeder and our download is finished.
    DisconnectSeeder,
}

/// Response to an incoming Request message (ID=6).
///
/// Mirrors C++ `BtRequestMessage::doReceivedAction()` which either queues a
/// Piece message, a Reject message, or drops the request silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestResponse {
    /// Queue a Piece message with the given data.
    Piece {
        index: u32,
        begin: u32,
        length: u32,
    },
    /// Queue a Reject message (fast extension).
    Reject {
        index: u32,
        begin: u32,
        length: u32,
    },
    /// Drop the request silently (choking without fast extension).
    None,
}

pub struct BtPeerMessageHandler {
    /// Embedded message dispatcher for outgoing queue + request slots.
    /// `pub(crate)` for test access.
    pub(crate) dispatcher: BtMessageDispatcher,
    /// Anti-flooding stat tracker.
    /// `pub(crate)` for test access.
    pub(crate) flooding_stat: FloodingStat,
    /// Maximum concurrent outstanding download requests.
    max_outstanding_requests: usize,
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

    // ── Request Lifecycle ────────────────────────────────────────────────

    /// Queue a Request message and track it as an outstanding request slot.
    ///
    /// The serialized message bytes are added to the dispatcher's outgoing queue
    /// and a [`RequestSlot`] is created to track the outstanding request.
    ///
    /// Returns the serialized Request message bytes if the request was accepted,
    /// or `None` if the max outstanding limit has been reached.
    ///
    /// Mirrors C++ `BtRequestFactory::createRequestMessage()` followed by
    /// `dispatcher_->addMessageToQueue()` and `addOutstandingRequest()`.
    pub fn send_request(
        &mut self,
        index: u32,
        begin: u32,
        length: u32,
        serialized_msg: Vec<u8>,
    ) -> Option<Vec<u8>> {
        if !self.can_send_request() {
            debug!(
                "PeerHandler: max outstanding requests ({}) reached, deferring request \
                 (piece={}, begin={})",
                self.max_outstanding_requests, index, begin
            );
            return None;
        }

        // Add request slot first so the slot is tracked even if queue add fails
        self.dispatcher.add_request_slot(index, begin, length);
        // Queue the serialized request message
        self.dispatcher
            .add_request_message(serialized_msg, index, begin, length);

        debug!(
            "PeerHandler: queued request for piece={} begin={} len={} (outstanding={})",
            index,
            begin,
            length,
            self.count_outstanding_requests()
        );

        // Return a freshly serialized copy for the caller to send immediately.
        // The original bytes were moved into the queue; re-serializing avoids
        // cloning the entire buffer.
        Some(
            aria2_protocol::bittorrent::message::serializer::serialize(
                &aria2_protocol::bittorrent::message::types::BtMessage::Request {
                    request: aria2_protocol::bittorrent::message::types::PieceBlockRequest::new(
                        index, begin, length,
                    ),
                },
            ),
        )
    }

    /// Handle receiving a Piece message from the peer.
    ///
    /// Removes the matching request slot from the dispatcher.
    /// Returns the removed [`RequestSlot`] if found, or `None` if the
    /// piece data was unsolicited (no matching outstanding request).
    ///
    /// Mirrors C++ `BtPieceMessage::doReceivedAction()` which calls
    /// `dispatcher_->removeOutstandingRequest()`.
    pub fn on_piece_received(&mut self, index: u32, begin: u32, length: u32) -> Option<RequestSlot> {
        if self.dispatcher.remove_request_slot(index, begin, length) {
            debug!(
                "PeerHandler: piece received matched outstanding request (piece={}, begin={})",
                index, begin
            );
            // Return a reconstructed slot for caller bookkeeping
            Some(RequestSlot::new(index, begin, length, BLOCK_SIZE))
        } else {
            debug!(
                "PeerHandler: received unsolicited piece data (piece={}, begin={})",
                index, begin
            );
            None
        }
    }

    // ── Event-Driven Actions ─────────────────────────────────────────────

    /// Handle receiving a Choke message from the peer.
    ///
    /// Removes outstanding request slots for pieces NOT in the allowed-fast set.
    /// Returns removed slots so the caller can send Cancel messages if needed.
    ///
    /// Mirrors C++ `DefaultBtMessageDispatcher::doChokedAction()`.
    pub fn on_choke_received<F>(&mut self, is_in_allowed_fast: F) -> Vec<RequestSlot>
    where
        F: Fn(u32) -> bool,
    {
        self.peer_choking = true;
        self.flooding_stat.inc_choke_unchoke_count();

        let removed = self.dispatcher.do_choked_action(is_in_allowed_fast);
        if !removed.is_empty() {
            debug!(
                "PeerHandler: choke received, removed {} outstanding requests",
                removed.len()
            );
        }
        removed
    }

    /// Handle receiving an Unchoke message from the peer.
    ///
    /// Updates the choking state and increments the flooding stat counter.
    /// Mirrors C++ `DefaultBtInteractive::receiveMessages()` which calls
    /// `floodingStat_.incChokeUnchokeCount()` on state transitions.
    pub fn on_unchoke_received(&mut self) {
        self.peer_choking = false;
        self.flooding_stat.inc_choke_unchoke_count();
        debug!("PeerHandler: unchoke received");
    }

    /// Handle sending a Choke message to the peer.
    ///
    /// Invalidates all queued Piece upload messages since we are choking
    /// the peer and should not send them data.
    ///
    /// Mirrors C++ `DefaultBtMessageDispatcher::doChokingAction()`.
    pub fn on_choke_sent(&mut self) {
        self.dispatcher.do_choking_action();
        debug!("PeerHandler: choke sent, invalidated upload messages");
    }

    /// Handle receiving a Cancel message from the peer.
    ///
    /// Invalidates any queued Piece message that matches the specified block.
    /// Mirrors C++ `DefaultBtMessageDispatcher::doCancelSendingPieceAction()`.
    pub fn on_cancel_received(&mut self, index: u32, begin: u32, length: u32) {
        self.dispatcher
            .do_cancel_sending_piece_action(index, begin, length);
        debug!(
            "PeerHandler: cancel received for piece={} begin={} len={}",
            index, begin, length
        );
    }

    /// Handle receiving a KeepAlive message from the peer.
    ///
    /// Increments the flooding stat keepalive counter.
    /// Mirrors C++ `DefaultBtInteractive::receiveMessages()` which calls
    /// `floodingStat_.incKeepAliveCount()` for KeepAlive messages.
    pub fn on_keepalive_received(&mut self) {
        self.flooding_stat.inc_keepalive_count();
        debug!("PeerHandler: keepalive received");
    }

    // ── Message Side-Effect Handlers ────────────────────────────────────

    /// Handle receiving a Have message (ID=4).
    ///
    /// Returns [`PeerStateUpdate::HavePiece`] so the caller can update the
    /// peer's bitfield and piece stats. If the peer becomes a seeder and
    /// our download is finished, also returns [`PeerStateUpdate::DisconnectSeeder`].
    ///
    /// Mirrors C++ `BtHaveMessage::doReceivedAction()`.
    pub fn on_have_received(
        &mut self,
        piece_index: u32,
        is_seeder_after: bool,
        download_finished: bool,
    ) -> Vec<PeerStateUpdate> {
        trace!("PeerHandler: Have received for piece {}", piece_index);

        let mut updates = vec![PeerStateUpdate::HavePiece {
            index: piece_index,
        }];

        if is_seeder_after && download_finished {
            debug!(
                "PeerHandler: peer became seeder after Have({}) and download finished — disconnect",
                piece_index
            );
            updates.push(PeerStateUpdate::DisconnectSeeder);
        }

        updates
    }

    /// Handle receiving a Bitfield message (ID=5).
    ///
    /// Returns [`PeerStateUpdate::SetBitfield`] so the caller can update
    /// piece stats and the peer's bitfield. If the peer is a seeder and
    /// our download is finished, also returns [`PeerStateUpdate::DisconnectSeeder`].
    ///
    /// Mirrors C++ `BtBitfieldMessage::doReceivedAction()`.
    pub fn on_bitfield_received(
        &mut self,
        bitfield: Vec<u8>,
        is_seeder: bool,
        download_finished: bool,
    ) -> Vec<PeerStateUpdate> {
        trace!(
            "PeerHandler: Bitfield received ({} bytes, seeder={})",
            bitfield.len(),
            is_seeder
        );

        let mut updates = vec![PeerStateUpdate::SetBitfield {
            data: bitfield,
        }];

        if is_seeder && download_finished {
            debug!(
                "PeerHandler: peer is seeder per Bitfield and download finished — disconnect"
            );
            updates.push(PeerStateUpdate::DisconnectSeeder);
        }

        updates
    }

    /// Handle receiving an Interested message (ID=2).
    ///
    /// Sets `peer_interested = true`. If we are choking the peer, triggers
    /// [`PeerStateUpdate::ExecuteChoke`] so the caller can re-evaluate the
    /// choking algorithm (unchoke if appropriate).
    ///
    /// Mirrors C++ `BtInterestedMessage::doReceivedAction()`.
    pub fn on_interested_received(&mut self) -> Vec<PeerStateUpdate> {
        self.peer_interested = true;
        trace!("PeerHandler: Interested received (peer_interested=true)");

        if self.am_choking {
            debug!("PeerHandler: Interested while am_choking — trigger executeChoke");
            vec![PeerStateUpdate::ExecuteChoke]
        } else {
            vec![]
        }
    }

    /// Handle receiving a NotInterested message (ID=3).
    ///
    /// Sets `peer_interested = false`. If we are NOT choking the peer,
    /// triggers [`PeerStateUpdate::ExecuteChoke`] so the caller can
    /// re-evaluate the choking algorithm (may choke this peer to free
    /// an upload slot).
    ///
    /// Mirrors C++ `BtNotInterestedMessage::doReceivedAction()`.
    pub fn on_not_interested_received(&mut self) -> Vec<PeerStateUpdate> {
        self.peer_interested = false;
        trace!("PeerHandler: NotInterested received (peer_interested=false)");

        if !self.am_choking {
            debug!("PeerHandler: NotInterested while not am_choking — trigger executeChoke");
            vec![PeerStateUpdate::ExecuteChoke]
        } else {
            vec![]
        }
    }

    /// Handle receiving a Request message (ID=6) for upload.
    ///
    /// Returns the appropriate [`RequestResponse`]:
    /// - `Piece` if we have the piece and are not choking (or it's in our
    ///   allowed-fast set) — the caller should queue the piece data.
    /// - `Reject` if we are choking and fast extension is enabled.
    /// - `None` if we are choking and fast extension is NOT enabled (drop).
    ///
    /// The `has_piece` closure checks whether we have the requested piece.
    /// The `is_in_am_allowed_fast` closure checks whether the piece index
    /// is in our allowed-fast set (fast extension).
    ///
    /// Mirrors C++ `BtRequestMessage::doReceivedAction()`.
    pub fn on_request_received<F1, F2>(
        &self,
        index: u32,
        begin: u32,
        length: u32,
        has_piece: F1,
        is_in_am_allowed_fast: F2,
    ) -> RequestResponse
    where
        F1: Fn(u32) -> bool,
        F2: Fn(u32) -> bool,
    {
        if has_piece(index) && (!self.am_choking || is_in_am_allowed_fast(index)) {
            trace!(
                "PeerHandler: Request received for piece={} begin={} len={} — will send Piece",
                index, begin, length
            );
            RequestResponse::Piece {
                index,
                begin,
                length,
            }
        } else if self.fast_extension_enabled {
            debug!(
                "PeerHandler: Request rejected (choking, fast ext) for piece={} begin={} len={}",
                index, begin, length
            );
            RequestResponse::Reject {
                index,
                begin,
                length,
            }
        } else {
            debug!(
                "PeerHandler: Request dropped (choking, no fast ext) for piece={} begin={} len={}",
                index, begin, length
            );
            RequestResponse::None
        }
    }

    /// Handle receiving a Reject message (ID=13).
    ///
    /// Removes the matching outstanding request slot from the dispatcher.
    /// Returns an error if fast extension is not enabled (per spec, Reject
    /// is only valid with fast extension).
    ///
    /// Mirrors C++ `BtRejectMessage::doReceivedAction()`.
    pub fn on_reject_received(
        &mut self,
        index: u32,
        begin: u32,
        length: u32,
    ) -> std::result::Result<(), String> {
        if !self.fast_extension_enabled {
            let msg = format!(
                "Reject message received but fast extension is not enabled (piece={}, begin={}, len={})",
                index, begin, length
            );
            warn!("PeerHandler: {}", msg);
            return Err(msg);
        }

        let removed = self.dispatcher.remove_request_slot(index, begin, length);
        if removed {
            debug!(
                "PeerHandler: Reject received, removed outstanding request (piece={}, begin={})",
                index, begin
            );
        } else {
            trace!(
                "PeerHandler: Reject received but no matching outstanding request (piece={}, begin={})",
                index, begin
            );
        }
        Ok(())
    }

    /// Handle receiving an AllowedFast message (ID=11).
    ///
    /// Adds the piece index to the peer's allowed-fast set.
    /// Returns an error if fast extension is not enabled.
    ///
    /// Mirrors C++ `BtAllowedFastMessage::doReceivedAction()`.
    pub fn on_allowed_fast_received(&mut self, index: u32) -> std::result::Result<(), String> {
        if !self.fast_extension_enabled {
            let msg = format!(
                "AllowedFast message received but fast extension is not enabled (piece={})",
                index
            );
            warn!("PeerHandler: {}", msg);
            return Err(msg);
        }

        self.peer_allowed_fast_set.insert(index);
        trace!(
            "PeerHandler: AllowedFast received for piece {} (set size={})",
            index,
            self.peer_allowed_fast_set.len()
        );
        Ok(())
    }

    /// Handle receiving a HaveAll message (ID=14).
    ///
    /// Returns [`PeerStateUpdate::MarkSeeder`] so the caller can mark the peer
    /// as a seeder and update piece stats. If download is finished, also
    /// returns [`PeerStateUpdate::DisconnectSeeder`].
    ///
    /// Mirrors C++ `BtHaveAllMessage::doReceivedAction()`.
    pub fn on_have_all_received(&mut self, download_finished: bool) -> Vec<PeerStateUpdate> {
        trace!("PeerHandler: HaveAll received");

        let mut updates = vec![PeerStateUpdate::MarkSeeder];

        if download_finished {
            debug!("PeerHandler: HaveAll and download finished — disconnect seeder");
            updates.push(PeerStateUpdate::DisconnectSeeder);
        }

        updates
    }

    /// Handle receiving a HaveNone message (ID=15).
    ///
    /// Returns [`PeerStateUpdate::ClearBitfield`] so the caller can update
    /// piece stats and clear the peer's bitfield.
    ///
    /// Mirrors C++ `BtHaveNoneMessage::doReceivedAction()`.
    pub fn on_have_none_received(&mut self) -> Vec<PeerStateUpdate> {
        trace!("PeerHandler: HaveNone received");
        vec![PeerStateUpdate::ClearBitfield]
    }

    /// Handle receiving a SuggestPiece message (ID=12).
    ///
    /// Currently a no-op — the C++ implementation also ignores this message
    /// (TODO in original code). May be used in the future for piece priority
    /// boosting.
    ///
    /// Mirrors C++ `BtSuggestPieceMessage::doReceivedAction()`.
    pub fn on_suggest_received(&mut self, index: u32) {
        trace!(
            "PeerHandler: SuggestPiece received for piece {} (currently ignored)",
            index
        );
    }

    /// Handle receiving a Port message (ID=9).
    ///
    /// If DHT is enabled and the port is non-zero, the caller should create
    /// a DHT node and ping it. If bootstrap is needed, the caller should
    /// initiate a node_lookup task.
    ///
    /// This handler only logs the event; actual DHT operations are delegated
    /// to the caller.
    ///
    /// Mirrors C++ `BtPortMessage::doReceivedAction()`.
    pub fn on_port_received(&mut self, port: u16) {
        if port != 0 {
            trace!("PeerHandler: Port received (port={}), DHT action delegated to caller", port);
        } else {
            trace!("PeerHandler: Port received (port=0), ignoring");
        }
    }

    /// Handle receiving an Extended message (ID=20).
    ///
    /// Delegates to the extension message handler. This handler only logs
    /// the event; actual processing is delegated to the caller.
    ///
    /// Mirrors C++ `BtExtendedMessage::doReceivedAction()` which calls
    /// `extensionMessage->doReceivedAction()`.
    pub fn on_extended_received(&mut self, ext_id: u8, payload: &[u8]) {
        trace!(
            "PeerHandler: Extended message received (ext_id={}, payload_len={})",
            ext_id,
            payload.len()
        );
    }

    // ── Fast Extension & Choking State Accessors ────────────────────────

    /// Check if fast extension is enabled for this peer.
    pub fn is_fast_extension_enabled(&self) -> bool {
        self.fast_extension_enabled
    }

    /// Enable or disable the fast extension for this peer.
    ///
    /// Should be called once when the handshake completes and the fast
    /// extension bit is set in the reserved bytes.
    pub fn set_fast_extension_enabled(&mut self, enabled: bool) {
        self.fast_extension_enabled = enabled;
        debug!("PeerHandler: fast extension set to {}", enabled);
    }

    /// Set whether we are choking the peer (amChoking).
    ///
    /// Mirrors C++ `peer->amChoking(true/false)`.
    pub fn set_am_choking(&mut self, choking: bool) {
        self.am_choking = choking;
    }

    /// Check if we are choking the peer.
    pub fn is_am_choking(&self) -> bool {
        self.am_choking
    }

    /// Check if the peer is interested in our data.
    pub fn is_peer_interested(&self) -> bool {
        self.peer_interested
    }

    /// Set metadata-get mode.
    ///
    /// When true, certain side effects (e.g., bitfield updates) are skipped
    /// because we only need metadata, not actual piece data.
    pub fn set_metadata_get_mode(&mut self, mode: bool) {
        self.metadata_get_mode = mode;
        debug!("PeerHandler: metadata_get_mode set to {}", mode);
    }

    /// Check if metadata-get mode is active.
    pub fn is_metadata_get_mode(&self) -> bool {
        self.metadata_get_mode
    }

    /// Check if a piece index is in the peer's allowed-fast set.
    pub fn is_in_peer_allowed_fast(&self, index: u32) -> bool {
        self.peer_allowed_fast_set.contains(&index)
    }

    /// Handle aborting all outstanding requests for a specific piece.
    ///
    /// Called when a piece is reassigned to another peer. Removes matching
    /// request slots and invalidates queued Request messages.
    ///
    /// Mirrors C++ `DefaultBtMessageDispatcher::doAbortOutstandingRequestAction()`.
    pub fn abort_piece_requests(&mut self, piece_index: u32) -> Vec<RequestSlot> {
        let removed = self
            .dispatcher
            .do_abort_outstanding_request_action(piece_index);
        if !removed.is_empty() {
            debug!(
                "PeerHandler: aborted {} outstanding requests for piece {}",
                removed.len(),
                piece_index
            );
        }
        removed
    }

    // ── Periodic Maintenance ─────────────────────────────────────────────

    /// Check request slots for timeouts and already-acquired blocks.
    ///
    /// Should be called approximately once per second, matching the C++
    /// `perSecTimer_` pattern in `DefaultBtInteractive::doInteractionProcessing()`.
    ///
    /// If any slot times out, the peer is marked as snubbing.
    /// If any block has been acquired from another peer, a Cancel is needed.
    ///
    /// Returns a [`SlotCheckResult`] with:
    /// - `timed_out` — true if any slot timed out (caller should mark peer snubbing)
    /// - `cancelled_blocks` — blocks acquired elsewhere that need Cancel messages
    pub fn check_request_slots<F>(&mut self, is_block_acquired: F) -> SlotCheckResult
    where
        F: Fn(u32, u32) -> bool,
    {
        let result = self.dispatcher.check_request_slots(is_block_acquired);

        if result.timed_out {
            self.peer_snubbing = true;
            warn!("PeerHandler: peer marked as snubbing (request timeout detected)");
        }

        result
    }

    /// Detect message flooding from this peer.
    ///
    /// Checks the [`FloodingStat`] counters and resets them if the check
    /// interval has elapsed. Returns true if flooding was detected.
    ///
    /// Mirrors C++ `DefaultBtInteractive::detectMessageFlooding()`.
    /// The caller should disconnect the peer if this returns true.
    pub fn detect_flooding(&mut self) -> bool {
        self.flooding_stat.check_and_reset()
    }

    // ── Outstanding Request Queries ──────────────────────────────────────

    /// Check if we can send another request (below max outstanding limit).
    pub fn can_send_request(&self) -> bool {
        self.count_outstanding_requests() < self.max_outstanding_requests
    }

    /// Return the number of outstanding request slots.
    pub fn count_outstanding_requests(&self) -> usize {
        self.dispatcher.count_request_slots()
    }

    /// Check if there are any outstanding requests.
    pub fn has_outstanding_requests(&self) -> bool {
        self.dispatcher.has_outstanding_requests()
    }

    /// Check if there is an outstanding request for the given piece+block.
    pub fn is_outstanding_request(&self, index: u32, block_index: u32) -> bool {
        self.dispatcher.is_outstanding_request(index, block_index)
    }

    /// Return whether this peer has been marked as snubbing.
    pub fn is_peer_snubbing(&self) -> bool {
        self.peer_snubbing
    }

    /// Return whether this peer is currently choking us.
    pub fn is_peer_choking(&self) -> bool {
        self.peer_choking
    }

    /// Get the current max outstanding request limit.
    pub fn max_outstanding_requests(&self) -> usize {
        self.max_outstanding_requests
    }

    /// Dynamically adjust the max outstanding request count.
    ///
    /// Mirrors C++ auto-scaling logic in `DefaultBtInteractive::receiveMessages()`:
    /// If many outstanding requests were satisfied recently, double the limit
    /// (up to `UB_MAX_OUTSTANDING_REQUEST`).
    pub fn scale_max_outstanding_requests(&mut self, old_outstanding: usize) {
        let current_outstanding = self.count_outstanding_requests();
        if old_outstanding > current_outstanding {
            let satisfied = old_outstanding - current_outstanding;
            // If >= 25% of max outstanding were satisfied, double the limit
            if satisfied * 4 >= self.max_outstanding_requests {
                let new_max = (self.max_outstanding_requests * 2)
                    .min(UB_MAX_OUTSTANDING_REQUEST);
                if new_max != self.max_outstanding_requests {
                    debug!(
                        "PeerHandler: scaling max outstanding from {} to {} ({} satisfied)",
                        self.max_outstanding_requests, new_max, satisfied
                    );
                    self.max_outstanding_requests = new_max;
                }
            }
        }
    }

    // ── Message Queue Operations ─────────────────────────────────────────

    /// Drain messages that are ready to be sent from the dispatcher queue.
    ///
    /// Invalidated messages are skipped; upload messages are deferred if
    /// speed limits are active.
    pub fn drain_sendable_messages(&mut self) -> Vec<Vec<u8>> {
        self.dispatcher.drain_sendable_messages()
    }

    /// Add a control message (non-upload) to the outgoing queue.
    pub fn queue_control_message(&mut self, data: Vec<u8>) {
        self.dispatcher.add_control_message(data);
    }

    /// Add a Piece upload message to the outgoing queue.
    pub fn queue_upload_message(&mut self, data: Vec<u8>, index: u32, begin: u32, length: u32) {
        self.dispatcher.add_upload_message(data, index, begin, length);
    }

    /// Check if there are pending messages in the queue.
    pub fn has_pending_messages(&self) -> bool {
        self.dispatcher.has_pending_messages()
    }

    /// Return the count of pending messages in the queue.
    pub fn count_pending_messages(&self) -> usize {
        self.dispatcher.count_messages()
    }

    // ── Upload Speed Limiting ────────────────────────────────────────────

    /// Set whether the global upload speed limit is exceeded.
    pub fn set_upload_speed_exceeded(&mut self, exceeded: bool) {
        self.dispatcher.set_upload_speed_exceeded(exceeded);
    }

    /// Set whether the per-group upload speed limit is exceeded.
    pub fn set_group_upload_speed_exceeded(&mut self, exceeded: bool) {
        self.dispatcher.set_group_upload_speed_exceeded(exceeded);
    }

    /// Check if upload speed limiting is active.
    pub fn is_upload_limited(&self) -> bool {
        self.dispatcher.is_upload_limited()
    }

    // ── Cleanup ──────────────────────────────────────────────────────────

    /// Clear all messages and request slots (e.g., on peer disconnect).
    pub fn clear(&mut self) {
        self.dispatcher.clear();
        self.flooding_stat.reset();
        self.peer_snubbing = false;
        self.peer_interested = false;
        self.am_choking = true;
        self.peer_allowed_fast_set.clear();
    }

    /// Get a reference to the underlying dispatcher for advanced operations.
    pub fn dispatcher(&self) -> &BtMessageDispatcher {
        &self.dispatcher
    }

    /// Get a mutable reference to the underlying dispatcher.
    pub fn dispatcher_mut(&mut self) -> &mut BtMessageDispatcher {
        &mut self.dispatcher
    }
}

/// Result of a block download attempt
pub struct BlockDownloadResult {
    /// Whether the block was successfully received
    pub success: bool,
    /// The received data (if successful)
    pub data: Option<Vec<u8>>,
    /// Number of bytes received (for statistics)
    pub bytes_received: u64,
}

/// BT Message Handler for block-level operations (legacy, stateless).
///
/// Manages the process of requesting and receiving individual blocks
/// from peers during piece download.
///
/// # Deprecation Note
///
/// This struct provides only static methods with no per-peer state.
/// For new code, prefer [`BtPeerMessageHandler`] which integrates with
/// [`BtMessageDispatcher`] for request slot tracking, event-driven
/// actions, flooding detection, and timeout checking.
#[allow(dead_code)]
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
            tokio::time::sleep(std::time::Duration::from_millis(
                constants::BT_RETRY_DELAY_MS,
            ))
            .await;
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

    // ── Legacy BtMessageHandler tests (preserved) ───────────────────────

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

    // ── BtPeerMessageHandler new field initialization tests ────────────

    #[test]
    fn test_new_handler_initial_state() {
        let h = BtPeerMessageHandler::new(16384);
        assert!(h.peer_choking);
        assert!(!h.peer_snubbing);
        assert!(!h.peer_interested);
        assert!(h.am_choking);
        assert!(!h.fast_extension_enabled);
        assert!(!h.metadata_get_mode);
        assert!(h.peer_allowed_fast_set.is_empty());
    }

    #[test]
    fn test_with_max_outstanding_initial_state() {
        let h = BtPeerMessageHandler::with_max_outstanding(16384, 10);
        assert_eq!(h.max_outstanding_requests(), 10);
        assert!(h.peer_choking);
        assert!(!h.peer_interested);
        assert!(h.am_choking);
        assert!(!h.fast_extension_enabled);
        assert!(!h.metadata_get_mode);
    }

    // ── on_have_received tests ─────────────────────────────────────────

    #[test]
    fn test_on_have_received_updates_bitfield() {
        let mut h = BtPeerMessageHandler::new(16384);
        let updates = h.on_have_received(42, false, false);
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            &updates[0],
            PeerStateUpdate::HavePiece { index } if *index == 42
        ));
    }

    #[test]
    fn test_on_have_received_seeder_download_finished_disconnect() {
        let mut h = BtPeerMessageHandler::new(16384);
        let updates = h.on_have_received(7, true, true);
        assert_eq!(updates.len(), 2);
        assert!(matches!(&updates[0], PeerStateUpdate::HavePiece { index } if *index == 7));
        assert!(matches!(&updates[1], PeerStateUpdate::DisconnectSeeder));
    }

    #[test]
    fn test_on_have_received_seeder_download_not_finished_no_disconnect() {
        let mut h = BtPeerMessageHandler::new(16384);
        let updates = h.on_have_received(7, true, false);
        assert_eq!(updates.len(), 1);
        assert!(matches!(&updates[0], PeerStateUpdate::HavePiece { .. }));
    }

    // ── on_bitfield_received tests ─────────────────────────────────────

    #[test]
    fn test_on_bitfield_received_sets_bitfield() {
        let mut h = BtPeerMessageHandler::new(16384);
        let bf = vec![0xFF, 0x00];
        let updates = h.on_bitfield_received(bf.clone(), false, false);
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            &updates[0],
            PeerStateUpdate::SetBitfield { data } if data == &bf
        ));
    }

    #[test]
    fn test_on_bitfield_received_seeder_download_finished_disconnect() {
        let mut h = BtPeerMessageHandler::new(16384);
        let updates = h.on_bitfield_received(vec![0xFF], true, true);
        assert_eq!(updates.len(), 2);
        assert!(matches!(&updates[0], PeerStateUpdate::SetBitfield { .. }));
        assert!(matches!(&updates[1], PeerStateUpdate::DisconnectSeeder));
    }

    // ── on_interested_received tests ───────────────────────────────────

    #[test]
    fn test_on_interested_received_sets_flag() {
        let mut h = BtPeerMessageHandler::new(16384);
        assert!(!h.is_peer_interested());
        // am_choking is true by default, so ExecuteChoke should be triggered
        let updates = h.on_interested_received();
        assert!(h.is_peer_interested());
        assert_eq!(updates.len(), 1);
        assert!(matches!(&updates[0], PeerStateUpdate::ExecuteChoke));
    }

    #[test]
    fn test_on_interested_received_no_execute_choke_when_not_choking() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.set_am_choking(false);
        let updates = h.on_interested_received();
        assert!(h.is_peer_interested());
        assert!(updates.is_empty());
    }

    // ── on_not_interested_received tests ───────────────────────────────

    #[test]
    fn test_on_not_interested_received_clears_flag() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.peer_interested = true;
        // am_choking is true by default, so no ExecuteChoke
        let updates = h.on_not_interested_received();
        assert!(!h.is_peer_interested());
        assert!(updates.is_empty());
    }

    #[test]
    fn test_on_not_interested_received_triggers_execute_choke_when_not_choking() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.peer_interested = true;
        h.set_am_choking(false);
        let updates = h.on_not_interested_received();
        assert!(!h.is_peer_interested());
        assert_eq!(updates.len(), 1);
        assert!(matches!(&updates[0], PeerStateUpdate::ExecuteChoke));
    }

    // ── on_request_received tests ──────────────────────────────────────

    #[test]
    fn test_on_request_received_piece_when_has_piece_and_not_choking() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.set_am_choking(false);
        let resp = h.on_request_received(5, 0, 16384, |_| true, |_| false);
        assert_eq!(
            resp,
            RequestResponse::Piece {
                index: 5,
                begin: 0,
                length: 16384
            }
        );
    }

    #[test]
    fn test_on_request_received_piece_when_allowed_fast() {
        let h = BtPeerMessageHandler::new(16384);
        // am_choking is true, but piece is in am-allowed-fast set
        let resp = h.on_request_received(5, 0, 16384, |_| true, |idx| idx == 5);
        assert_eq!(
            resp,
            RequestResponse::Piece {
                index: 5,
                begin: 0,
                length: 16384
            }
        );
    }

    #[test]
    fn test_on_request_received_reject_when_choking_and_fast_extension() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.set_fast_extension_enabled(true);
        // am_choking is true by default, not in allowed-fast
        let resp = h.on_request_received(5, 0, 16384, |_| true, |_| false);
        assert_eq!(
            resp,
            RequestResponse::Reject {
                index: 5,
                begin: 0,
                length: 16384
            }
        );
    }

    #[test]
    fn test_on_request_received_none_when_choking_no_fast_extension() {
        let h = BtPeerMessageHandler::new(16384);
        // am_choking is true, fast_extension is false
        let resp = h.on_request_received(5, 0, 16384, |_| true, |_| false);
        assert_eq!(resp, RequestResponse::None);
    }

    #[test]
    fn test_on_request_received_reject_when_no_piece_and_fast_ext() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.set_am_choking(false);
        h.set_fast_extension_enabled(true);
        let resp = h.on_request_received(5, 0, 16384, |_| false, |_| false);
        assert_eq!(
            resp,
            RequestResponse::Reject {
                index: 5,
                begin: 0,
                length: 16384
            }
        );
    }

    // ── on_reject_received tests ───────────────────────────────────────

    #[test]
    fn test_on_reject_received_removes_slot() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.set_fast_extension_enabled(true);
        h.dispatcher.add_request_slot(5, 0, 16384);
        assert_eq!(h.count_outstanding_requests(), 1);

        let result = h.on_reject_received(5, 0, 16384);
        assert!(result.is_ok());
        assert_eq!(h.count_outstanding_requests(), 0);
    }

    #[test]
    fn test_on_reject_received_errors_without_fast_extension() {
        let mut h = BtPeerMessageHandler::new(16384);
        // fast_extension is false by default
        let result = h.on_reject_received(5, 0, 16384);
        assert!(result.is_err());
    }

    #[test]
    fn test_on_reject_received_no_matching_slot_is_ok() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.set_fast_extension_enabled(true);
        let result = h.on_reject_received(99, 0, 16384);
        assert!(result.is_ok());
    }

    // ── on_allowed_fast_received tests ─────────────────────────────────

    #[test]
    fn test_on_allowed_fast_received_adds_to_set() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.set_fast_extension_enabled(true);
        let result = h.on_allowed_fast_received(42);
        assert!(result.is_ok());
        assert!(h.is_in_peer_allowed_fast(42));
        assert!(!h.is_in_peer_allowed_fast(43));
    }

    #[test]
    fn test_on_allowed_fast_received_errors_without_fast_extension() {
        let mut h = BtPeerMessageHandler::new(16384);
        let result = h.on_allowed_fast_received(42);
        assert!(result.is_err());
        assert!(!h.is_in_peer_allowed_fast(42));
    }

    #[test]
    fn test_on_allowed_fast_received_duplicate_index() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.set_fast_extension_enabled(true);
        assert!(h.on_allowed_fast_received(42).is_ok());
        assert!(h.on_allowed_fast_received(42).is_ok()); // idempotent
        assert!(h.is_in_peer_allowed_fast(42));
        assert_eq!(h.peer_allowed_fast_set.len(), 1);
    }

    // ── on_have_all_received tests ─────────────────────────────────────

    #[test]
    fn test_on_have_all_received_marks_seeder() {
        let mut h = BtPeerMessageHandler::new(16384);
        let updates = h.on_have_all_received(false);
        assert_eq!(updates.len(), 1);
        assert!(matches!(&updates[0], PeerStateUpdate::MarkSeeder));
    }

    #[test]
    fn test_on_have_all_received_disconnect_when_download_finished() {
        let mut h = BtPeerMessageHandler::new(16384);
        let updates = h.on_have_all_received(true);
        assert_eq!(updates.len(), 2);
        assert!(matches!(&updates[0], PeerStateUpdate::MarkSeeder));
        assert!(matches!(&updates[1], PeerStateUpdate::DisconnectSeeder));
    }

    // ── on_have_none_received tests ────────────────────────────────────

    #[test]
    fn test_on_have_none_received_clears_bitfield() {
        let mut h = BtPeerMessageHandler::new(16384);
        let updates = h.on_have_none_received();
        assert_eq!(updates.len(), 1);
        assert!(matches!(&updates[0], PeerStateUpdate::ClearBitfield));
    }

    // ── on_suggest_received tests ──────────────────────────────────────

    #[test]
    fn test_on_suggest_received_noop() {
        let mut h = BtPeerMessageHandler::new(16384);
        let interested_before = h.peer_interested;
        let choking_before = h.am_choking;
        h.on_suggest_received(42);
        assert_eq!(h.peer_interested, interested_before);
        assert_eq!(h.am_choking, choking_before);
    }

    // ── on_port_received tests ─────────────────────────────────────────

    #[test]
    fn test_on_port_received_nonzero() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.on_port_received(6881);
        // No state change expected, just logs
    }

    #[test]
    fn test_on_port_received_zero() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.on_port_received(0);
        // No state change expected, just logs
    }

    // ── on_extended_received tests ─────────────────────────────────────

    #[test]
    fn test_on_extended_received_logs() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.on_extended_received(0, &[1, 2, 3]);
        // No state change expected, just logs
    }

    // ── Fast extension and choking state accessor tests ────────────────

    #[test]
    fn test_set_and_check_fast_extension() {
        let mut h = BtPeerMessageHandler::new(16384);
        assert!(!h.is_fast_extension_enabled());
        h.set_fast_extension_enabled(true);
        assert!(h.is_fast_extension_enabled());
        h.set_fast_extension_enabled(false);
        assert!(!h.is_fast_extension_enabled());
    }

    #[test]
    fn test_set_and_check_am_choking() {
        let mut h = BtPeerMessageHandler::new(16384);
        assert!(h.is_am_choking());
        h.set_am_choking(false);
        assert!(!h.is_am_choking());
        h.set_am_choking(true);
        assert!(h.is_am_choking());
    }

    #[test]
    fn test_metadata_get_mode() {
        let mut h = BtPeerMessageHandler::new(16384);
        assert!(!h.is_metadata_get_mode());
        h.set_metadata_get_mode(true);
        assert!(h.is_metadata_get_mode());
        h.set_metadata_get_mode(false);
        assert!(!h.is_metadata_get_mode());
    }

    // ── clear() resets new fields ──────────────────────────────────────

    #[test]
    fn test_clear_resets_all_state() {
        let mut h = BtPeerMessageHandler::new(16384);
        h.peer_interested = true;
        h.set_am_choking(false);
        h.set_fast_extension_enabled(true);
        h.on_allowed_fast_received(42).ok();
        h.dispatcher.add_request_slot(5, 0, 16384);

        h.clear();

        assert!(!h.peer_interested);
        assert!(h.am_choking);
        assert!(h.peer_allowed_fast_set.is_empty());
        assert_eq!(h.count_outstanding_requests(), 0);
        // fast_extension_enabled and metadata_get_mode are NOT reset by clear()
        // (they are negotiated at handshake and remain for the connection lifetime)
    }

    // ── RequestResponse enum tests ─────────────────────────────────────

    #[test]
    fn test_request_response_equality() {
        let a = RequestResponse::Piece {
            index: 5,
            begin: 0,
            length: 16384,
        };
        let b = RequestResponse::Piece {
            index: 5,
            begin: 0,
            length: 16384,
        };
        let c = RequestResponse::Reject {
            index: 5,
            begin: 0,
            length: 16384,
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, RequestResponse::None);
    }

    // ── PeerStateUpdate variant tests ──────────────────────────────────

    #[test]
    fn test_peer_state_update_debug_format() {
        let u = PeerStateUpdate::HavePiece { index: 42 };
        assert!(format!("{:?}", u).contains("42"));
        let u = PeerStateUpdate::SetBitfield {
            data: vec![0xFF],
        };
        assert!(format!("{:?}", u).contains("255"));
        let u = PeerStateUpdate::MarkSeeder;
        assert!(format!("{:?}", u).contains("MarkSeeder"));
        let u = PeerStateUpdate::ClearBitfield;
        assert!(format!("{:?}", u).contains("ClearBitfield"));
        let u = PeerStateUpdate::ExecuteChoke;
        assert!(format!("{:?}", u).contains("ExecuteChoke"));
        let u = PeerStateUpdate::DisconnectSeeder;
        assert!(format!("{:?}", u).contains("DisconnectSeeder"));
    }

    // ── Integration: on_interested + on_not_interested cycle ───────────

    #[test]
    fn test_interested_not_interested_cycle() {
        let mut h = BtPeerMessageHandler::new(16384);
        // Start: peer_interested=false, am_choking=true
        let updates = h.on_interested_received();
        assert!(h.is_peer_interested());
        assert_eq!(updates.len(), 1); // ExecuteChoke

        h.set_am_choking(false);
        let updates = h.on_not_interested_received();
        assert!(!h.is_peer_interested());
        assert_eq!(updates.len(), 1); // ExecuteChoke

        // Now am_choking=true again, NotInterested should NOT trigger ExecuteChoke
        h.set_am_choking(true);
        h.peer_interested = true; // reset
        let updates = h.on_not_interested_received();
        assert!(!h.is_peer_interested());
        assert!(updates.is_empty());
    }
}

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

use std::collections::HashSet;

use crate::engine::bt_message_dispatcher::{
    BtMessageDispatcher, FloodingStat, RequestSlot, SlotCheckResult,
};
use tracing::{debug, trace, warn};

use super::types::{
    DEFAULT_MAX_OUTSTANDING_REQUEST, UB_MAX_OUTSTANDING_REQUEST, PeerStateUpdate, RequestResponse,
    BLOCK_SIZE,
};

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

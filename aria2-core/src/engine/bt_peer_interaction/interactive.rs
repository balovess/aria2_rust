//! BtPeerInteractive — per-peer interaction loop (C++ DefaultBtInteractive)
//!
//! This module contains the `BtPeerInteractive` struct and its implementation,
//! which manages the per-peer interaction processing loop in the BitTorrent
//! protocol.

use std::time::{Duration, Instant};

use aria2_protocol::bittorrent::message::types::BtMessage;

use crate::constants;
use crate::engine::bt_message_dispatcher::{
    ActiveInteractionChecker, FloodingStat, InactiveReason,
};
use crate::engine::bt_message_handler::BtPeerMessageHandler;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_request_factory::{BtRequestFactory, PieceBlockRequest};
use crate::engine::extension_registry::{self, ExtensionRegistry, ExtensionUpdate};
use crate::error::Result;
use tracing::{debug, trace, warn};

use super::piece_provider::PieceProvider;
use super::types::*;

/// Per-peer interaction manager that runs the processing loop each tick.
///
/// Mirrors C++ `DefaultBtInteractive`. Each active peer connection has one
/// instance. The main entry point is [`do_interaction_processing()`],
/// which is called once per command execution cycle in the `Wired` state.
///
/// # C++ `doInteractionProcessing()` flow
///
/// ```text
/// checkActiveInteraction()           — 30s mutual-uninterested, 60s total, seeder-seeder
/// if perSecTimer >= 1s:
///     checkRequestSlotAndDoNecessaryThing()  — timeout + already-acquired
/// receiveMessages()
/// detectMessageFlooding()           — >=2 choke/unchoke or >=2 keepalive in 5s
/// decideChoking()                   — should we choke/unchoke?
/// decideInterest()                  — do we have missing pieces from this peer?
/// checkHave()                       — advertise newly completed pieces
/// sendKeepAlive()                   — every 120s
/// removeCompletedPiece()
/// if !downloadFinished:
///     addRequests()                 — fill piece requests
/// addPeerExchangeMessage()
/// sendPendingMessage()
/// ```
pub struct BtPeerInteractive {
    // ── Connection state ───────────────────────────────────────────────
    /// Current lifecycle state of this peer connection.
    state: PeerConnectionState,

    // ── Message handler (C++ DefaultBtMessageDispatcher per peer) ──────
    /// Per-peer message handler with request slot tracking and flooding.
    pub(crate) handler: BtPeerMessageHandler,

    // ── Peer state tracking (C++ Peer fields) ─────────────────────────
    /// Whether we are currently choking this peer (C++ `amChoking_`).
    pub(crate) am_choking: bool,
    /// Whether we are currently interested in this peer (C++ `amInterested_`).
    pub(crate) am_interested: bool,
    /// Whether the peer is currently choking us (C++ `peerChoking_`).
    pub(crate) peer_choking: bool,
    /// Whether the peer is currently interested in us (C++ `peerInterested_`).
    pub(crate) peer_interested: bool,

    // ── Timers (matching C++) ──────────────────────────────────────────
    /// Timer for keep-alive sending (C++ `keepAliveTimer_`).
    pub(crate) keep_alive_timer: Instant,
    /// Timer for flooding check interval (C++ `floodingTimer_`).
    pub(crate) flooding_timer: Instant,
    /// Timer for inactive peer detection (C++ `inactiveTimer_`).
    inactive_timer: Instant,
    /// Per-second timer for request slot checking (C++ `perSecTimer_`).
    per_sec_timer: Instant,
    /// Timer for peer exchange messages (C++ `pexTimer_`).
    pex_timer: Instant,

    // ── Configuration ──────────────────────────────────────────────────
    /// Keep-alive interval in seconds (C++ `keepAliveInterval_`, default 120).
    pub(crate) keep_alive_interval_secs: u64,
    /// Maximum outstanding piece requests (C++ `maxOutstandingRequest_`, default 6).
    pub(crate) max_outstanding_request: usize,
    /// Allowed-fast set size (C++ `allowedFastSetSize_`, default 10).
    pub(crate) allowed_fast_set_size: usize,

    // ── Flooding detection ─────────────────────────────────────────────
    /// Flooding statistics tracker.
    pub(crate) flooding_stat: FloodingStat,

    // ── Active interaction checking ────────────────────────────────────
    /// Inactive peer checker.
    active_interaction_checker: ActiveInteractionChecker,

    // ── Tracking ───────────────────────────────────────────────────────
    /// Last have index we have advertised to the peer (C++ `lastHaveIndex_`).
    last_have_index: u64,
    /// Number of messages received in the current iteration (C++ `numReceivedMessage_`).
    num_received_message: usize,
    /// Total number of pieces in the torrent.
    #[allow(dead_code)]
    num_pieces: u32,
    /// 20-byte info hash for this torrent.
    info_hash: [u8; 20],

    // ── Feature flags ──────────────────────────────────────────────────
    /// Whether UT PEX (peer exchange) is enabled (C++ `utPexEnabled_`).
    pub(crate) ut_pex_enabled: bool,
    /// Whether DHT is enabled (C++ `dhtEnabled_`).
    pub(crate) dht_enabled: bool,
    /// Whether we are in metadata-get mode (C++ `metadataGetMode_`).
    metadata_get_mode: bool,
    /// Whether the download is finished (affects addRequests step).
    pub(crate) download_finished: bool,

    // ── Extension Protocol (BEP 10) ──────────────────────────────────────
    /// Per-peer extension registry tracking local and peer ext_id assignments.
    extension_registry: ExtensionRegistry,

    // ── Request generation (C++ DefaultBtRequestFactory) ──────────────────
    /// Per-peer request factory managing target pieces and generating Request messages.
    /// Mirrors C++ `btRequestFactory_` in `DefaultBtInteractive`.
    request_factory: BtRequestFactory,

    /// Whether end-game mode has been entered for this download.
    /// Mirrors C++ `endGame_` in `DefaultBtInteractive`.
    pub(crate) endgame: bool,
}

impl BtPeerInteractive {
    /// Create a new `BtPeerInteractive` for a peer connection.
    ///
    /// # Arguments
    /// * `info_hash` — 20-byte torrent info hash
    /// * `num_pieces` — Total number of pieces in the torrent
    ///
    /// All timers are initialized to `Instant::now()`, matching the C++
    /// constructor which sets all timers to `global::wallclock()`.
    pub fn new(info_hash: [u8; 20], num_pieces: u32) -> Self {
        let now = Instant::now();
        Self {
            state: PeerConnectionState::InitiatorSendHandshake,
            handler: BtPeerMessageHandler::new(constants::BT_BLOCK_SIZE as u32),
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            keep_alive_timer: now,
            flooding_timer: now,
            inactive_timer: now,
            per_sec_timer: now,
            pex_timer: now,
            keep_alive_interval_secs: DEFAULT_KEEP_ALIVE_INTERVAL_SECS,
            max_outstanding_request: DEFAULT_MAX_OUTSTANDING_REQUEST,
            allowed_fast_set_size: DEFAULT_ALLOWED_FAST_SET_SIZE,
            flooding_stat: FloodingStat::new(),
            active_interaction_checker: ActiveInteractionChecker::new(),
            last_have_index: 0,
            num_received_message: 0,
            num_pieces,
            info_hash,
            ut_pex_enabled: false,
            dht_enabled: false,
            metadata_get_mode: false,
            download_finished: false,
            extension_registry: ExtensionRegistry::new(),
            request_factory: BtRequestFactory::new(constants::BT_BLOCK_SIZE as u32),
            endgame: false,
        }
    }

    /// Create with a specific initial state (e.g., `ReceiverWaitHandshake`
    /// for incoming connections).
    pub fn with_state(info_hash: [u8; 20], num_pieces: u32, state: PeerConnectionState) -> Self {
        let mut interactive = Self::new(info_hash, num_pieces);
        interactive.state = state;
        interactive
    }

    // ── Configuration setters ──────────────────────────────────────────

    /// Set the keep-alive interval in seconds.
    /// Matches C++ `setKeepAliveInterval()`.
    pub fn set_keep_alive_interval(&mut self, secs: u64) {
        self.keep_alive_interval_secs = secs;
    }

    /// Set the maximum outstanding request count.
    pub fn set_max_outstanding_request(&mut self, max: usize) {
        self.max_outstanding_request = max.max(1).min(UB_MAX_OUTSTANDING_REQUEST);
    }

    /// Set the allowed-fast set size.
    pub fn set_allowed_fast_set_size(&mut self, size: usize) {
        self.allowed_fast_set_size = size;
    }

    /// Enable or disable UT PEX (peer exchange).
    /// Matches C++ `setUTPexEnabled()`.
    pub fn set_ut_pex_enabled(&mut self, enabled: bool) {
        self.ut_pex_enabled = enabled;
    }

    /// Enable or disable DHT.
    /// Matches C++ `setDHTEnabled()`.
    pub fn set_dht_enabled(&mut self, enabled: bool) {
        self.dht_enabled = enabled;
    }

    /// Enable metadata-get mode.
    /// Matches C++ `enableMetadataGetMode()`.
    pub fn enable_metadata_get_mode(&mut self) {
        self.metadata_get_mode = true;
    }

    /// Set whether the download is finished (affects addRequests step).
    pub fn set_download_finished(&mut self, finished: bool) {
        self.download_finished = finished;
    }

    // ── State accessors ────────────────────────────────────────────────

    /// Get the current connection lifecycle state.
    pub fn state(&self) -> PeerConnectionState {
        self.state
    }

    /// Get the number of messages received in the last iteration.
    /// Matches C++ `countReceivedMessageInIteration()`.
    pub fn count_received_message_in_iteration(&self) -> usize {
        self.num_received_message
    }

    /// Get the current max outstanding request count.
    pub fn max_outstanding_request(&self) -> usize {
        self.max_outstanding_request
    }

    /// Get the info hash for this connection.
    pub fn info_hash(&self) -> &[u8; 20] {
        &self.info_hash
    }

    /// Check if metadata-get mode is enabled.
    pub fn is_metadata_get_mode(&self) -> bool {
        self.metadata_get_mode
    }

    /// Get whether we are currently choking this peer.
    /// Matches C++ `Peer::amChoking()`.
    pub fn am_choking(&self) -> bool {
        self.am_choking
    }

    /// Get whether we are currently interested in this peer.
    /// Matches C++ `Peer::amInterested()`.
    pub fn am_interested(&self) -> bool {
        self.am_interested
    }

    /// Get whether the peer is currently choking us.
    /// Matches C++ `Peer::peerChoking()`.
    pub fn peer_choking(&self) -> bool {
        self.peer_choking
    }

    /// Get whether the peer is currently interested in us.
    /// Matches C++ `Peer::peerInterested()`.
    pub fn peer_interested(&self) -> bool {
        self.peer_interested
    }

    /// Get a reference to the per-peer message handler.
    pub fn handler(&self) -> &BtPeerMessageHandler {
        &self.handler
    }

    /// Get a mutable reference to the per-peer message handler.
    pub fn handler_mut(&mut self) -> &mut BtPeerMessageHandler {
        &mut self.handler
    }

    /// Get a reference to the per-peer extension registry.
    pub fn extension_registry(&self) -> &ExtensionRegistry {
        &self.extension_registry
    }

    /// Get a mutable reference to the per-peer extension registry.
    pub fn extension_registry_mut(&mut self) -> &mut ExtensionRegistry {
        &mut self.extension_registry
    }

    // ── State machine transitions ──────────────────────────────────────

    /// Advance the state machine to `Wired` after a successful handshake.
    ///
    /// Resets all interaction timers, matching C++
    /// `doPostHandshakeProcessing()` which sets:
    /// - `keepAliveTimer_ = global::wallclock()`
    /// - `floodingTimer_ = global::wallclock()`
    /// - `pexTimer_ = Timer::zero()` (effectively "immediate")
    ///
    /// # Panics
    /// Panics if the current state is already `Wired` (invalid transition).
    pub fn advance_to_wired(&mut self) {
        debug!(
            state = %self.state,
            "BtPeerInteractive: advancing to WIRED state"
        );
        assert!(
            !self.state.is_wired(),
            "Cannot advance to WIRED from WIRED state"
        );
        let now = Instant::now();
        self.state = PeerConnectionState::Wired;
        self.keep_alive_timer = now;
        self.flooding_timer = now;
        self.inactive_timer = now;
        self.per_sec_timer = now;
        // PEX timer set to "far past" so the first PEX message is sent
        // immediately when the interval is checked. Use checked_sub to
        // avoid panic on platforms where Instant origin is near zero.
        self.pex_timer = now.checked_sub(Duration::from_secs(3600)).unwrap_or(now);
    }

    /// Transition from `InitiatorSendHandshake` to `InitiatorWaitHandshake`.
    ///
    /// # Panics
    /// Panics if the current state is not `InitiatorSendHandshake`.
    pub fn advance_to_wait_handshake(&mut self) {
        debug!(
            state = %self.state,
            "BtPeerInteractive: handshake sent, waiting for response"
        );
        assert_eq!(
            self.state,
            PeerConnectionState::InitiatorSendHandshake,
            "Can only advance to WAIT_HANDSHAKE from SEND_HANDSHAKE"
        );
        self.state = PeerConnectionState::InitiatorWaitHandshake;
    }

    // ── Post-handshake processing ──────────────────────────────────────

    /// Perform post-handshake processing.
    ///
    /// Mirrors C++ `doPostHandshakeProcessing()`. Called after the
    /// handshake completes and before the normal interaction loop starts.
    /// This sends:
    /// - Extension handshake (BEP 10) if both sides support it
    /// - Bitfield message with our current piece possession
    /// - Allowed-fast set messages (BEP 6) if fast extension is enabled
    /// - Port message (BEP 5) if DHT is enabled
    ///
    /// For now this is a stub — the actual message sending is done by
    /// the caller using the connection. This method returns a summary
    /// of what should be sent so the caller can decide.
    ///
    /// # Returns
    ///
    /// A [`PostHandshakeActions`] describing what messages should be sent.
    pub fn post_handshake_processing(&self) -> PostHandshakeActions {
        PostHandshakeActions {
            send_bitfield: true,
            // Send extension handshake if we have local extensions configured
            send_extension_handshake: true,
            send_dht_port: self.dht_enabled,
            allowed_fast_pieces: Vec::new(), // TODO: compute allowed-fast set
        }
    }

    // ── Main interaction loop ──────────────────────────────────────────

    /// Main interaction processing loop, matching C++
    /// `DefaultBtInteractive::doInteractionProcessing()`.
    ///
    /// This is the core per-tick method called each time the peer
    /// interaction command executes in the `Wired` state.
    ///
    /// # Flow (normal mode — all 12 C++ steps)
    ///
    /// 1. `check_active_interaction()` — disconnect idle peers
    /// 2. Per-second: check request slots for timeouts
    /// 3. Receive messages and dispatch to handlers
    /// 4. `detect_flooding()` — detect choke/keepalive flooding
    /// 5. `decide_choking()` — send choke/unchoke if needed
    /// 6. `decide_interest()` — send interested/not-interested if needed
    /// 7. `check_have()` — advertise newly completed pieces
    /// 8. `should_send_keepalive()` — send keepalive if interval elapsed
    /// 9. `remove_completed_piece()` — handled by handler
    /// 10. `add_requests()` — request more pieces if not finished
    /// 11. PEX message if applicable
    /// 12. `send_pending_message()` — flush outgoing queue
    ///
    /// # Callbacks
    ///
    /// Several steps require access to piece storage or peer storage
    /// that this struct does not own. These are provided as closures:
    ///
    /// * `has_missing_piece` — returns true if the peer has pieces we need
    /// * `get_advertised_pieces` — returns newly completed piece indexes
    /// * `is_in_allowed_fast` — returns true if a piece is in the allowed-fast set
    /// * `is_block_acquired` — returns true if a block was obtained from another peer
    ///
    /// # Returns
    ///
    /// - `InteractionResult::Continue { pex_pending }` — normal tick, keep running; if pex_pending is true, caller should send PEX
    /// - `InteractionResult::Disconnect(reason)` — peer should be dropped
    /// - `InteractionResult::FloodingDetected` — flooding detected
    /// - `InteractionResult::WaitingForHandshake` — not yet wired
    pub async fn do_interaction_processing(
        &mut self,
        conn: &mut BtPeerConn,
        has_missing_piece: impl Fn(&BtPeerConn) -> bool,
        get_advertised_pieces: impl Fn() -> Vec<u32>,
        is_in_allowed_fast: impl Fn(u32) -> bool + Clone,
        is_block_acquired: impl Fn(u32, u32) -> bool,
        piece_storage: Option<&mut dyn PieceProvider>,
        cuid: u64,
    ) -> Result<InteractionResult> {
        // If not yet wired, skip interaction processing
        if self.state.is_handshake_state() {
            return Ok(InteractionResult::WaitingForHandshake);
        }

        if self.metadata_get_mode {
            // Simplified metadata-get mode: just keep-alive + receive
            if self.should_send_keepalive() {
                if let Err(e) = conn.send_keepalive().await {
                    warn!("Failed to send keepalive in metadata-get mode: {}", e);
                }
            }
            self.num_received_message =
                self.receive_messages(conn, is_in_allowed_fast.clone()).await?;
            return Ok(InteractionResult::Continue { pex_pending: false });
        }

        // ── Step 1: checkActiveInteraction ──────────────────────────────
        if let Some(reason) = self.check_active_interaction(conn) {
            conn.disconnected_gracefully = true;
            return Ok(InteractionResult::Disconnect(reason));
        }

        // ── Step 2: per-second request slot check ──────────────────────
        if self.per_sec_timer.elapsed() >= Duration::from_secs(PER_SEC_INTERVAL_SECS) {
            self.per_sec_timer = Instant::now();
            let result = self.handler.check_request_slots(is_block_acquired);
            if result.timed_out {
                warn!("Peer marked as snubbing (request slot timeout)");
            }
            // Send Cancel messages for blocks acquired elsewhere
            for (index, begin, length) in &result.cancelled_blocks {
                if let Err(e) = conn
                    .send_cancel(&aria2_protocol::bittorrent::message::types::PieceBlockRequest::new(
                        *index, *begin, *length,
                    ))
                    .await
                {
                    warn!("Failed to send Cancel for piece {}: {}", index, e);
                }
            }
            trace!("Per-second timer fired, request slot check done");
        }

        // ── Step 3: receiveMessages ─────────────────────────────────────
        self.num_received_message =
            self.receive_messages(conn, is_in_allowed_fast.clone()).await?;

        // ── Step 4: detectMessageFlooding ───────────────────────────────
        if self.detect_flooding() {
            warn!("Message flooding detected, disconnecting peer");
            return Ok(InteractionResult::FloodingDetected);
        }

        // ── Step 5: decideChoking ───────────────────────────────────────
        let choking_decision = self.decide_choking(conn);
        match choking_decision {
            ChokingDecision::Choke => {
                debug!("Choking peer");
                self.handler.on_choke_sent();
                if let Err(e) = conn.send_choke().await {
                    warn!("Failed to send choke: {}", e);
                }
                self.am_choking = true;
                conn.stats.record_choke();
            }
            ChokingDecision::Unchoke => {
                debug!("Unchoking peer");
                if let Err(e) = conn.send_unchoke().await {
                    warn!("Failed to send unchoke: {}", e);
                }
                self.am_choking = false;
                conn.stats.record_unchoke();
            }
            ChokingDecision::NoChange => {}
        }

        // ── Step 6: decideInterest ─────────────────────────────────────
        let interest_decision = self.decide_interest_with_callback(conn, &has_missing_piece);
        match interest_decision {
            InterestDecision::Interested => {
                debug!("Expressing interest in peer");
                if let Err(e) = conn.send_interested().await {
                    warn!("Failed to send interested: {}", e);
                }
                self.am_interested = true;
            }
            InterestDecision::NotInterested => {
                debug!("Expressing lack of interest in peer");
                if let Err(e) = conn.send_not_interested().await {
                    warn!("Failed to send not-interested: {}", e);
                }
                self.am_interested = false;
            }
            InterestDecision::NoChange => {}
        }

        // ── Step 7: checkHave ───────────────────────────────────────────
        // C++ checkHave(): query PieceStorage for newly completed pieces and
        // send Have messages. Optimization: if there are many new pieces,
        // send a single Bitfield message instead.
        //
        // NOTE: We borrow `piece_storage` immutably here and stash the
        // results before the mutable borrow in Step 10, to satisfy the
        // borrow checker.
        if let Some(ref ps) = piece_storage {
            let bitfield_length = ps.get_bitfield_length_ext();
            let fast_ext = conn.is_fast_extension_enabled();
            let all_done = ps.all_download_finished_ext();
            let completed_len = ps.get_completed_length_ext();

            let result = self.check_have_optimized(
                &|last_idx| ps.get_advertised_piece_indexes_ext(cuid, last_idx),
                bitfield_length,
                fast_ext,
                all_done,
                completed_len,
            );

            match result {
                CheckHaveResult::None => {}
                CheckHaveResult::HaveIndexes(indexes) => {
                    for index in indexes {
                        if let Err(e) = conn.send_have(index as u32).await {
                            warn!("Failed to send Have({}): {}", index, e);
                        }
                    }
                }
                CheckHaveResult::Bitfield => {
                    let bf = ps.get_bitfield_ext();
                    if let Err(e) = conn.send_bitfield(bf).await {
                        warn!("Failed to send Bitfield: {}", e);
                    }
                }
                CheckHaveResult::HaveAll => {
                    if let Err(e) = conn.send_have_all().await {
                        warn!("Failed to send HaveAll: {}", e);
                    }
                }
            }
        } else {
            // Legacy path without piece storage
            let have_indices = self.check_have_with_callback(&get_advertised_pieces);
            for index in have_indices {
                if let Err(e) = conn.send_have(index).await {
                    warn!("Failed to send Have({}): {}", index, e);
                }
            }
        }

        // ── Step 8: sendKeepAlive ───────────────────────────────────────
        if self.should_send_keepalive() {
            if let Err(e) = conn.send_keepalive().await {
                warn!("Failed to send keepalive: {}", e);
            }
            self.reset_keep_alive_timer();
        }

        // ── Step 9: removeCompletedPiece ────────────────────────────────
        // Remove target pieces that have been fully downloaded.
        // C++ calls: btRequestFactory_->removeCompletedPiece()
        let completed_indices = self.remove_completed_piece();
        if !completed_indices.is_empty() {
            trace!(
                "Removed {} completed target pieces: {:?}",
                completed_indices.len(),
                completed_indices
            );
        }

        // ── Step 10: addRequests ────────────────────────────────────────
        // Generate new piece requests if the download is not finished.
        // C++ calls: if(!pieceStorage_->downloadFinished()) { addRequests(); }
        if !self.download_finished {
            if let Some(ps) = piece_storage {
                let requests = self.add_requests(ps, conn, cuid);
                if !requests.is_empty() {
                    trace!("addRequests: generated {} new requests", requests.len());
                }
            } else {
                // No piece storage provided — legacy path: just log readiness
                if !self.peer_choking && self.handler.can_send_request() {
                    trace!(
                        "Ready to add requests (outstanding={})",
                        self.handler.count_outstanding_requests()
                    );
                }
            }
        }

        // ── Step 11: addPeerExchangeMessage ─────────────────────────────
        // In C++ aria2, DefaultBtInteractive::addPeerExchangeMessage() accesses
        // PeerStorage to get the list of connected/dropped peers and builds a
        // ut_pex Extended message. Here we signal that PEX is due so the caller
        // (which has access to the peer list) can build and inject the message.
        let mut pex_pending = false;
        if self.ut_pex_enabled
            && self.pex_timer.elapsed() >= Duration::from_secs(PEX_INTERVAL_SECS)
        {
            self.pex_timer = Instant::now();
            pex_pending = true;
            trace!("PEX timer fired, peer exchange message due");
        }

        // ── Step 12: sendPendingMessage ────────────────────────────────
        // Drain sendable messages from the handler's dispatcher queue
        // first, then flush the connection's send buffer.
        let pending = self.handler.drain_sendable_messages();
        for msg_bytes in pending {
            // Queue each pending message into the connection's send buffer.
            // The actual sending happens during flush_send_buffer().
            conn.queue_message(msg_bytes);
        }
        if let Err(e) = conn.flush_send_buffer().await {
            warn!("Failed to flush send buffer: {}", e);
        }

        Ok(InteractionResult::Continue { pex_pending })
    }

    // ── Individual processing steps ─────────────────────────────────────

    /// Check for inactive interaction and return a reason to disconnect.
    ///
    /// Mirrors C++ `checkActiveInteraction()`:
    /// - 30s mutual-uninterested → disconnect
    /// - 60s total inactivity → disconnect
    /// - seeder-to-seeder → disconnect
    ///
    /// Uses the tracked `am_interested` and `peer_interested` fields
    /// instead of heuristics.
    ///
    /// Returns `Some(InactiveReason)` if the peer should be dropped.
    pub(crate) fn check_active_interaction(&mut self, conn: &BtPeerConn) -> Option<InactiveReason> {
        // Use tracked interest state rather than heuristics.
        // For we_are_seeder, check the connection's session resource.
        let we_are_seeder = conn
            .session_resource
            .as_ref()
            .map_or(false, |res| res.is_seeder());
        let peer_is_seeder = conn.seeder;

        self.active_interaction_checker.check(
            self.am_interested,
            self.peer_interested,
            we_are_seeder,
            peer_is_seeder,
        )
    }

    /// Decide whether we should choke or unchoke the peer.
    ///
    /// Mirrors C++ `decideChoking()`:
    /// - If `shouldBeChoking()` is true and we are not choking → send Choke
    /// - If `shouldBeChoking()` is false and we are choking → send Unchoke
    ///
    /// Now properly tracks `am_choking` state to only produce a decision
    /// when the state actually needs to change.
    pub(crate) fn decide_choking(&self, conn: &BtPeerConn) -> ChokingDecision {
        if let Some(ref res) = conn.session_resource {
            let should_be_choking = res.should_be_choking();
            if should_be_choking && !self.am_choking {
                // Should be choking but currently not → send Choke
                ChokingDecision::Choke
            } else if !should_be_choking && self.am_choking {
                // Should not be choking but currently are → send Unchoke
                ChokingDecision::Unchoke
            } else {
                ChokingDecision::NoChange
            }
        } else {
            // No session resource — no choking decision possible
            ChokingDecision::NoChange
        }
    }

    /// Decide whether we should express interest or lack thereof.
    ///
    /// Mirrors C++ `decideInterest()`:
    /// - If `hasMissingPiece(peer)` and not amInterested → send Interested
    /// - If `!hasMissingPiece(peer)` and amInterested → send NotInterested
    ///
    /// Uses the provided `has_missing_piece` callback to check whether
    /// the peer has pieces we need (i.e., PieceStorage::hasMissingPiece).
    pub(crate) fn decide_interest_with_callback(
        &self,
        conn: &BtPeerConn,
        has_missing_piece: &impl Fn(&BtPeerConn) -> bool,
    ) -> InterestDecision {
        let should_be_interested = has_missing_piece(conn);
        if should_be_interested && !self.am_interested {
            InterestDecision::Interested
        } else if !should_be_interested && self.am_interested {
            InterestDecision::NotInterested
        } else {
            InterestDecision::NoChange
        }
    }

    /// Legacy decide_interest using heuristic (for backward compat).
    ///
    /// Prefer `decide_interest_with_callback` for proper PieceStorage integration.
    #[allow(dead_code)]
    pub(crate) fn decide_interest(&self, conn: &BtPeerConn) -> InterestDecision {
        // Heuristic: if peer is a seeder or has a session resource,
        // we are likely interested. This matches the original simplified
        // behavior before callback integration.
        let should_be_interested = conn.session_resource.is_some();
        if should_be_interested && !self.am_interested {
            InterestDecision::Interested
        } else if !should_be_interested && self.am_interested {
            InterestDecision::NotInterested
        } else {
            InterestDecision::NoChange
        }
    }

    /// Check for new Have messages to send.
    ///
    /// Mirrors C++ `checkHave()`: queries `PieceStorage` for piece indexes
    /// that have been completed since `lastHaveId_` and returns them.
    ///
    /// In the C++ code, this calls `pieceStorage_->getAdvertisedPieceIndexes()`.
    /// Without piece storage integration, this returns an empty vector.
    #[allow(dead_code)]
    pub(crate) fn check_have(&mut self) -> Vec<u32> {
        Vec::new()
    }

    /// Check for new Have messages using a callback for piece storage.
    ///
    /// Mirrors C++ `checkHave()`: calls the `get_advertised_pieces` callback
    /// which should return piece indexes completed since `lastHaveIndex_`.
    ///
    /// After sending these Have messages, `lastHaveIndex_` is updated.
    pub(crate) fn check_have_with_callback(&mut self, get_advertised_pieces: &impl Fn() -> Vec<u32>) -> Vec<u32> {
        let pieces = get_advertised_pieces();
        if !pieces.is_empty() {
            // Update last_have_index to the maximum advertised index
            if let Some(&max_idx) = pieces.iter().max() {
                self.last_have_index = self.last_have_index.max(max_idx as u64);
            }
            trace!("checkHave: advertising {} new pieces", pieces.len());
        }
        pieces
    }

    /// Check for new Have messages and decide whether to send individual
    /// Have messages or a single Bitfield/HaveAll/HaveNone message.
    ///
    /// Mirrors C++ `DefaultBtInteractive::checkHave()`:
    /// - If `5 + bitfieldLength <= haveIndexes.size() * 9`, send a single
    ///   Bitfield message (or HaveAll/HaveNone if fast extension is enabled)
    /// - Otherwise, send individual Have messages
    ///
    /// Returns a `CheckHaveResult` indicating what type of message(s) to send.
    pub(crate) fn check_have_optimized(
        &mut self,
        get_advertised_pieces: &impl Fn(u64) -> (Vec<usize>, u64),
        bitfield_length: usize,
        fast_extension_enabled: bool,
        all_download_finished: bool,
        completed_length: u64,
    ) -> CheckHaveResult {
        let (have_indexes, new_last) = get_advertised_pieces(self.last_have_index);
        self.last_have_index = new_last;

        if have_indexes.is_empty() {
            return CheckHaveResult::None;
        }

        // C++ optimization: use bitfield message if it is equal to or less
        // than the total size of have messages.
        // Have message = 5 bytes (4 length + 1 ID) + 4 bytes (piece index) = 9 bytes each
        // Bitfield message = 5 bytes (4 length + 1 ID) + bitfieldLength bytes
        if 5 + bitfield_length <= have_indexes.len() * 9 {
            if fast_extension_enabled && all_download_finished {
                return CheckHaveResult::HaveAll;
            }
            // Only send bitfield if we have some completed data
            if completed_length > 0 {
                return CheckHaveResult::Bitfield;
            }
        }

        CheckHaveResult::HaveIndexes(have_indexes)
    }

    /// Set the last advertised have index (called by the caller after
    /// checking piece storage).
    pub fn set_last_have_index(&mut self, index: u64) {
        self.last_have_index = index;
    }

    /// Get the last advertised have index.
    pub fn last_have_index(&self) -> u64 {
        self.last_have_index
    }

    /// Check whether we should send a keep-alive message.
    ///
    /// Mirrors C++ `sendKeepAlive()`: returns true if
    /// `keepAliveTimer_.difference() >= keepAliveInterval_`.
    pub fn should_send_keepalive(&self) -> bool {
        self.keep_alive_timer.elapsed() >= Duration::from_secs(self.keep_alive_interval_secs)
    }

    /// Reset the keep-alive timer after sending a keep-alive.
    pub fn reset_keep_alive_timer(&mut self) {
        self.keep_alive_timer = Instant::now();
    }

    /// Detect message flooding from the peer.
    ///
    /// Mirrors C++ `detectMessageFlooding()`: checks if the peer has
    /// sent >= 2 choke/unchoke transitions or >= 2 keepalive messages
    /// within the flooding check interval (5 seconds).
    ///
    /// The check interval is managed by this struct's `flooding_timer`,
    /// matching the C++ design where `DefaultBtInteractive` owns the timer
    /// and `FloodingStat` only holds the counts.
    ///
    /// Returns `true` if flooding was detected.
    pub(crate) fn detect_flooding(&mut self) -> bool {
        if self.flooding_timer.elapsed() >= Duration::from_secs(FLOODING_CHECK_INTERVAL_SECS) {
            let choke_count = self.flooding_stat.choke_unchoke_count();
            let keepalive_count = self.flooding_stat.keepalive_count();
            let detected = choke_count >= 2 || keepalive_count >= 2;

            if detected {
                warn!(
                    "Flooding detected: choke_unchoke={}, keepalive={}",
                    choke_count, keepalive_count
                );
            }

            // Reset counters regardless of detection result
            self.flooding_stat.reset();
            self.flooding_timer = Instant::now();
            detected
        } else {
            false
        }
    }

    // ── Message dispatch ────────────────────────────────────────────────

    /// Dispatch a received message to the appropriate handler method.
    ///
    /// This is the central message dispatch that the C++ code handles
    /// via virtual dispatch on `BtMessage::doReceivedAction()`. Each
    /// message type is routed to the corresponding `on_*_received()`
    /// method on the handler, and internal state (peer_choking,
    /// peer_interested, flooding stats) is updated.
    ///
    /// # Arguments
    /// * `msg` — The received BtMessage to dispatch
    /// * `conn` — The peer connection (for AllowedFast set access)
    /// * `is_in_allowed_fast` — Closure checking if a piece is in the
    ///   peer's allowed-fast set (needed for Choke handling)
    ///
    /// # Returns
    ///
    /// A [`DispatchUpdate`] containing state changes for the caller to apply.
    pub(crate) fn dispatch_message<F>(
        &mut self,
        msg: BtMessage,
        conn: &mut BtPeerConn,
        is_in_allowed_fast: F,
    ) -> DispatchUpdate
    where
        F: Fn(u32) -> bool,
    {
        let mut update = DispatchUpdate::default();

        match msg {
            BtMessage::Choke => {
                let was_choking = self.peer_choking;
                // Delegate to handler: removes non-allowed-fast request slots
                update.cancelled_slots = self.handler.on_choke_received(is_in_allowed_fast);
                self.peer_choking = true;
                update.peer_choking_changed = !was_choking;
                update.peer_choking = true;
                // Update flooding stat for transition detection
                if !was_choking {
                    self.flooding_stat.inc_choke_unchoke_count();
                }
                trace!("Dispatched Choke message");
            }
            BtMessage::Unchoke => {
                let was_choking = self.peer_choking;
                self.handler.on_unchoke_received();
                self.peer_choking = false;
                update.peer_choking_changed = was_choking;
                update.peer_choking = false;
                // Update flooding stat for transition detection
                if was_choking {
                    self.flooding_stat.inc_choke_unchoke_count();
                }
                trace!("Dispatched Unchoke message");
            }
            BtMessage::Interested => {
                self.peer_interested = true;
                trace!("Dispatched Interested message");
            }
            BtMessage::NotInterested => {
                self.peer_interested = false;
                trace!("Dispatched NotInterested message");
            }
            BtMessage::Have { piece_index } => {
                // Update the peer's bitfield
                if let Some(ref mut res) = conn.session_resource {
                    res.update_bitfield(piece_index as usize, 1);
                }
                // If the peer was a seeder before and now has even more,
                // or if the peer now has all pieces, mark as seeder
                if let Some(ref res) = conn.session_resource {
                    if res.is_seeder() {
                        conn.seeder = true;
                    }
                }
                update.have_index = Some(piece_index);
                trace!("Dispatched Have({}) message", piece_index);
            }
            BtMessage::Bitfield { data } => {
                // Update the peer's bitfield from the full bitfield message
                if let Some(ref mut res) = conn.session_resource {
                    res.set_bitfield(&data);
                    if res.is_seeder() {
                        conn.seeder = true;
                    }
                }
                update.bitfield_data = Some(data);
                trace!("Dispatched Bitfield message");
            }
            BtMessage::Request { request } => {
                // Incoming request from peer to upload data.
                // Record data exchange for active interaction checking.
                self.active_interaction_checker.record_data_exchange();
                trace!(
                    "Dispatched Request(piece={}, begin={}, len={})",
                    request.index, request.begin, request.length
                );
            }
            BtMessage::Piece {
                index,
                begin,
                ref data,
            } => {
                // Received piece data — remove matching request slot
                self.handler.on_piece_received(index, begin, data.len() as u32);
                // Record data exchange for active interaction checking
                self.active_interaction_checker.record_data_exchange();
                trace!(
                    "Dispatched Piece(index={}, begin={}, len={})",
                    index,
                    begin,
                    data.len()
                );
            }
            BtMessage::Cancel { request } => {
                // Peer cancels a pending upload
                self.handler
                    .on_cancel_received(request.index, request.begin, request.length);
                trace!(
                    "Dispatched Cancel(piece={}, begin={}, len={})",
                    request.index, request.begin, request.length
                );
            }
            BtMessage::KeepAlive => {
                self.handler.on_keepalive_received();
                self.flooding_stat.inc_keepalive_count();
                trace!("Dispatched KeepAlive message");
            }
            BtMessage::Port { port } => {
                // DHT port message (BEP 5)
                if self.dht_enabled {
                    trace!("Dispatched Port({}) message", port);
                }
            }
            BtMessage::AllowedFast { index } => {
                // BEP 6: peer grants fast access to a piece
                conn.add_allowed_fast(index);
                trace!("Dispatched AllowedFast({}) message", index);
            }
            BtMessage::Reject {
                index,
                offset,
                length,
            } => {
                // BEP 6: peer rejected our request
                // Remove the matching outstanding request slot
                self.handler.on_piece_received(index, offset, length);
                trace!(
                    "Dispatched Reject(piece={}, offset={}, len={})",
                    index, offset, length
                );
            }
            BtMessage::Suggest { index } => {
                // BEP 6: peer suggests we download this piece
                // The caller should boost the priority of this piece
                trace!("Dispatched Suggest({}) message", index);
            }
            BtMessage::HaveAll => {
                // BEP 6: peer has all pieces
                conn.mark_seeder();
                trace!("Dispatched HaveAll message");
            }
            BtMessage::HaveNone => {
                // BEP 6: peer has no pieces
                trace!("Dispatched HaveNone message");
            }
            BtMessage::Extended { ext_id, ref payload } => {
                // BEP 10: extension protocol message.
                // Dispatch via the extension registry which handles:
                //   ext_id == 0 → Extension Handshake (BEP 10)
                //   ext_id == peer_ut_metadata_id → ut_metadata (BEP 9)
                //   ext_id == peer_ut_pex_id → ut_pex (BEP 11)
                //   otherwise → unknown extension
                let ext_update = extension_registry::dispatch_extension_message(
                    &mut self.extension_registry,
                    ext_id,
                    payload,
                );

                if let Some(ref update) = ext_update {
                    match update {
                        ExtensionUpdate::HandshakeReceived { .. } => {
                            // Enable PEX if both sides support it
                            if self.extension_registry.supports_ut_pex() {
                                self.ut_pex_enabled = true;
                                debug!("ut_pex enabled after extension handshake");
                            }
                            debug!(
                                "Dispatched Extended handshake (ut_metadata={:?}, ut_pex={:?})",
                                self.extension_registry.peer_ut_metadata_id(),
                                self.extension_registry.peer_ut_pex_id()
                            );
                        }
                        ExtensionUpdate::MetadataPiece { piece, .. } => {
                            debug!("Dispatched Extended ut_metadata Data(piece={})", piece);
                        }
                        ExtensionUpdate::MetadataRequest { piece } => {
                            debug!("Dispatched Extended ut_metadata Request(piece={})", piece);
                        }
                        ExtensionUpdate::MetadataReject { piece } => {
                            debug!("Dispatched Extended ut_metadata Reject(piece={})", piece);
                        }
                        ExtensionUpdate::PeerExchange { added_v4, added_v6 } => {
                            debug!(
                                "Dispatched Extended ut_pex ({} v4, {} v6 peers)",
                                added_v4.len(),
                                added_v6.len()
                            );
                        }
                    }
                } else {
                    warn!(
                        "Dispatched Extended with unknown ext_id={} (payload_len={})",
                        ext_id,
                        payload.len()
                    );
                }

                update.extension_update = ext_update;
            }
        }

        update
    }

    /// Receive messages from the peer connection and dispatch each one.
    ///
    /// Mirrors C++ `receiveMessages()`: reads all available messages
    /// from the peer, dispatches each to the handler via
    /// [`dispatch_message()`], and resets the inactive timer on data
    /// messages.
    ///
    /// Returns the number of messages received.
    async fn receive_messages<F>(
        &mut self,
        conn: &mut BtPeerConn,
        is_in_allowed_fast: F,
    ) -> Result<usize>
    where
        F: Fn(u32) -> bool,
    {
        let mut count = 0usize;

        // Read up to a reasonable batch of messages per iteration.
        // The C++ code reads in a loop while messages are available.
        for _ in 0..UB_MAX_OUTSTANDING_REQUEST {
            match conn.read_message().await {
                Ok(Some(msg)) => {
                    count += 1;
                    trace!("Received message from peer: {:?}", msg);

                    // Dispatch the message to the handler
                    let update = self.dispatch_message(msg, conn, &is_in_allowed_fast);

                    // Process dispatch updates: send Cancel for removed slots
                    for slot in &update.cancelled_slots {
                        if let Err(e) = conn
                            .send_cancel(
                                &aria2_protocol::bittorrent::message::types::PieceBlockRequest::new(
                                    slot.index, slot.begin, slot.length,
                                ),
                            )
                            .await
                        {
                            warn!(
                                "Failed to send Cancel for piece {} begin {}: {}",
                                slot.index, slot.begin, e
                            );
                        }
                    }

                    // Reset inactive timer on any received message
                    self.inactive_timer = Instant::now();
                }
                Ok(None) => {
                    // No more messages available
                    break;
                }
                Err(e) => {
                    // Read error — return it to the caller
                    return Err(e);
                }
            }
        }

        Ok(count)
    }

    /// Process a received message and update internal state.
    ///
    /// This method updates flooding stats and inactive timer based on
    /// the message type, matching the C++ `receiveMessages()` switch.
    ///
    /// # Arguments
    /// * `msg_id` — The BT message type ID (0=Choke, 1=Unchoke, etc.)
    /// * `was_peer_choking` — Whether the peer was choking us before
    ///   this message (needed to detect choke/unchoke transitions)
    pub fn on_message_received(&mut self, msg_id: u8, was_peer_choking: bool) {
        match msg_id {
            // Choke (ID=0)
            0 => {
                if !was_peer_choking {
                    self.flooding_stat.inc_choke_unchoke_count();
                }
            }
            // Unchoke (ID=1)
            1 => {
                if was_peer_choking {
                    self.flooding_stat.inc_choke_unchoke_count();
                }
            }
            // Request (ID=6) or Piece (ID=7) — data exchange
            6 | 7 => {
                self.active_interaction_checker.record_data_exchange();
            }
            // KeepAlive (ID implied by zero-length)
            _ => {
                // KeepAlive messages increment flooding counter
                // In C++, this is handled by matching BtKeepAliveMessage::ID
                // We treat any unrecognized as potential keepalive for safety
            }
        }
    }

    /// Process a keepalive message for flooding detection.
    ///
    /// Call this when a KeepAlive message is received.
    pub fn on_keepalive_received(&mut self) {
        self.flooding_stat.inc_keepalive_count();
    }

    /// Dynamically scale `max_outstanding_request` based on request
    /// fulfillment rate.
    ///
    /// Mirrors the C++ logic at the end of `receiveMessages()`:
    /// if not in end-game and we lost >= 1/4 of outstanding requests,
    /// double `maxOutstandingRequest_` (up to `UB_MAX_OUTSTANDING_REQUEST`).
    pub fn scale_max_outstanding_request(
        &mut self,
        old_outstanding: usize,
        new_outstanding: usize,
        is_end_game: bool,
    ) {
        if !is_end_game
            && old_outstanding > new_outstanding
            && (old_outstanding - new_outstanding) * 4 >= self.max_outstanding_request
        {
            self.max_outstanding_request = (self.max_outstanding_request * 2)
                .min(UB_MAX_OUTSTANDING_REQUEST);
            debug!(
                "Scaled max_outstanding_request to {}",
                self.max_outstanding_request
            );
        }
    }

    // ── Request generation (C++ addRequests / fillPiece) ────────────────

    /// Get a reference to the per-peer request factory.
    pub fn request_factory(&self) -> &BtRequestFactory {
        &self.request_factory
    }

    /// Get a mutable reference to the per-peer request factory.
    pub fn request_factory_mut(&mut self) -> &mut BtRequestFactory {
        &mut self.request_factory
    }

    /// Check whether end-game mode is active.
    pub fn is_endgame(&self) -> bool {
        self.endgame
    }

    /// Fill target pieces from piece storage, up to `max_missing_block` total
    /// missing blocks across all target pieces.
    ///
    /// Mirrors C++ `DefaultBtInteractive::fillPiece(maxMissingBlock)`:
    ///
    /// 1. If `piece_storage.has_missing_piece(peer)`:
    ///    - Count current missing blocks in the request factory
    ///    - If `numMissingBlock >= maxMissingBlock`, return (already have enough)
    ///    - Calculate `diffMissingBlock = maxMissingBlock - numMissingBlock`
    ///    - If peer is choking us AND fast extension enabled: get fast pieces
    ///    - If peer is not choking us: get regular pieces
    ///    - For each piece: `request_factory.addTargetPiece(piece)`
    ///
    /// # Arguments
    ///
    /// * `piece_storage` — The piece storage provider (trait abstraction)
    /// * `conn` — The peer connection (for peer state and fast extension check)
    /// * `cuid` — Command ID for piece storage operations
    pub(crate) fn fill_piece(
        &mut self,
        piece_storage: &mut dyn PieceProvider,
        conn: &BtPeerConn,
        cuid: u64,
    ) {
        if !piece_storage.has_missing_piece(conn) {
            return;
        }

        let num_missing_block = self.request_factory.count_missing_block();
        if num_missing_block >= self.max_outstanding_request {
            return;
        }

        let diff_missing_block = self.max_outstanding_request - num_missing_block;
        let target_indexes = self.request_factory.get_target_piece_indexes();

        let pieces = if self.peer_choking {
            // Peer is choking us — only get fast pieces if fast extension enabled.
            // C++: if(peer_->peerChoking() && peer_->isFastExtensionEnabled())
            let fast_ext = conn
                .session_resource
                .as_ref()
                .map_or(false, |r| r.is_fast_extension_enabled());
            if fast_ext {
                piece_storage.get_missing_fast_pieces(
                    diff_missing_block,
                    conn,
                    &target_indexes,
                    cuid,
                )
            } else {
                Vec::new()
            }
        } else {
            // Peer is not choking us — get regular pieces.
            // C++: else { pieceStorage_->getMissingPiece(...) }
            piece_storage.get_missing_pieces(
                diff_missing_block,
                conn,
                &target_indexes,
                cuid,
            )
        };

        for piece in pieces {
            self.request_factory.add_target_piece(piece);
        }
    }

    /// Generate and queue piece requests, matching C++ `addRequests()`.
    ///
    /// This is the core request generation step called each iteration of
    /// the interaction loop. It:
    ///
    /// 1. Checks if end-game should be entered (no missing unused pieces
    ///    left but we still have target pieces with missing blocks).
    /// 2. Calls `fillPiece()` to ensure we have enough target pieces.
    /// 3. Calculates how many new requests to create based on the gap
    ///    between `maxOutstandingRequest` and current outstanding count.
    /// 4. Creates requests via `BtRequestFactory::create_request_messages()`
    ///    and queues them through the handler (actual sending happens in
    ///    step 12 of `do_interaction_processing()`).
    ///
    /// # Arguments
    ///
    /// * `piece_storage` — The piece storage provider (trait abstraction)
    /// * `conn` — The peer connection (for peer state checks)
    /// * `cuid` — Command ID for piece storage operations
    ///
    /// # Returns
    ///
    /// A vector of `PieceBlockRequest` descriptors for the requests that
    /// were generated. The caller can use this for tracking or logging.
    pub(crate) fn add_requests(
        &mut self,
        piece_storage: &mut dyn PieceProvider,
        conn: &BtPeerConn,
        cuid: u64,
    ) -> Vec<PieceBlockRequest> {
        // Check if we should enter end-game mode.
        // C++: if(!pieceStorage_->isEndGame() && !pieceStorage_->hasMissingUnusedPiece())
        if !self.endgame && !piece_storage.has_missing_unused_piece() {
            self.endgame = true;
            piece_storage.enter_end_game();
            debug!("Entered end-game mode");
        }

        // Fill target pieces from piece storage
        self.fill_piece(piece_storage, conn, cuid);

        // Calculate how many new requests to create
        // C++: reqNumToCreate = max(maxOutstandingRequest - countOutstandingRequest, 0)
        let outstanding = self.handler.count_outstanding_requests();
        let req_num_to_create = if self.max_outstanding_request > outstanding {
            self.max_outstanding_request - outstanding
        } else {
            0
        };

        let mut all_requests = Vec::new();

        if req_num_to_create > 0 {
            // Create request messages via the factory
            // C++ calls: btRequestFactory_->createRequestMessages(reqNumToCreate, isEndGame)
            let is_endgame = self.endgame;
            let requests = self.request_factory.create_request_messages(
                req_num_to_create,
                is_endgame,
                |index, block_index| self.handler.is_outstanding_request(index, block_index),
            );

            // Send each request through the handler and connection
            for req in &requests {
                // Serialize the Request message
                let serialized = aria2_protocol::bittorrent::message::serializer::serialize(
                    &BtMessage::Request {
                        request: aria2_protocol::bittorrent::message::types::PieceBlockRequest::new(
                            req.index, req.begin, req.length,
                        ),
                    },
                );

                // Queue through the handler (tracks request slots + outgoing queue)
                if let Some(_msg_bytes) = self.handler.send_request(
                    req.index,
                    req.begin,
                    req.length,
                    serialized,
                ) {
                    trace!(
                        "addRequests: queued request piece={} begin={} len={}",
                        req.index, req.begin, req.length
                    );
                }
            }

            all_requests = requests;
        }

        all_requests
    }

    /// Cancel all target pieces and remove outstanding requests.
    ///
    /// Mirrors C++ `DefaultBtInteractive::cancelAllPiece()`. Called when
    /// the peer connection is being torn down.
    ///
    /// Returns the indices of pieces that were removed (for the caller to
    /// abort outstanding requests in the dispatcher).
    pub fn cancel_all_piece(&mut self) -> Vec<u32> {
        let removed = self.request_factory.remove_all_target_pieces();
        removed.iter().map(|p| p.index() as u32).collect()
    }

    /// Remove completed pieces from the request factory.
    ///
    /// Mirrors C++ `btRequestFactory_->removeCompletedPiece()` called
    /// in `doInteractionProcessing()` step 9.
    ///
    /// Returns the indices of removed completed pieces (for the caller to
    /// abort outstanding requests in the dispatcher).
    pub fn remove_completed_piece(&mut self) -> Vec<u32> {
        self.request_factory.remove_completed_piece()
    }
}

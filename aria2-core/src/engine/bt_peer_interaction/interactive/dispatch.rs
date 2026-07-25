//! Message dispatch, receive, flooding detection, and outstanding-request
//! scaling for `BtPeerInteractive`.

use std::time::{Duration, Instant};

use aria2_protocol::bittorrent::message::types::BtMessage;

use crate::engine::bt_message_dispatcher::InactiveReason;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::extension_registry;
use crate::error::Result;
use tracing::{debug, trace, warn};

use super::super::piece_provider::PieceProvider;
use super::super::types::*;
use super::BtPeerInteractive;

impl BtPeerInteractive {
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
    pub(crate) async fn receive_messages<F>(
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

}

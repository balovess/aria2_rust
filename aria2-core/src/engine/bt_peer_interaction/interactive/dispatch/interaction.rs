//! Main interaction processing loop for `BtPeerInteractive`.
//!
//! Contains the core per-tick method [`do_interaction_processing()`] that
//! implements the 12-step C++ `DefaultBtInteractive::doInteractionProcessing()`
//! flow.

use std::time::{Duration, Instant};

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::Result;
use tracing::{debug, trace, warn};

use super::super::BtPeerInteractive;
use crate::engine::bt_peer_interaction::piece_provider::PieceProvider;
use crate::engine::bt_peer_interaction::types::*;

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
    #[allow(clippy::too_many_arguments)]
    pub async fn do_interaction_processing(
        &mut self,
        conn: &mut BtPeerConn,
        has_missing_piece: impl Fn(&BtPeerConn) -> bool,
        get_advertised_pieces: impl Fn() -> Vec<u32>,
        is_in_allowed_fast: impl Fn(u32) -> bool + Clone,
        is_block_acquired: impl Fn(u32, u32) -> bool,
        mut piece_storage: Option<&mut dyn PieceProvider>,
        cuid: u64,
    ) -> Result<InteractionResult> {
        // If not yet wired, skip interaction processing
        if self.state.is_handshake_state() {
            return Ok(InteractionResult::WaitingForHandshake);
        }

        if self.metadata_get_mode {
            // Simplified metadata-get mode: just keep-alive + receive
            if self.should_send_keepalive()
                && let Err(e) = conn.send_keepalive().await
            {
                warn!("Failed to send keepalive in metadata-get mode: {}", e);
            }
            let (count, pex_update, _) = self
                .receive_messages(conn, is_in_allowed_fast.clone())
                .await?;
            self.num_received_message = count;
            return Ok(InteractionResult::Continue {
                pex_pending: false,
                pex_update,
            });
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
                    .send_cancel(
                        &aria2_protocol::bittorrent::message::types::PieceBlockRequest::new(
                            *index, *begin, *length,
                        ),
                    )
                    .await
                {
                    warn!("Failed to send Cancel for piece {}: {}", index, e);
                }
            }
            trace!("Per-second timer fired, request slot check done");
        }

        // ── Step 3: receiveMessages ─────────────────────────────────────
        let (received_count, pex_update, bitfield_updates) = self
            .receive_messages(conn, is_in_allowed_fast.clone())
            .await?;
        if let Some(storage) = piece_storage {
            for update in &bitfield_updates {
                storage.update_piece_stats(&update.new, &update.old);
            }
            piece_storage = Some(storage);
        }
        self.num_received_message = received_count;

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
        if self.ut_pex_enabled && self.pex_timer.elapsed() >= Duration::from_secs(PEX_INTERVAL_SECS)
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

        Ok(InteractionResult::Continue {
            pex_pending,
            pex_update,
        })
    }
}

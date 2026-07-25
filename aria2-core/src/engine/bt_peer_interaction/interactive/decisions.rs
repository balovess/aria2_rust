//! Choking/interest decisions, check-have, keep-alive, flooding detection,
//! peer exchange, and request generation for `BtPeerInteractive`.

use std::time::Duration;

use aria2_protocol::bittorrent::message::types::BtMessage;

use crate::engine::bt_message_dispatcher::InactiveReason;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_request_factory::{BtRequestFactory, PieceBlockRequest};
use crate::engine::extension_registry::ExtensionRegistry;
use tracing::{debug, trace, warn};

use super::super::piece_provider::PieceProvider;
use super::super::types::*;
use super::BtPeerInteractive;

impl BtPeerInteractive {
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
        self.keep_alive_timer = std::time::Instant::now();
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
            self.flooding_timer = std::time::Instant::now();
            detected
        } else {
            false
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

    // ── Message-received helpers ────────────────────────────────────────

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
}

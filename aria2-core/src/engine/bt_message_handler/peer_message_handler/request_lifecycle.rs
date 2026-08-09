//! Request lifecycle management — send, receive, query, abort, and scale.

use crate::engine::bt_message_dispatcher::RequestSlot;
use tracing::debug;

use super::super::types::{BLOCK_SIZE, UB_MAX_OUTSTANDING_REQUEST};
use super::BtPeerMessageHandler;

impl BtPeerMessageHandler {
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
        Some(aria2_protocol::bittorrent::message::serializer::serialize(
            &aria2_protocol::bittorrent::message::types::BtMessage::Request {
                request: aria2_protocol::bittorrent::message::types::PieceBlockRequest::new(
                    index, begin, length,
                ),
            },
        ))
    }

    /// Handle receiving a Piece message from the peer.
    ///
    /// Removes the matching request slot from the dispatcher.
    /// Returns the removed [`RequestSlot`] if found, or `None` if the
    /// piece data was unsolicited (no matching outstanding request).
    ///
    /// Mirrors C++ `BtPieceMessage::doReceivedAction()` which calls
    /// `dispatcher_->removeOutstandingRequest()`.
    pub fn on_piece_received(
        &mut self,
        index: u32,
        begin: u32,
        length: u32,
    ) -> Option<RequestSlot> {
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
                let new_max = (self.max_outstanding_requests * 2).min(UB_MAX_OUTSTANDING_REQUEST);
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
}

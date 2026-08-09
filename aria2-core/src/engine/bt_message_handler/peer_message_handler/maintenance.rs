//! Periodic maintenance, message queue operations, upload limiting, and cleanup.

use crate::engine::bt_message_dispatcher::{BtMessageDispatcher, SlotCheckResult};
use tracing::warn;

use super::BtPeerMessageHandler;

impl BtPeerMessageHandler {
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
        self.dispatcher
            .add_upload_message(data, index, begin, length);
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

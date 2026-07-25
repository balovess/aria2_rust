//! Tests for BtPeerMessageHandler — per-peer stateful message handler
//!
//! Split from bt_message_handler.rs to keep the main module under 1200 lines.

#[cfg(test)]
pub(crate) mod tests {
    use crate::engine::bt_message_handler::{
        BtPeerMessageHandler, BLOCK_SIZE, DEFAULT_MAX_OUTSTANDING_REQUEST,
        UB_MAX_OUTSTANDING_REQUEST,
    };
    use std::time::{Duration, Instant};

    // ── BtPeerMessageHandler creation tests ─────────────────────────────

    #[test]
    fn test_peer_handler_new_defaults() {
        let handler = BtPeerMessageHandler::new(BLOCK_SIZE);
        assert!(handler.is_peer_choking());
        assert!(!handler.is_peer_snubbing());
        assert!(!handler.has_outstanding_requests());
        assert_eq!(handler.count_outstanding_requests(), 0);
        assert!(handler.can_send_request());
        assert_eq!(handler.max_outstanding_requests(), DEFAULT_MAX_OUTSTANDING_REQUEST);
    }

    #[test]
    fn test_peer_handler_custom_max_outstanding() {
        let handler = BtPeerMessageHandler::with_max_outstanding(BLOCK_SIZE, 10);
        assert_eq!(handler.max_outstanding_requests(), 10);
    }

    // ── Request slot lifecycle tests ────────────────────────────────────

    #[test]
    fn test_send_request_adds_slot_and_queue() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        let result = handler.send_request(5, 0, BLOCK_SIZE, vec![0u8; 17]);
        assert!(result.is_some());
        assert_eq!(handler.count_outstanding_requests(), 1);
        assert!(handler.is_outstanding_request(5, 0));
        assert!(handler.has_pending_messages());
    }

    #[test]
    fn test_send_request_respects_max_outstanding() {
        let mut handler = BtPeerMessageHandler::with_max_outstanding(BLOCK_SIZE, 2);

        // Fill up to max
        assert!(handler.send_request(1, 0, BLOCK_SIZE, vec![1]).is_some());
        assert!(handler.send_request(2, 0, BLOCK_SIZE, vec![2]).is_some());
        assert_eq!(handler.count_outstanding_requests(), 2);

        // Third request should be rejected
        assert!(handler.send_request(3, 0, BLOCK_SIZE, vec![3]).is_none());
        assert!(!handler.can_send_request());
    }

    #[test]
    fn test_on_piece_received_removes_slot() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        handler.send_request(5, 0, BLOCK_SIZE, vec![0u8; 17]);
        assert_eq!(handler.count_outstanding_requests(), 1);

        let slot = handler.on_piece_received(5, 0, BLOCK_SIZE);
        assert!(slot.is_some());
        assert_eq!(handler.count_outstanding_requests(), 0);

        let slot = slot.unwrap();
        assert_eq!(slot.index, 5);
        assert_eq!(slot.begin, 0);
        assert_eq!(slot.length, BLOCK_SIZE);
    }

    #[test]
    fn test_on_piece_received_unsolicited() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        // No outstanding request for this piece
        let slot = handler.on_piece_received(99, 0, BLOCK_SIZE);
        assert!(slot.is_none());
    }

    #[test]
    fn test_request_slot_full_lifecycle() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        // Send multiple requests
        handler.send_request(5, 0, BLOCK_SIZE, vec![1]);
        handler.send_request(5, BLOCK_SIZE, BLOCK_SIZE, vec![2]);
        handler.send_request(6, 0, BLOCK_SIZE, vec![3]);
        assert_eq!(handler.count_outstanding_requests(), 3);

        // Receive pieces out of order
        let slot = handler.on_piece_received(6, 0, BLOCK_SIZE);
        assert!(slot.is_some());
        assert_eq!(handler.count_outstanding_requests(), 2);

        let slot = handler.on_piece_received(5, 0, BLOCK_SIZE);
        assert!(slot.is_some());
        assert_eq!(handler.count_outstanding_requests(), 1);

        let slot = handler.on_piece_received(5, BLOCK_SIZE, BLOCK_SIZE);
        assert!(slot.is_some());
        assert_eq!(handler.count_outstanding_requests(), 0);
    }

    // ── Event-driven action tests ───────────────────────────────────────

    #[test]
    fn test_on_choke_received_removes_non_fast_slots() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);
        handler.peer_choking = false; // Start unchoked

        handler.send_request(5, 0, BLOCK_SIZE, vec![1]);
        handler.send_request(6, 0, BLOCK_SIZE, vec![2]);
        handler.send_request(7, 0, BLOCK_SIZE, vec![3]);

        // Piece 6 is in allowed-fast set
        let removed = handler.on_choke_received(|idx| idx == 6);
        assert_eq!(removed.len(), 2); // pieces 5 and 7 removed
        assert!(handler.is_peer_choking());
        assert_eq!(handler.count_outstanding_requests(), 1); // piece 6 kept
        assert!(handler.is_outstanding_request(6, 0));
    }

    #[test]
    fn test_on_choke_received_keeps_all_if_all_fast() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);
        handler.peer_choking = false;

        handler.send_request(5, 0, BLOCK_SIZE, vec![1]);
        handler.send_request(6, 0, BLOCK_SIZE, vec![2]);

        // All pieces are in allowed-fast set
        let removed = handler.on_choke_received(|_| true);
        assert!(removed.is_empty());
        assert_eq!(handler.count_outstanding_requests(), 2);
    }

    #[test]
    fn test_on_unchoke_received() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);
        assert!(handler.is_peer_choking());

        handler.on_unchoke_received();
        assert!(!handler.is_peer_choking());
    }

    #[test]
    fn test_on_choke_sent_invalidates_uploads() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        handler.queue_control_message(vec![1]); // Choke
        handler.queue_upload_message(vec![2], 5, 0, BLOCK_SIZE); // Piece
        handler.queue_upload_message(vec![3], 6, 0, BLOCK_SIZE); // Piece

        handler.on_choke_sent();

        // Only non-upload messages should remain sendable
        let sendable = handler.drain_sendable_messages();
        assert_eq!(sendable.len(), 1); // Only the control message
    }

    #[test]
    fn test_on_cancel_received_invalidates_matching_piece() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        handler.queue_upload_message(vec![1], 5, 0, BLOCK_SIZE);
        handler.queue_upload_message(vec![2], 5, BLOCK_SIZE, BLOCK_SIZE);
        handler.queue_upload_message(vec![3], 6, 0, BLOCK_SIZE);

        handler.on_cancel_received(5, 0, BLOCK_SIZE);

        // Only piece 5@BLOCK_SIZE and 6@0 should remain
        let sendable = handler.drain_sendable_messages();
        assert_eq!(sendable.len(), 2);
    }

    #[test]
    fn test_on_keepalive_received_increments_counter() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        handler.on_keepalive_received();
        handler.on_keepalive_received();

        // Not yet flooding (check interval hasn't elapsed)
        assert!(!handler.detect_flooding());
    }

    // ── Abort piece requests tests ──────────────────────────────────────

    #[test]
    fn test_abort_piece_requests() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        handler.send_request(5, 0, BLOCK_SIZE, vec![1]);
        handler.send_request(5, BLOCK_SIZE, BLOCK_SIZE, vec![2]);
        handler.send_request(6, 0, BLOCK_SIZE, vec![3]);

        let removed = handler.abort_piece_requests(5);
        assert_eq!(removed.len(), 2);
        assert_eq!(handler.count_outstanding_requests(), 1); // piece 6 remains
    }

    // ── Flooding detection tests ────────────────────────────────────────

    #[test]
    fn test_flooding_detected_via_choke_unchoke() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        // Simulate rapid choke/unchoke transitions
        handler.on_choke_received(|_| false);
        handler.on_unchoke_received();
        // Now 2 choke/unchoke transitions

        // Force the check interval to have elapsed
        handler.flooding_stat.last_reset = Instant::now() - Duration::from_secs(6);

        assert!(handler.detect_flooding());
    }

    #[test]
    fn test_flooding_detected_via_keepalive() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        handler.on_keepalive_received();
        handler.on_keepalive_received();
        // 2 keepalive messages

        handler.flooding_stat.last_reset = Instant::now() - Duration::from_secs(6);

        assert!(handler.detect_flooding());
    }

    #[test]
    fn test_no_flooding_under_normal_traffic() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        // Only 1 transition — not flooding
        handler.on_unchoke_received();
        assert!(!handler.detect_flooding());
    }

    // ── Timeout detection tests ─────────────────────────────────────────

    #[test]
    fn test_check_request_slots_timeout_marks_snubbing() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        handler.send_request(5, 0, BLOCK_SIZE, vec![1]);
        assert!(!handler.is_peer_snubbing());

        // Manually set the slot's dispatched time to the past
        if let Some(slot) = handler.dispatcher_mut().request_slots.first_mut() {
            slot.dispatched_at = Instant::now() - Duration::from_secs(120);
        }

        let result = handler.check_request_slots(|_, _| false);
        assert!(result.timed_out);
        assert!(handler.is_peer_snubbing());
        assert_eq!(handler.count_outstanding_requests(), 0);
    }

    #[test]
    fn test_check_request_slots_acquired_block() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        handler.send_request(5, 0, BLOCK_SIZE, vec![1]); // block_index = 0
        handler.send_request(5, BLOCK_SIZE, BLOCK_SIZE, vec![2]); // block_index = 1

        // Block 0 of piece 5 was acquired from another peer
        let result = handler.check_request_slots(|piece, block| piece == 5 && block == 0);
        assert!(!result.timed_out);
        assert_eq!(result.cancelled_blocks.len(), 1);
        assert_eq!(result.cancelled_blocks[0], (5, 0, BLOCK_SIZE));
        assert_eq!(handler.count_outstanding_requests(), 1); // block 1 still there
    }

    // ── Outstanding request scaling tests ───────────────────────────────

    #[test]
    fn test_scale_max_outstanding_requests_up() {
        let mut handler = BtPeerMessageHandler::with_max_outstanding(BLOCK_SIZE, 10);

        // Fill up 8 slots (80% of max)
        for i in 0..8 {
            handler.send_request(i, 0, BLOCK_SIZE, vec![1]);
        }
        assert_eq!(handler.count_outstanding_requests(), 8);

        // Simulate 3 slots being satisfied (3 * 4 = 12 >= 10)
        // This should trigger scaling
        let old = handler.count_outstanding_requests();
        for i in 0..3 {
            handler.on_piece_received(i, 0, BLOCK_SIZE);
        }
        handler.scale_max_outstanding_requests(old);
        assert_eq!(handler.max_outstanding_requests(), 20); // doubled
    }

    #[test]
    fn test_scale_max_outstanding_requests_capped() {
        let mut handler = BtPeerMessageHandler::with_max_outstanding(BLOCK_SIZE, 300);

        // Even if doubled, it should cap at UB_MAX_OUTSTANDING_REQUEST
        for i in 0..200 {
            handler.send_request(i, 0, BLOCK_SIZE, vec![1]);
        }
        let old = handler.count_outstanding_requests();
        for i in 0..200 {
            handler.on_piece_received(i, 0, BLOCK_SIZE);
        }
        handler.scale_max_outstanding_requests(old);
        assert_eq!(handler.max_outstanding_requests(), UB_MAX_OUTSTANDING_REQUEST);
    }

    #[test]
    fn test_no_scaling_when_few_satisfied() {
        let mut handler = BtPeerMessageHandler::with_max_outstanding(BLOCK_SIZE, 100);

        handler.send_request(1, 0, BLOCK_SIZE, vec![1]);
        handler.send_request(2, 0, BLOCK_SIZE, vec![2]);

        // Only 1 satisfied (1 * 4 = 4 < 100), no scaling
        let old = handler.count_outstanding_requests();
        handler.on_piece_received(1, 0, BLOCK_SIZE);
        handler.scale_max_outstanding_requests(old);
        assert_eq!(handler.max_outstanding_requests(), 100); // unchanged
    }

    // ── Message queue operations tests ──────────────────────────────────

    #[test]
    fn test_queue_and_drain_messages() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        handler.queue_control_message(vec![0, 0, 0, 0]); // Choke
        handler.queue_control_message(vec![0, 0, 0, 1]); // Unchoke
        assert!(handler.has_pending_messages());
        assert_eq!(handler.count_pending_messages(), 2);

        let sent = handler.drain_sendable_messages();
        assert_eq!(sent.len(), 2);
        assert!(!handler.has_pending_messages());
    }

    #[test]
    fn test_upload_deferral_via_speed_limit() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        handler.queue_control_message(vec![1]);
        handler.queue_upload_message(vec![2], 0, 0, BLOCK_SIZE);
        handler.set_upload_speed_exceeded(true);

        let sent = handler.drain_sendable_messages();
        assert_eq!(sent.len(), 1); // Only control message sent
        assert!(handler.has_pending_messages()); // Upload still queued

        handler.set_upload_speed_exceeded(false);
        let sent = handler.drain_sendable_messages();
        assert_eq!(sent.len(), 1); // The deferred upload message
    }

    // ── Cleanup tests ───────────────────────────────────────────────────

    #[test]
    fn test_clear_resets_state() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        handler.send_request(5, 0, BLOCK_SIZE, vec![1]);
        handler.queue_control_message(vec![2]);
        handler.on_unchoke_received();
        handler.peer_snubbing = true;

        assert!(handler.has_outstanding_requests());
        assert!(handler.has_pending_messages());

        handler.clear();

        assert!(!handler.has_outstanding_requests());
        assert!(!handler.has_pending_messages());
        assert!(!handler.is_peer_snubbing());
    }

    // ── Dispatcher access tests ─────────────────────────────────────────

    #[test]
    fn test_dispatcher_access() {
        let mut handler = BtPeerMessageHandler::new(BLOCK_SIZE);

        // Read-only access
        assert_eq!(handler.dispatcher().count_request_slots(), 0);

        // Mutable access
        handler.dispatcher_mut().add_request_slot(5, 0, BLOCK_SIZE);
        assert_eq!(handler.count_outstanding_requests(), 1);
    }

    // ── Default max outstanding constant test ───────────────────────────

    #[test]
    fn test_default_max_outstanding_matches_cpp() {
        // C++ BtConstants.h: DEFAULT_MAX_OUTSTANDING_REQUEST = 6, UB = 256
        assert_eq!(DEFAULT_MAX_OUTSTANDING_REQUEST, 6);
        assert_eq!(UB_MAX_OUTSTANDING_REQUEST, 256);
    }
}

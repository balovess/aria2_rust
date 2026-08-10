//! Tests for peer_stats — per-peer statistics tracking and speed calculation.

#[cfg(test)]
pub(crate) mod tests {
    use std::net::SocketAddr;
    use std::thread;
    use std::time::Duration;

    use crate::engine::peer_stats::{BAD_DATA_THRESHOLD, PeerStats};

    fn make_test_peer() -> PeerStats {
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        PeerStats::new([0x42; 20], addr)
    }

    #[test]
    fn test_new_peer_stats() {
        let stats = make_test_peer();

        // Byte counters should be zero
        assert_eq!(stats.uploaded_bytes, 0);
        assert_eq!(stats.downloaded_bytes, 0);

        // Speeds should be zero
        assert_eq!(stats.upload_speed, 0.0);
        assert_eq!(stats.download_speed, 0.0);

        // Default choke state: we choke the peer by default
        assert!(stats.am_choking);
        assert!(!stats.am_interested);

        // Peer default states
        assert!(stats.peer_choking); // peer chokes us initially
        assert!(!stats.peer_interested);

        // Not snubbed initially
        assert!(!stats.is_snubbed);

        // Peer ID preserved
        assert_eq!(stats.peer_id, [0x42; 20]);
    }

    #[test]
    fn test_on_data_sent_updates_counters() {
        let mut stats = make_test_peer();

        // Small sleep so elapsed > 0
        thread::sleep(Duration::from_millis(10));

        stats.on_data_sent(1024);

        assert_eq!(stats.uploaded_bytes, 1024);
        assert!(
            stats.upload_speed > 0.0,
            "upload_speed should be positive after sending data"
        );

        // Send more data
        thread::sleep(Duration::from_millis(10));
        stats.on_data_sent(2048);

        assert_eq!(stats.uploaded_bytes, 1024 + 2048);
        assert!(stats.upload_speed > 0.0);
    }

    #[test]
    fn test_on_data_received_resets_snubbed() {
        let mut stats = make_test_peer();

        // Mark as snubbed manually
        stats.is_snubbed = true;
        assert!(stats.is_snubbed);

        // Receive data -- should reset snubbed flag
        thread::sleep(Duration::from_millis(10));
        stats.on_data_received(512);

        assert!(
            !stats.is_snubbed,
            "receiving data should reset snubbed status"
        );
        assert_eq!(stats.downloaded_bytes, 512);
        assert!(stats.download_speed > 0.0);
    }

    #[test]
    fn test_check_snubbed_timeout() {
        let mut stats = make_test_peer();

        // Immediately after creation, should NOT be snubbed with a reasonable timeout
        let result = stats.check_snubbed(10);
        assert!(!result, "should not be snubbed immediately");
        assert!(!stats.is_snubbed);

        // Use timeout=0 to guarantee it triggers (elapsed >= 0 always true)
        let result = stats.check_snubbed(0);
        assert!(
            result,
            "with timeout=0, any elapsed time should trigger snubbed"
        );
        assert!(stats.is_snubbed);

        // Calling again should return false (already snubbed)
        let result2 = stats.check_snubbed(0);
        assert!(
            !result2,
            "second call should return false (already snubbed)"
        );
    }

    #[test]
    fn test_choke_state_transitions() {
        let mut stats = make_test_peer();

        // Initial state: we are choking
        assert!(stats.am_choking);

        // Unchoke
        stats.record_unchoke();
        assert!(!stats.am_choking);

        // Verify timestamp updated
        let unchoke_time = stats.time_since_last_unchoke();
        assert!(unchoke_time < Duration::from_millis(100));

        // Re-choke
        stats.record_choke();
        assert!(stats.am_choking);

        // Optimistic unchoke
        stats.record_optimistic_unchoke();
        assert!(!stats.am_choking);

        let opt_time = stats.time_since_last_optimistic_unchoke();
        assert!(opt_time < Duration::from_millis(100));
    }

    #[test]
    fn test_cumulative_byte_counts() {
        let mut stats = make_test_peer();

        for _ in 0..5 {
            thread::sleep(Duration::from_millis(5));
            stats.on_data_sent(1024);
            thread::sleep(Duration::from_millis(5));
            stats.on_data_received(2048);
        }

        assert_eq!(stats.uploaded_bytes, 5 * 1024);
        assert_eq!(stats.downloaded_bytes, 5 * 2048);
    }

    #[test]
    fn test_reset_snubbed_explicit() {
        let mut stats = make_test_peer();

        stats.is_snubbed = true;
        stats.reset_snubbed();
        assert!(!stats.is_snubbed);
    }

    // ==================================================================
    // Bad Peer Ban System Tests
    // ==================================================================

    #[test]
    fn test_new_peer_stats_default_ban_state() {
        let stats = make_test_peer();

        // Bad data tracking should start at 0
        assert_eq!(stats.bad_data_count, 0);
        assert_eq!(stats.snub_count, 0);

        // Peer should not be banned initially
        assert!(!stats.is_banned);
        assert!(stats.ban_reason.is_none());

        // Should be eligible for selection
        assert!(stats.is_eligible_for_selection());

        // Average speeds should be 0
        assert_eq!(stats.avg_upload_speed, 0);
        assert_eq!(stats.avg_download_speed, 0);

        // Timestamps should be None
        assert!(stats.last_data_time.is_none());
        assert!(stats.last_upload_time.is_none());
    }

    #[test]
    fn test_bad_data_incremented_on_invalid_hash() {
        let mut stats = make_test_peer();

        // First invalid piece
        let should_ban = stats.increment_bad_data();
        assert!(!should_ban, "Should not ban after 1 bad piece");
        assert_eq!(stats.bad_data_count, 1);

        // Second invalid piece
        let should_ban = stats.increment_bad_data();
        assert!(!should_ban, "Should not ban after 2 bad pieces");
        assert_eq!(stats.bad_data_count, 2);
    }

    #[test]
    fn test_ban_threshold_reached_peer_banned() {
        let mut stats = make_test_peer();

        // Increment to threshold (BAD_DATA_THRESHOLD = 3)
        stats.increment_bad_data(); // count = 1
        stats.increment_bad_data(); // count = 2

        // Third strike - should trigger ban
        let should_ban = stats.increment_bad_data();
        assert!(
            should_ban,
            "Should ban after {} bad pieces",
            BAD_DATA_THRESHOLD
        );
        assert_eq!(stats.bad_data_count, BAD_DATA_THRESHOLD);
    }

    #[test]
    fn test_successful_piece_decrements_bad_count() {
        let mut stats = make_test_peer();

        // Simulate some bad pieces
        stats.increment_bad_data(); // count = 1
        stats.increment_bad_data(); // count = 2
        assert_eq!(stats.bad_data_count, 2);

        // Successful piece received - gradual recovery
        stats.decrement_bad_data();
        assert_eq!(stats.bad_data_count, 1, "Should decrement by 1");

        // Another successful piece
        stats.decrement_bad_data();
        assert_eq!(stats.bad_data_count, 0, "Should reach 0");

        // Decrementing below 0 should floor at 0
        stats.decrement_bad_data();
        assert_eq!(stats.bad_data_count, 0, "Should never go negative");
    }

    #[test]
    fn test_ban_peer_sets_flags_and_reason() {
        let mut stats = make_test_peer();

        assert!(!stats.is_banned);
        assert!(stats.ban_reason.is_none());
        assert!(stats.is_eligible_for_selection());

        // Ban with reason
        stats.ban_peer("Too many invalid pieces (3 >= 3)".to_string());

        assert!(stats.is_banned, "Peer should be marked as banned");
        assert!(
            stats.ban_reason.as_deref() == Some("Too many invalid pieces (3 >= 3)"),
            "Ban reason should be preserved"
        );
        assert!(
            !stats.is_eligible_for_selection(),
            "Banned peer should not be eligible for selection"
        );
    }

    #[test]
    fn test_banned_peer_excluded_from_selection() {
        let mut stats = make_test_peer();

        // Before banning: eligible
        assert!(stats.is_eligible_for_selection());

        // After banning: excluded
        stats.ban_peer("Test ban".to_string());
        assert!(!stats.is_eligible_for_selection());

        // Even if we try to unchoke them, they remain banned
        stats.record_unchoke();
        assert!(
            !stats.is_eligible_for_selection(),
            "Banned status persists after unchoke"
        );
    }

    #[test]
    fn test_snub_count_increments_on_snubbed() {
        let mut stats = make_test_peer();

        assert_eq!(stats.snub_count, 0);

        // Trigger snub detection with timeout=0
        let snubbed = stats.check_snubbed(0);
        assert!(snubbed);
        assert_eq!(
            stats.snub_count, 1,
            "Snub count should increment on first snub"
        );

        // Second call should NOT increment (already snubbed)
        let snubbed_again = stats.check_snubbed(0);
        assert!(!snubbed_again, "Already snubbed, should return false");
        assert_eq!(stats.snub_count, 1, "Snub count should NOT increment again");

        // Reset and re-check
        stats.reset_snubbed();
        let snubbed_third = stats.check_snubbed(0);
        assert!(snubbed_third);
        assert_eq!(
            stats.snub_count, 2,
            "Snub count should increment after reset+re-snub"
        );
    }

}

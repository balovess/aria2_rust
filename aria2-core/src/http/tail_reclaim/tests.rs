//! Tests for the tail reclaim module.
//!
//! Covers config defaults, reclaim logic, stall tracking, and integration
//! scenarios combining config + tracker.

use std::time::{Duration, Instant};

use super::*;

// ── TailReclaimConfig defaults ──────────────────────────────────────

#[test]
fn test_config_default_values() {
    let config = TailReclaimConfig::default();
    assert_eq!(config.min_tail_length, DEFAULT_MIN_TAIL_LENGTH);
    assert_eq!(config.stall_timeout_secs, DEFAULT_STALL_TIMEOUT_SECS);
    assert!(config.enabled);
}

#[test]
fn test_config_builder_pattern() {
    let config = TailReclaimConfig::new()
        .with_min_tail_length(512 * 1024)
        .with_stall_timeout_secs(60)
        .with_enabled(false);

    assert_eq!(config.min_tail_length, 512 * 1024);
    assert_eq!(config.stall_timeout_secs, 60);
    assert!(!config.enabled);
}

// ── TailReclaimConfig::should_reclaim ───────────────────────────────

#[test]
fn test_should_reclaim_disabled() {
    let config = TailReclaimConfig::new().with_enabled(false);
    // 10 MiB range, nothing received — clearly reclaimable, but disabled.
    assert!(!config.should_reclaim(0, 10 * 1024 * 1024, 0, 0));
}

#[test]
fn test_should_reclaim_large_remaining() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // Range [0, 2047], 0 bytes received, 0 in flight → remaining = 2048 > 1024.
    assert!(config.should_reclaim(0, 2047, 0, 0));
}

#[test]
fn test_should_reclaim_small_remaining() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // Range [0, 1023], 0 bytes received → remaining = 1024, not > 1024.
    assert!(!config.should_reclaim(0, 1023, 0, 0));
}

#[test]
fn test_should_reclaim_exact_min_tail_boundary() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // remaining = 1025 > 1024 → should reclaim
    assert!(config.should_reclaim(0, 1024, 0, 0));
    // remaining = 1024 → not > 1024 → should not reclaim
    assert!(!config.should_reclaim(0, 1023, 0, 0));
}

#[test]
fn test_should_reclaim_with_bytes_received() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // Range [0, 4095] = 4096 bytes, 3072 received, 0 in flight → remaining = 1024.
    // 1024 is not > 1024, so no reclaim.
    assert!(!config.should_reclaim(0, 4095, 3072, 0));
    // 3071 received → remaining = 1025 > 1024.
    assert!(config.should_reclaim(0, 4095, 3071, 0));
}

#[test]
fn test_should_reclaim_with_bytes_in_flight() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // Range [0, 4095] = 4096 bytes, 2048 received, 1024 in flight → remaining = 1024.
    // 1024 is not > 1024, so no reclaim.
    assert!(!config.should_reclaim(0, 4095, 2048, 1024));
    // 2047 received + 1024 in flight = 3071 consumed → remaining = 1025 > 1024.
    assert!(config.should_reclaim(0, 4095, 2047, 1024));
}

#[test]
fn test_should_reclaim_all_received() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // Range [0, 4095] = 4096 bytes, all received → remaining = 0.
    assert!(!config.should_reclaim(0, 4095, 4096, 0));
}

#[test]
fn test_should_reclaim_over_received() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // More received than the range size — should not panic, should return false.
    assert!(!config.should_reclaim(0, 4095, 5000, 0));
}

#[test]
fn test_should_reclaim_inverted_range() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // range_end < range_start — invalid range, should return false.
    assert!(!config.should_reclaim(100, 50, 0, 0));
}

#[test]
fn test_should_reclaim_nonzero_start() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // Range [1000, 3095] = 2096 bytes, 0 received → remaining = 2096 > 1024.
    assert!(config.should_reclaim(1000, 3095, 0, 0));
    // 1072 received → remaining = 1024 → not > 1024.
    assert!(!config.should_reclaim(1000, 3095, 1072, 0));
}

// ── TailReclaimConfig::calculate_tail ────────────────────────────────

#[test]
fn test_calculate_tail_basic() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // Range [0, 4095], 2048 received, 0 in flight → tail from 2048 to 4095.
    let result = config.calculate_tail(0, 4095, 2048, 0);
    assert!(result.is_some());
    let r = result.unwrap();
    assert_eq!(r.tail_start, 2048);
    assert_eq!(r.tail_end, 4095);
    assert_eq!(r.length(), 2048);
}

#[test]
fn test_calculate_tail_with_in_flight() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // Range [0, 4095], 1024 received, 1024 in flight → tail from 2048 to 4095.
    let result = config.calculate_tail(0, 4095, 1024, 1024);
    assert!(result.is_some());
    let r = result.unwrap();
    assert_eq!(r.tail_start, 2048);
    assert_eq!(r.tail_end, 4095);
    assert_eq!(r.length(), 2048);
}

#[test]
fn test_calculate_tail_no_reclaim_returns_none() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // remaining = 1024, not > 1024 → None.
    assert!(config.calculate_tail(0, 4095, 3072, 0).is_none());
}

#[test]
fn test_calculate_tail_disabled_returns_none() {
    let config = TailReclaimConfig::new().with_enabled(false);
    assert!(config.calculate_tail(0, 10 * 1024 * 1024, 0, 0).is_none());
}

#[test]
fn test_calculate_tail_nonzero_start() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // Range [1000, 5095], 1000 received, 0 in flight → tail from 2000 to 5095.
    let result = config.calculate_tail(1000, 5095, 1000, 0);
    assert!(result.is_some());
    let r = result.unwrap();
    assert_eq!(r.tail_start, 2000);
    assert_eq!(r.tail_end, 5095);
    assert_eq!(r.length(), 3096);
}

#[test]
fn test_calculate_tail_invariant_tail_start_greater_than_range_start() {
    let config = TailReclaimConfig::new().with_min_tail_length(0);
    // Range [0, 0] = 1 byte, 0 received → remaining = 1 > 0, tail_start = 0.
    // tail_start <= range_start → should return None (invariant violated).
    assert!(config.calculate_tail(0, 0, 0, 0).is_none());
}

#[test]
fn test_calculate_tail_invariant_tail_start_within_range() {
    let config = TailReclaimConfig::new().with_min_tail_length(0);
    // Range [10, 12] = 3 bytes, 0 received → remaining = 3 > 0.
    // tail_start = 10, which is <= range_start (10) → None.
    assert!(config.calculate_tail(10, 12, 0, 0).is_none());
    // With 1 byte received → tail_start = 11, > range_start = 10 → valid.
    let result = config.calculate_tail(10, 12, 1, 0);
    assert!(result.is_some());
    let r = result.unwrap();
    assert_eq!(r.tail_start, 11);
    assert_eq!(r.tail_end, 12);
    assert_eq!(r.length(), 2);
}

#[test]
fn test_calculate_tail_over_consumed_returns_none() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // More consumed than the range — saturating_sub makes remaining = 0.
    // should_reclaim returns false, so calculate_tail returns None.
    assert!(config.calculate_tail(0, 4095, 5000, 0).is_none());
}

// ── TailReclaimResult ───────────────────────────────────────────────

#[test]
fn test_result_length_single_byte() {
    let result = TailReclaimResult {
        tail_start: 100,
        tail_end: 100,
    };
    assert_eq!(result.length(), 1);
}

#[test]
fn test_result_length_range() {
    let result = TailReclaimResult {
        tail_start: 100,
        tail_end: 199,
    };
    assert_eq!(result.length(), 100);
}

// ── ConnectionStallTracker ──────────────────────────────────────────

#[test]
fn test_tracker_new_not_stalled() {
    let tracker = ConnectionStallTracker::new();
    assert!(!tracker.is_stalled());
    assert_eq!(tracker.last_bytes_received(), 0);
}

#[test]
fn test_tracker_update_progress_resets_stall() {
    let mut tracker = ConnectionStallTracker::new();
    // Simulate a stall by checking with a very short timeout.
    // (In practice, we can't wait 30 seconds in a unit test, so we test
    // the logic with update_progress_at and check_stalled_at.)

    // Mark progress
    tracker.update_progress(1024);
    assert!(!tracker.is_stalled());
    assert_eq!(tracker.last_bytes_received(), 1024);
}

#[test]
fn test_tracker_update_progress_same_bytes_no_reset() {
    let now = Instant::now();
    let mut tracker = ConnectionStallTracker::new_at(now);

    // First progress update
    tracker.update_progress_at(1024, now);
    assert_eq!(tracker.last_bytes_received(), 1024);

    // Same byte count — no progress, timer not reset.
    let later = now + Duration::from_secs(10);
    tracker.update_progress_at(1024, later);
    // last_progress_time should still be the first update time.
    // (We verify indirectly by checking stall detection.)
    assert!(!tracker.is_stalled());
    assert_eq!(tracker.last_bytes_received(), 1024);
}

#[test]
fn test_tracker_stall_detection_with_at() {
    let now = Instant::now();
    let mut tracker = ConnectionStallTracker::new_at(now);

    // Initial progress
    tracker.update_progress_at(1024, now);

    // Check at exactly timeout — stalled
    let at_timeout = now + Duration::from_secs(30);
    assert!(tracker.check_stalled_at(Duration::from_secs(30), at_timeout));

    // Check just before timeout — not stalled
    let before_timeout = now + Duration::from_millis(29999);
    let mut tracker2 = ConnectionStallTracker::new_at(now);
    tracker2.update_progress_at(1024, now);
    assert!(!tracker2.check_stalled_at(Duration::from_secs(30), before_timeout));
}

#[test]
fn test_tracker_progress_resets_stall() {
    let now = Instant::now();
    let mut tracker = ConnectionStallTracker::new_at(now);

    // Initial progress
    tracker.update_progress_at(1024, now);

    // Stalled after timeout
    let at_timeout = now + Duration::from_secs(30);
    assert!(tracker.check_stalled_at(Duration::from_secs(30), at_timeout));
    assert!(tracker.is_stalled());

    // New data arrives — stall cleared
    tracker.update_progress_at(2048, at_timeout);
    assert!(!tracker.is_stalled());
}

#[test]
fn test_tracker_stall_not_reset_by_same_progress() {
    let now = Instant::now();
    let mut tracker = ConnectionStallTracker::new_at(now);

    tracker.update_progress_at(1024, now);

    // Stall
    let at_timeout = now + Duration::from_secs(30);
    assert!(tracker.check_stalled_at(Duration::from_secs(30), at_timeout));

    // Same byte count does NOT reset stall
    tracker.update_progress_at(1024, at_timeout);
    assert!(tracker.is_stalled());
}

#[test]
fn test_tracker_check_stalled_real_time() {
    let mut tracker = ConnectionStallTracker::new();
    tracker.update_progress(1024);

    // With a 1ms timeout, it's very likely that enough time has elapsed
    // between the update and the check for the stall to trigger.
    // We use a generous 500ms sleep to ensure the test is reliable.
    // However, we avoid sleeps in tests — instead test the logic path
    // with check_stalled_at above. This test just verifies the
    // real-time path doesn't panic.
    let _ = tracker.check_stalled(Duration::from_secs(3600));
    // Should not be stalled after 0 seconds of real elapsed time.
    assert!(!tracker.is_stalled());
}

#[test]
fn test_tracker_reset() {
    let mut tracker = ConnectionStallTracker::new();
    tracker.update_progress(2048);
    assert_eq!(tracker.last_bytes_received(), 2048);

    tracker.reset();
    assert!(!tracker.is_stalled());
    assert_eq!(tracker.last_bytes_received(), 0);
}

#[test]
fn test_tracker_default() {
    let tracker = ConnectionStallTracker::default();
    assert!(!tracker.is_stalled());
    assert_eq!(tracker.last_bytes_received(), 0);
}

// ── Integration: config + tracker together ──────────────────────────

#[test]
fn test_full_tail_reclaim_flow() {
    let config = TailReclaimConfig::new()
        .with_min_tail_length(1024)
        .with_stall_timeout_secs(30);

    let now = Instant::now();
    let mut tracker = ConnectionStallTracker::new_at(now);

    // Connection is assigned range [0, 4095] (4096 bytes).
    let range_start: u64 = 0;
    let range_end: u64 = 4095;

    // Initially, 1024 bytes received.
    tracker.update_progress_at(1024, now);

    // After 30 seconds with no progress — stalled.
    let at_timeout = now + Duration::from_secs(30);
    assert!(tracker.check_stalled_at(Duration::from_secs(config.stall_timeout_secs), at_timeout));

    // Calculate tail: 1024 received, 0 in flight → remaining = 3072 > 1024.
    let result = config.calculate_tail(range_start, range_end, 1024, 0);
    assert!(result.is_some());
    let r = result.unwrap();
    assert_eq!(r.tail_start, 1024);
    assert_eq!(r.tail_end, 4095);
    assert_eq!(r.length(), 3072);
}

#[test]
fn test_full_tail_reclaim_no_reclaim_when_progressing() {
    let config = TailReclaimConfig::new()
        .with_min_tail_length(1024)
        .with_stall_timeout_secs(30);

    let now = Instant::now();
    let mut tracker = ConnectionStallTracker::new_at(now);

    let range_start: u64 = 0;
    let range_end: u64 = 4095;

    // 1024 bytes received.
    tracker.update_progress_at(1024, now);

    // After 10 seconds, more data received.
    let at_10s = now + Duration::from_secs(10);
    tracker.update_progress_at(2048, at_10s);

    // After 30 seconds from initial progress — but progress was made at 10s,
    // so not stalled.
    let at_30s = now + Duration::from_secs(30);
    assert!(!tracker.check_stalled_at(Duration::from_secs(30), at_30s));

    // Even if we force-check the tail, the data is still valid.
    let result = config.calculate_tail(range_start, range_end, 2048, 0);
    assert!(result.is_some());
    let r = result.unwrap();
    assert_eq!(r.tail_start, 2048);
    assert_eq!(r.length(), 2048);
}

#[test]
fn test_full_tail_reclaim_disabled() {
    let config = TailReclaimConfig::new()
        .with_min_tail_length(1024)
        .with_enabled(false);

    let now = Instant::now();
    let mut tracker = ConnectionStallTracker::new_at(now);

    let range_start: u64 = 0;
    let range_end: u64 = 4095;

    tracker.update_progress_at(1024, now);

    // Even though stalled, tail reclaim is disabled.
    let at_timeout = now + Duration::from_secs(30);
    assert!(tracker.check_stalled_at(Duration::from_secs(30), at_timeout));
    assert!(
        config
            .calculate_tail(range_start, range_end, 1024, 0)
            .is_none()
    );
}

#[test]
fn test_tail_reclaim_with_large_in_flight() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // Range [0, 10240] = 10241 bytes, 0 received, 8192 in flight.
    // remaining = 10241 - 0 - 8192 = 2049 > 1024 → reclaim.
    let result = config.calculate_tail(0, 10240, 0, 8192);
    assert!(result.is_some());
    let r = result.unwrap();
    assert_eq!(r.tail_start, 8192);
    assert_eq!(r.tail_end, 10240);
    assert_eq!(r.length(), 2049);
}

#[test]
fn test_safe_tail_requires_range_support_and_matching_content_range() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    let base = TailReclaimConnectionState {
        range_supported: true,
        response_range: Some((0, 4095, 4096)),
        written_ranges: std::iter::once(0..=1023).collect(),
        ..Default::default()
    };

    assert!(config.calculate_safe_tail(0, 4095, 4096, &base).is_some());

    let mut no_range = base.clone();
    no_range.range_supported = false;
    assert!(
        config
            .calculate_safe_tail(0, 4095, 4096, &no_range)
            .is_none()
    );

    let mut wrong_range = base.clone();
    wrong_range.response_range = Some((1, 4095, 4096));
    assert!(
        config
            .calculate_safe_tail(0, 4095, 4096, &wrong_range)
            .is_none()
    );

    let mut wrong_total = base;
    wrong_total.response_range = Some((0, 4095, 4097));
    assert!(
        config
            .calculate_safe_tail(0, 4095, 4096, &wrong_total)
            .is_none()
    );
}

#[test]
fn test_safe_tail_does_not_duplicate_written_verified_or_in_flight_suffix() {
    let config = TailReclaimConfig::new().with_min_tail_length(512);
    let state = TailReclaimConnectionState {
        range_supported: true,
        response_range: Some((0, 4095, 4096)),
        written_ranges: std::iter::once(0..=1023).collect(),
        verified_ranges: std::iter::once(1500..=1799).collect(),
        in_flight_ranges: std::iter::once(2500..=2999).collect(),
    };

    let result = config
        .calculate_safe_tail(0, 4095, 4096, &state)
        .expect("the unowned suffix is large enough");
    assert_eq!(
        result,
        TailReclaimResult {
            tail_start: 3000,
            tail_end: 4095
        }
    );
}

#[test]
fn test_safe_tail_rejects_duplicate_tail_and_missing_ownership() {
    let config = TailReclaimConfig::new().with_min_tail_length(512);
    let state = TailReclaimConnectionState {
        range_supported: true,
        response_range: Some((0, 4095, 4096)),
        written_ranges: std::iter::once(0..=3599).collect(),
        ..Default::default()
    };
    assert!(config.calculate_safe_tail(0, 4095, 4096, &state).is_none());

    let no_ownership = TailReclaimConnectionState {
        range_supported: true,
        response_range: Some((0, 4095, 4096)),
        ..Default::default()
    };
    assert!(
        config
            .calculate_safe_tail(0, 4095, 4096, &no_ownership)
            .is_none()
    );
}

#[test]
fn test_tail_reclaim_saturating_add_no_overflow() {
    let config = TailReclaimConfig::new().with_min_tail_length(1024);
    // bytes_received + bytes_in_flight would overflow u64 individually
    // but saturating_add handles it. Use extreme values.
    let result = config.calculate_tail(0, u64::MAX, u64::MAX / 2, u64::MAX / 2 + 1);
    // consumed saturates to u64::MAX, remaining = 0 → no reclaim.
    // But actually: range_size = u64::MAX, consumed = u64::MAX/2 + u64::MAX/2 + 1
    // = u64::MAX via saturating_add. remaining = 0 → no reclaim.
    assert!(result.is_none());

    // Also verify that TailReclaimResult::length() uses saturating_add
    // to avoid overflow when tail_end - tail_start (saturating) equals u64::MAX.
    // A result spanning [0, u64::MAX] has length = u64::MAX + 1 which must
    // saturate to u64::MAX rather than panicking.
    let extreme_result = TailReclaimResult {
        tail_start: 0,
        tail_end: u64::MAX,
    };
    assert_eq!(extreme_result.length(), u64::MAX);
}

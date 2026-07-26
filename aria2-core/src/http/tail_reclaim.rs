//! HTTP tail reclaim policy for stalled download recovery.
//!
//! When an HTTP download connection stalls (no data received for a configurable
//! timeout), this module calculates whether the tail portion of the connection's
//! assigned range can be reclaimed and reassigned to a new connection.
//!
//! This matches the C++ aria2 behavior where `HttpRequest::tailRequestEnabled_`
//! and related logic detects stalled connections and splits the remaining range
//! to allow parallel download completion.
//!
//! # Tail Reclaim Algorithm
//!
//! 1. Track bytes received per connection over time
//! 2. When a connection has not received data for `stall_timeout` seconds:
//!    a. Calculate the remaining unrequested bytes in the connection's range
//!    b. If remaining > `min_tail_length`, split the tail off
//!    c. The original connection keeps its in-flight requests
//!    d. A new connection can pick up the tail portion
//! 3. The tail length is calculated as:
//!    `remaining = end - (start + bytes_received + bytes_in_flight)`
//!    If remaining > min_tail_length, the tail starts at
//!    `start + bytes_received + bytes_in_flight`
//!
//! # Relationship to engine::http_tail_reclaim
//!
//! The `engine::http_tail_reclaim` module makes the *global* decision of whether
//! the download as a whole should reclaim its HTTP tail segment (considering
//! protocol, p2p involvement, concurrent command counts, etc.).
//!
//! This module operates at the *per-connection* level: it tracks whether an
//! individual connection has stalled and computes the exact byte range to
//! split off as the tail. The two modules are complementary — the engine-level
//! policy decides *when* to consider reclaiming, while this module decides
//! *what* to reclaim from a specific connection.

use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Default constants (match C++ aria2 PREF_HTTP_TAIL_RECLAIM_* settings)
// ---------------------------------------------------------------------------

/// Default minimum remaining bytes to trigger tail reclaim (1 MiB).
pub const DEFAULT_MIN_TAIL_LENGTH: u64 = 1024 * 1024;

/// Default stall timeout in seconds before reclaiming.
pub const DEFAULT_STALL_TIMEOUT_SECS: u64 = 30;

/// Default enabled state for tail reclaim.
pub const DEFAULT_TAIL_RECLAIM_ENABLED: bool = true;

// ---------------------------------------------------------------------------
// TailReclaimConfig
// ---------------------------------------------------------------------------

/// Configuration for tail reclaim behavior.
///
/// Matches C++ `PREF_HTTP_TAIL_RECLAIM_*` settings that control when the tail
/// portion of a stalled connection's range is split off for a new connection
/// to download.
#[derive(Debug, Clone)]
pub struct TailReclaimConfig {
    /// Minimum remaining bytes to trigger tail reclaim.
    ///
    /// If the remaining unrequested bytes are less than this value, no reclaim
    /// occurs — the overhead of starting a new connection outweighs the benefit.
    /// Default: 1 MiB (`DEFAULT_MIN_TAIL_LENGTH`).
    pub min_tail_length: u64,

    /// Stall timeout in seconds before reclaiming.
    ///
    /// A connection must have zero throughput for this duration before its tail
    /// is considered for reclaim.
    /// Default: 30 (`DEFAULT_STALL_TIMEOUT_SECS`).
    pub stall_timeout_secs: u64,

    /// Whether tail reclaim is enabled.
    /// Default: true (`DEFAULT_TAIL_RECLAIM_ENABLED`).
    pub enabled: bool,
}

impl Default for TailReclaimConfig {
    fn default() -> Self {
        Self {
            min_tail_length: DEFAULT_MIN_TAIL_LENGTH,
            stall_timeout_secs: DEFAULT_STALL_TIMEOUT_SECS,
            enabled: DEFAULT_TAIL_RECLAIM_ENABLED,
        }
    }
}

impl TailReclaimConfig {
    /// Create a new config with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the minimum tail length threshold.
    pub fn with_min_tail_length(mut self, min_tail_length: u64) -> Self {
        self.min_tail_length = min_tail_length;
        self
    }

    /// Set the stall timeout in seconds.
    pub fn with_stall_timeout_secs(mut self, secs: u64) -> Self {
        self.stall_timeout_secs = secs;
        self
    }

    /// Enable or disable tail reclaim.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Determine whether tail reclaim is applicable for the given range.
    ///
    /// Returns `true` when:
    /// - Tail reclaim is enabled in the config, AND
    /// - The remaining unrequested bytes exceed `min_tail_length`
    ///
    /// The remaining bytes are calculated as:
    /// ```text
    /// remaining = range_end - range_start + 1 - bytes_received - bytes_in_flight
    /// ```
    ///
    /// # Arguments
    ///
    /// * `range_start` - Start of the connection's assigned byte range (inclusive).
    /// * `range_end` - End of the connection's assigned byte range (inclusive).
    /// * `bytes_received` - Bytes already received and confirmed written.
    /// * `bytes_in_flight` - Bytes requested but not yet confirmed (in-flight).
    pub fn should_reclaim(
        &self,
        range_start: u64,
        range_end: u64,
        bytes_received: u64,
        bytes_in_flight: u64,
    ) -> bool {
        if !self.enabled {
            return false;
        }

        // Total bytes in the assigned range.
        // Use saturating_add to avoid overflow when diff == u64::MAX.
        let range_size = match range_end.checked_sub(range_start) {
            Some(diff) => diff.saturating_add(1), // inclusive range
            None => return false,                 // range_end < range_start: invalid range
        };

        // Bytes already consumed (received + in-flight).
        let consumed = bytes_received.saturating_add(bytes_in_flight);

        // Remaining unrequested bytes.
        let remaining = range_size.saturating_sub(consumed);

        remaining > self.min_tail_length
    }

    /// Calculate the exact tail range to reclaim.
    ///
    /// Returns `Some(TailReclaimResult)` when the remaining unrequested bytes
    /// exceed `min_tail_length`, or `None` otherwise.
    ///
    /// The tail range begins at `range_start + bytes_received + bytes_in_flight`
    /// and extends to `range_end` (both inclusive).
    ///
    /// # Arguments
    ///
    /// Same as [`should_reclaim`].
    ///
    /// # Invariants
    ///
    /// - The returned `tail_start` is always > `range_start` (i.e. at least one
    ///   byte is retained by the original connection).
    /// - The returned `tail_end` equals `range_end`.
    /// - The tail length (`tail_end - tail_start + 1`) is always >
    ///   `min_tail_length`.
    pub fn calculate_tail(
        &self,
        range_start: u64,
        range_end: u64,
        bytes_received: u64,
        bytes_in_flight: u64,
    ) -> Option<TailReclaimResult> {
        if !self.should_reclaim(range_start, range_end, bytes_received, bytes_in_flight) {
            return None;
        }

        let tail_start = range_start
            .saturating_add(bytes_received)
            .saturating_add(bytes_in_flight);

        // tail_start must be <= range_end and > range_start for a valid split.
        // The should_reclaim check already guarantees remaining > min_tail_length > 0,
        // which implies tail_start <= range_end and tail_start > range_start
        // (because consumed < range_size).
        if tail_start > range_end || tail_start <= range_start {
            return None;
        }

        Some(TailReclaimResult {
            tail_start,
            tail_end: range_end,
        })
    }
}

// ---------------------------------------------------------------------------
// TailReclaimResult
// ---------------------------------------------------------------------------

/// Result of a tail reclaim calculation.
///
/// Describes the byte range that should be split off from a stalled connection
/// and assigned to a new connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailReclaimResult {
    /// Start of the reclaimed tail range (inclusive).
    pub tail_start: u64,

    /// End of the reclaimed tail range (inclusive).
    pub tail_end: u64,
}

impl TailReclaimResult {
    /// Length of the reclaimed tail range in bytes.
    ///
    /// For a valid inclusive range `[tail_start, tail_end]`, the length is
    /// `tail_end - tail_start + 1`.
    pub fn length(&self) -> u64 {
        self.tail_end
            .saturating_sub(self.tail_start)
            .saturating_add(1)
    }
}

// ---------------------------------------------------------------------------
// ConnectionStallTracker
// ---------------------------------------------------------------------------

/// Per-connection stall tracker.
///
/// Tracks throughput on a single HTTP download connection to detect when the
/// connection has stalled (no data received for a configurable timeout).
///
/// # Usage
///
/// ```ignore
/// use aria2_core::http::tail_reclaim::ConnectionStallTracker;
/// use std::time::Duration;
///
/// let mut tracker = ConnectionStallTracker::new();
///
/// // Call on each data received event to reset the stall timer.
/// tracker.update_progress(1024);
///
/// // Check whether the connection is stalled.
/// if tracker.check_stalled(Duration::from_secs(30)) {
///     // Consider reclaiming the tail of this connection's range.
/// }
/// ```
pub struct ConnectionStallTracker {
    /// Timestamp of the last time data was received.
    last_progress_time: Instant,

    /// Bytes received at the last progress check.
    last_bytes_received: u64,

    /// Whether the connection is currently considered stalled.
    ///
    /// Set to `true` when `check_stalled` detects a timeout, reset to `false`
    /// when `update_progress` detects new data.
    stalled: bool,
}

impl ConnectionStallTracker {
    /// Create a new stall tracker starting from the current instant.
    ///
    /// The tracker starts with zero bytes received and is not stalled.
    pub fn new() -> Self {
        Self {
            last_progress_time: Instant::now(),
            last_bytes_received: 0,
            stalled: false,
        }
    }

    /// Create a stall tracker with a specific start time (for testing).
    pub fn new_at(now: Instant) -> Self {
        Self {
            last_progress_time: now,
            last_bytes_received: 0,
            stalled: false,
        }
    }

    /// Update the progress counter with the total bytes received so far.
    ///
    /// If `bytes_received` has increased since the last call, the stall timer
    /// is reset and the `stalled` flag is cleared. If `bytes_received` is
    /// unchanged, no action is taken — the stall timer continues running.
    ///
    /// # Arguments
    ///
    /// * `bytes_received` - Cumulative bytes received on this connection.
    pub fn update_progress(&mut self, bytes_received: u64) {
        if bytes_received > self.last_bytes_received {
            self.last_progress_time = Instant::now();
            self.last_bytes_received = bytes_received;
            self.stalled = false;
        }
    }

    /// Update the progress counter with a specific timestamp (for testing).
    ///
    /// Same semantics as `update_progress`, but uses the provided `Instant`
    /// instead of `Instant::now()`.
    pub fn update_progress_at(&mut self, bytes_received: u64, now: Instant) {
        if bytes_received > self.last_bytes_received {
            self.last_progress_time = now;
            self.last_bytes_received = bytes_received;
            self.stalled = false;
        }
    }

    /// Check whether the connection has stalled.
    ///
    /// A connection is considered stalled when no data has been received for
    /// at least `timeout`. This method updates the internal `stalled` flag.
    ///
    /// # Arguments
    ///
    /// * `timeout` - Duration without progress after which the connection is
    ///   considered stalled.
    ///
    /// # Returns
    ///
    /// `true` if the connection has stalled (no progress for >= `timeout`).
    pub fn check_stalled(&mut self, timeout: Duration) -> bool {
        let elapsed = self.last_progress_time.elapsed();
        self.stalled = elapsed >= timeout;
        self.stalled
    }

    /// Check whether the connection has stalled using a specific timestamp
    /// (for testing).
    pub fn check_stalled_at(&mut self, timeout: Duration, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last_progress_time);
        self.stalled = elapsed >= timeout;
        self.stalled
    }

    /// Query the current stall state without re-evaluating the timeout.
    ///
    /// Returns the value of the `stalled` flag as set by the most recent call
    /// to `check_stalled` or `update_progress`.
    pub fn is_stalled(&self) -> bool {
        self.stalled
    }

    /// Get the timestamp of the last progress update.
    pub fn last_progress_time(&self) -> Instant {
        self.last_progress_time
    }

    /// Get the bytes received at the last progress check.
    pub fn last_bytes_received(&self) -> u64 {
        self.last_bytes_received
    }

    /// Reset the tracker to its initial state.
    pub fn reset(&mut self) {
        self.last_progress_time = Instant::now();
        self.last_bytes_received = 0;
        self.stalled = false;
    }
}

impl Default for ConnectionStallTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
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
        assert!(
            tracker.check_stalled_at(Duration::from_secs(config.stall_timeout_secs), at_timeout)
        );

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
}

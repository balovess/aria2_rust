//! Per-connection stall tracking for tail reclaim.
//!
//! This module provides the [`ConnectionStallTracker`] which monitors throughput
//! on a single HTTP download connection and detects when it has stalled (no data
//! received for a configurable timeout).

use std::time::{Duration, Instant};

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

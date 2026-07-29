//! Tail reclaim configuration and result types.
//!
//! This module contains the configuration struct that controls when and how tail
//! reclaim is triggered, the result type describing a reclaimed range, and the
//! associated default constants.

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

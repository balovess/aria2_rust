//! Types and constants for file entry management.

use std::time::Duration;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Startup idle time before considering faster-server replacements.
/// Matches C++ aria2's 10-second startup idle window.
pub(super) const STARTUP_IDLE_TIME: Duration = Duration::from_secs(10);

/// Speed threshold (20 KB/s) for server-stat-based faster-server detection.
pub(super) const SPEED_THRESHOLD: u64 = 20_000;

/// Maximum number of URIs to scan in server-stat-based faster-server search.
pub(super) const NUM_URI_SCAN: usize = 10;

// ---------------------------------------------------------------------------
// UriResult — result of a URI attempt
// ---------------------------------------------------------------------------

/// Records the outcome of attempting to download from a URI.
///
/// Equivalent to C++ aria2's `URIResult` struct. The `result_code` follows
/// aria2's error_code values (e.g., 1 = OK, 2 = UNRESOLVED_HOST, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UriResult {
    /// The URI that was attempted.
    pub uri: String,
    /// Error/result code (aria2 error_code::Value).
    pub result_code: u16,
}

impl UriResult {
    /// Create a new `UriResult`.
    pub fn new(uri: String, result_code: u16) -> Self {
        Self { uri, result_code }
    }
}

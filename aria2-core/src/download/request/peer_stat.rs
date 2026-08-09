//! Per-connection download statistics (peer speed tracker).

// ---------------------------------------------------------------------------
// PeerStat — lightweight per-connection download statistics
// ---------------------------------------------------------------------------

/// Per-connection download speed tracker for a single peer/server.
///
/// Created from the **original** URI's host and protocol (not the redirected
/// one), because the URI selector picks mirrors based on the original URI.
#[derive(Debug, Clone)]
pub struct PeerStat {
    /// Connection identifier (0 until assigned by the engine).
    pub cuid: u64,
    /// Current instantaneous download speed (bytes/sec).
    pub download_speed: u64,
    /// Exponential moving average download speed (bytes/sec).
    pub avg_download_speed: u64,
    /// Bytes downloaded in the current session.
    pub session_download_length: u64,
    /// Server hostname (from original URI).
    pub hostname: String,
    /// Protocol scheme, e.g. "http", "https", "ftp" (from original URI).
    pub protocol: String,
}

impl PeerStat {
    /// Create a new `PeerStat` for the given host and protocol.
    pub fn new(cuid: u64, hostname: String, protocol: String) -> Self {
        Self {
            cuid,
            download_speed: 0,
            avg_download_speed: 0,
            session_download_length: 0,
            hostname,
            protocol,
        }
    }

    /// Add `length` bytes to the session download counter (saturating).
    pub fn add_session_download_length(&mut self, length: u64) {
        self.session_download_length = self.session_download_length.saturating_add(length);
    }

    /// Return the average download speed (bytes/sec).
    pub fn avg_download_speed(&self) -> u64 {
        self.avg_download_speed
    }
}

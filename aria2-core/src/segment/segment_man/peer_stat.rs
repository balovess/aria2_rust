//! Lightweight per-connection download statistics.

/// Status of a peer/connection for download speed tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStatus {
    /// Actively downloading data
    Active,
    /// Not currently downloading (idle)
    Idle,
}

/// Lightweight per-connection download statistics.
///
/// This is a simplified version of the full `PeerStats` from the engine
/// module. `SegmentMan` needs its own tracking for:
/// - Looking up peer status by CUID (for `get_clean_segment_if_owner_is_idle`)
/// - Tracking the fastest peer per server (for connection optimization)
#[derive(Debug, Clone)]
pub struct PeerStat {
    /// Connection ID
    pub cuid: u64,
    /// Current download speed in bytes/sec
    pub download_speed: u64,
    /// Average download speed in bytes/sec
    pub avg_download_speed: u64,
    /// Session download length in bytes
    pub session_download_length: u64,
    /// Server hostname
    pub hostname: String,
    /// Protocol (e.g., "http", "https", "ftp")
    pub protocol: String,
    /// Current status (active or idle)
    pub status: PeerStatus,
}

impl PeerStat {
    /// Creates a new `PeerStat` with the given CUID, hostname, and protocol.
    pub fn new(cuid: u64, hostname: String, protocol: String) -> Self {
        PeerStat {
            cuid,
            download_speed: 0,
            avg_download_speed: 0,
            session_download_length: 0,
            hostname,
            protocol,
            status: PeerStatus::Idle,
        }
    }

    /// Adds `length` bytes to the session download counter.
    pub fn add_session_download_length(&mut self, length: u64) {
        self.session_download_length = self.session_download_length.saturating_add(length);
    }
}

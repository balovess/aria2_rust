use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Threshold for banning peers that send too many invalid pieces.
///
/// When a peer sends `BAD_DATA_THRESHOLD` or more pieces with invalid hashes,
/// they are permanently banned for the remainder of the session.
pub const BAD_DATA_THRESHOLD: u32 = 3;

/// Represents an active upload session with a peer.
///
/// Tracks upload statistics and session state for a single peer connection
/// during the seeding phase.
#[derive(Debug, Clone)]
pub struct UploadSession {
    /// Peer's socket address
    pub peer_addr: SocketAddr,
    /// Bytes uploaded to this peer in the current session
    pub uploaded_bytes: u64,
    /// Upload speed in bytes/sec (rolling average)
    pub upload_speed: u64,
    /// Last time data was uploaded to this peer
    pub last_upload_time: Instant,
    /// Whether this session is active
    pub is_active: bool,
}

impl UploadSession {
    /// Create a new upload session for a peer.
    pub fn new(peer_addr: SocketAddr) -> Self {
        Self {
            peer_addr,
            uploaded_bytes: 0,
            upload_speed: 0,
            last_upload_time: Instant::now(),
            is_active: true,
        }
    }

    /// Record bytes uploaded to this peer.
    pub fn record_upload(&mut self, bytes: u64) {
        self.uploaded_bytes += bytes;
        self.last_upload_time = Instant::now();
    }

    /// Update the upload speed (rolling average).
    pub fn update_speed(&mut self, speed: u64) {
        self.upload_speed = speed;
    }

    /// Mark this session as inactive.
    pub fn deactivate(&mut self) {
        self.is_active = false;
    }
}

#[derive(Debug, Clone, Default)]
pub struct SeedExitCondition {
    pub seed_time: Option<Duration>,
    pub seed_ratio: Option<f64>,
}

impl SeedExitCondition {
    pub fn infinite() -> Self {
        Self {
            seed_time: None,
            seed_ratio: None,
        }
    }

    pub fn with_time(secs: u64) -> Self {
        if secs == 0 {
            Self::infinite()
        } else {
            Self {
                seed_time: Some(Duration::from_secs(secs)),
                seed_ratio: None,
            }
        }
    }

    pub fn with_ratio(ratio: f64) -> Self {
        if ratio <= 0
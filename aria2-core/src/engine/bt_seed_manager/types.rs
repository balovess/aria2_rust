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
        if ratio <= 0.0 {
            Self::infinite()
        } else {
            Self {
                seed_time: None,
                seed_ratio: Some(ratio),
            }
        }
    }

    pub fn with_time_and_ratio(secs: u64, ratio: f64) -> Self {
        let time = if secs == 0 {
            None
        } else {
            Some(Duration::from_secs(secs))
        };
        let r = if ratio <= 0.0 { None } else { Some(ratio) };
        Self {
            seed_time: time,
            seed_ratio: r,
        }
    }

    /// Check if the seed ratio condition has been met.
    ///
    /// Returns `true` when seeding should stop based on upload/download ratio:
    /// - If `seed_ratio <= 0.0`, returns `false` (infinite seeding)
    /// - If `downloaded == 0`, returns `false` (nothing downloaded yet)
    /// - Otherwise, checks if `uploaded / downloaded >= seed_ratio`
    ///
    /// # Arguments
    ///
    /// * `uploaded` - Total bytes uploaded during this session
    /// * `downloaded` - Total bytes downloaded during this session
    /// * `seed_ratio` - Target ratio (e.g., 1.0 means 1:1 upload:download)
    pub fn check_seed_condition(uploaded: u64, downloaded: u64, seed_ratio: f64) -> bool {
        if seed_ratio <= 0.0 {
            return false; // Infinite seeding (ratio 0 or negative)
        }
        if downloaded == 0 {
            return false; // Nothing downloaded yet, can't compute ratio
        }
        (uploaded as f64 / downloaded as f64) >= seed_ratio
    }

    /// Check if the seed time condition has been met.
    ///
    /// Returns `true` when pure seeding duration exceeds the limit:
    /// - A zero duration is immediately met; callers represent infinite
    ///   seeding by omitting the time criterion (`None`)
    /// - If not in pure seeding phase (`is_pure_seeding == false`), returns `false`
    /// - Otherwise, checks if elapsed seconds since `seeding_started_at >= seed_time_secs`
    ///
    /// # Arguments
    ///
    /// * `seeding_started_at` - Instant when pure seeding phase began
    /// * `seed_time_secs` - Maximum allowed seeding duration in seconds
    /// * `is_pure_seeding` - Whether all pieces are complete (true = in seeding phase)
    pub fn check_seed_time(
        seeding_started_at: Instant,
        seed_time_secs: u64,
        is_pure_seeding: bool,
    ) -> bool {
        if !is_pure_seeding {
            return false; // Still downloading, haven't entered pure seeding yet
        }
        seeding_started_at.elapsed().as_secs() >= seed_time_secs
    }
}

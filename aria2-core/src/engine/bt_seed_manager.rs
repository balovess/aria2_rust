use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, Result};

use super::bt_upload_session::{BtSeedingConfig, BtUploadSession, PieceDataProvider};
use super::choking_algorithm::{ChokeAction, ChokingAlgorithm};

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
    /// - If `seed_time_secs == 0`, returns `false` (infinite seeding)
    /// - If not in pure seeding phase (`is_pure_seeding == false`), returns `false`
    /// - Otherwise, checks if elapsed seconds since `seeding_started_at >= seed_time_secs`
    ///
    /// # Arguments
    ///
    /// * `seeding_started_at` - Instant when pure seeding phase began
    /// * `seed_time_secs` - Maximum allowed seeding duration in seconds (0 = infinite)
    /// * `is_pure_seeding` - Whether all pieces are complete (true = in seeding phase)
    pub fn check_seed_time(
        seeding_started_at: Instant,
        seed_time_secs: u64,
        is_pure_seeding: bool,
    ) -> bool {
        if seed_time_secs == 0 {
            return false; // Infinite seeding
        }
        if !is_pure_seeding {
            return false; // Still downloading, haven't entered pure seeding yet
        }
        seeding_started_at.elapsed().as_secs() >= seed_time_secs
    }
}

pub struct BtSeedManager {
    /// Info hash of the torrent being seeded
    info_hash: [u8; 20],
    sessions: Vec<BtUploadSession>,
    piece_data: Arc<dyn PieceDataProvider>,
    config: BtSeedingConfig,
    exit_condition: SeedExitCondition,
    pub total_uploaded: u64,
    total_downloaded: u64,
    pub seeding_start_time: Instant,
    last_optimistic_unchoke: Instant,
    optimistic_round: usize,
    /// Choking algorithm for tit-for-tat peer selection during seeding.
    /// When present, drives intelligent choke/unchoke decisions every rotation interval.
    pub choking_algo: Option<ChokingAlgorithm>,
    
    // Upload statistics (atomic for thread-safe access)
    /// Total uploaded bytes (atomic for concurrent access)
    uploaded_bytes_atomic: AtomicU64,
    /// Current upload speed in bytes/sec
    upload_speed_atomic: AtomicU64,
    /// Last upload timestamp
    last_upload_time: std::sync::Mutex<Instant>,
    
    // Seeding control
    /// Target seed ratio (upload/download)
    seed_ratio: f64,
    /// Target seed duration
    seed_time: Duration,
    
    // Active upload tracking
    /// Map of peer address to upload session
    active_uploads: HashMap<SocketAddr, UploadSession>,
    /// Maximum number of concurrent uploads
    max_uploads: usize,
    
    // Upload speed limiting
    /// Maximum upload speed in bytes/sec (None = unlimited)
    max_upload_speed: Option<u64>,
    /// Timestamp of last speed throttle check
    last_throttle_check: std::sync::Mutex<Instant>,
    /// Bytes uploaded in current throttle window
    throttle_window_bytes: AtomicU64,
}

impl BtSeedManager {
    /// Create a new BtSeedManager with default settings.
    ///
    /// # Arguments
    ///
    /// * `connections` - Peer connections to use for seeding
    /// * `piece_data` - Provider for piece data
    /// * `config` - Seeding configuration
    /// * `exit_condition` - Conditions for exiting seeding
    /// * `total_downloaded` - Total bytes downloaded (for ratio calculation)
    pub fn new(
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_data: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
    ) -> Self {
        Self::new_with_choking_algo(
            connections,
            piece_data,
            config,
            exit_condition,
            total_downloaded,
            None,
        )
    }

    /// Create a new BtSeedManager with an optional ChokingAlgorithm.
    ///
    /// When `choking_algo` is `Some`, the seeding loop will call
    /// [`ChokingAlgorithm::rotate_choke`] every `config.choke_rotation_interval_secs`
    /// and apply the resulting choke/unchoke actions to sessions.
    pub fn new_with_choking_algo(
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_data: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        choking_algo: Option<ChokingAlgorithm>,
    ) -> Self {
        let sessions = connections
            .into_iter()
            .map(|conn| BtUploadSession::new(conn, &config))
            .collect();

        let seed_ratio = exit_condition.seed_ratio.unwrap_or(0.0);
        let seed_time = exit_condition.seed_time.unwrap_or(Duration::ZERO);
        let max_uploads = config.max_peers_to_unchoke;
        let max_upload_speed = config.max_upload_bytes_per_sec;

        Self {
            info_hash: [0u8; 20], // Default, can be set via builder
            sessions,
            piece_data,
            config,
            exit_condition,
            total_uploaded: 0,
            total_downloaded,
            seeding_start_time: Instant::now(),
            last_optimistic_unchoke: Instant::now(),
            optimistic_round: 0,
            choking_algo,
            uploaded_bytes_atomic: AtomicU64::new(0),
            upload_speed_atomic: AtomicU64::new(0),
            last_upload_time: std::sync::Mutex::new(Instant::now()),
            seed_ratio,
            seed_time,
            active_uploads: HashMap::new(),
            max_uploads,
            max_upload_speed,
            last_throttle_check: std::sync::Mutex::new(Instant::now()),
            throttle_window_bytes: AtomicU64::new(0),
        }
    }

    /// Create a new BtSeedManager with explicit info_hash.
    ///
    /// This is the preferred constructor when the info_hash is known.
    pub fn new_with_info_hash(
        info_hash: [u8; 20],
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_data: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
    ) -> Self {
        let mut manager = Self::new(connections, piece_data, config, exit_condition, total_downloaded);
        manager.info_hash = info_hash;
        manager
    }

    /// Handle a piece request from a peer.
    ///
    /// This method reads the requested piece data from the piece provider
    /// and returns it for sending to the peer.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer's socket address
    /// * `index` - The piece index
    /// * `begin` - The offset within the piece
    /// * `length` - The length of data to read
    ///
    /// # Returns
    ///
    /// The requested piece data, or an error if the piece is not available.
    pub async fn handle_piece_request(
        &mut self,
        peer: SocketAddr,
        index: u32,
        begin: u32,
        length: u32,
    ) -> Result<Vec<u8>> {
        // Apply upload speed throttling if configured
        if let Some(max_speed) = self.max_upload_speed {
            self.throttle_upload(max_speed, length as u64).await?;
        }

        // Get piece data from provider
        let piece_data = self
            .piece_data
            .get_piece_data(index, begin, length)
            .ok_or_else(|| {
                Aria2Error::Recoverable(crate::error::RecoverableError::InvalidPieceIndex {
                    index,
                    max_index: self.piece_data.num_pieces(),
                })
            })?;

        // Update upload statistics
        self.uploaded_bytes_atomic
            .fetch_add(piece_data.len() as u64, Ordering::Relaxed);
        self.total_uploaded += piece_data.len() as u64;

        // Update session tracking
        if let Some(session) = self.active_uploads.get_mut(&peer) {
            session.record_upload(piece_data.len() as u64);
        } else {
            let mut session = UploadSession::new(peer);
            session.record_upload(piece_data.len() as u64);
            self.active_uploads.insert(peer, session);
        }

        // Update last upload time
        if let Ok(mut last) = self.last_upload_time.lock() {
            *last = Instant::now();
        }

        debug!(
            "Handled piece request: peer={}, index={}, offset={}, len={}, total_uploaded={}",
            peer, index, begin, length, self.total_uploaded
        );

        Ok(piece_data)
    }

    /// Throttle upload speed to respect the configured maximum.
    ///
    /// Uses a token-bucket style algorithm with a 1-second window.
    /// If the current window has exceeded the speed limit, sleeps until
    /// the window resets.
    ///
    /// # Arguments
    ///
    /// * `max_speed` - Maximum upload speed in bytes/sec
    /// * `bytes_to_upload` - Number of bytes about to be uploaded
    async fn throttle_upload(&self, max_speed: u64, bytes_to_upload: u64) -> Result<()> {
        if max_speed == 0 {
            return Ok(()); // No limit
        }

        let now = Instant::now();
        let window_bytes = self.throttle_window_bytes.load(Ordering::Relaxed);

        // Check if we need to reset the window (every 1 second)
        let should_reset = if let Ok(last_check) = self.last_throttle_check.lock() {
            now.duration_since(*last_check) >= Duration::from_secs(1)
        } else {
            false
        };

        if should_reset {
            // Reset the window
            self.throttle_window_bytes.store(0, Ordering::Relaxed);
            if let Ok(mut last_check) = self.last_throttle_check.lock() {
                *last_check = now;
            }
        }

        // Check if we would exceed the limit
        if window_bytes + bytes_to_upload > max_speed {
            // Calculate how long to wait
            let bytes_over = window_bytes + bytes_to_upload - max_speed;
            let wait_secs = bytes_over as f64 / max_speed as f64;
            let wait_duration = Duration::from_secs_f64(wait_secs.min(1.0));

            debug!(
                "Throttling upload: {} bytes over limit, waiting {:?}",
                bytes_over, wait_duration
            );

            tokio::time::sleep(wait_duration).await;

            // Reset window after waiting
            self.throttle_window_bytes.store(0, Ordering::Relaxed);
            if let Ok(mut last_check) = self.last_throttle_check.lock() {
                *last_check = Instant::now();
            }
        }

        // Track the bytes we're about to upload
        self.throttle_window_bytes
            .fetch_add(bytes_to_upload, Ordering::Relaxed);

        Ok(())
    }

    /// Check if seeding should stop based on configured conditions.
    ///
    /// Returns `true` when either:
    /// - The seed ratio has been reached (uploaded >= ratio * downloaded)
    /// - The seed time has elapsed
    ///
    /// # Arguments
    ///
    /// * `downloaded_bytes` - Total bytes downloaded (for ratio calculation)
    pub fn should_stop_seeding(&self, downloaded_bytes: u64) -> bool {
        // Check seed ratio
        if self.seed_ratio > 0.0 && downloaded_bytes > 0 {
            let uploaded = self.uploaded_bytes_atomic.load(Ordering::Relaxed);
            let ratio = uploaded as f64 / downloaded_bytes as f64;
            if ratio >= self.seed_ratio {
                info!(
                    "Seed ratio reached: {:.2} >= {:.2} (uploaded={}, downloaded={})",
                    ratio, self.seed_ratio, uploaded, downloaded_bytes
                );
                return true;
            }
        }

        // Check seed time
        if self.seed_time > Duration::ZERO {
            let elapsed = self.seeding_start_time.elapsed();
            if elapsed >= self.seed_time {
                info!(
                    "Seed time reached: {:?} >= {:?}",
                    elapsed, self.seed_time
                );
                return true;
            }
        }

        false
    }

    /// Get upload statistics.
    ///
    /// Returns a tuple of (total_uploaded_bytes, current_upload_speed).
    pub fn get_upload_stats(&self) -> (u64, u64) {
        let uploaded = self.uploaded_bytes_atomic.load(Ordering::Relaxed);
        let speed = self.upload_speed_atomic.load(Ordering::Relaxed);
        (uploaded, speed)
    }

    /// Update the upload speed statistic.
    ///
    /// This should be called periodically to track the current upload rate.
    pub fn update_upload_speed(&self, speed: u64) {
        self.upload_speed_atomic.store(speed, Ordering::Relaxed);
    }

    /// Get the info hash of the torrent being seeded.
    pub fn info_hash(&self) -> &[u8; 20] {
        &self.info_hash
    }

    /// Set the info hash.
    pub fn set_info_hash(&mut self, info_hash: [u8; 20]) {
        self.info_hash = info_hash;
    }

    /// Get the number of active upload sessions.
    pub fn num_active_uploads(&self) -> usize {
        self.active_uploads.values().filter(|s| s.is_active).count()
    }

    /// Get the maximum number of concurrent uploads.
    pub fn max_uploads(&self) -> usize {
        self.max_uploads
    }

    /// Set the maximum number of concurrent uploads.
    pub fn set_max_uploads(&mut self, max: usize) {
        self.max_uploads = max;
    }

    /// Get the seed ratio target.
    pub fn seed_ratio(&self) -> f64 {
        self.seed_ratio
    }

    /// Get the seed time target.
    pub fn seed_time(&self) -> Duration {
        self.seed_time
    }

    /// Get a reference to active upload sessions.
    pub fn active_uploads(&self) -> &HashMap<SocketAddr, UploadSession> {
        &self.active_uploads
    }

    /// Get total downloaded bytes.
    pub fn total_downloaded(&self) -> u64 {
        self.total_downloaded
    }

    pub async fn run_seeding_loop(&mut self) -> Result<()> {
        info!(
            "Seeding started: {} peers, condition={:?}, choking_algo={}",
            self.sessions.len(),
            self.exit_condition,
            self.choking_algo.is_some()
        );

        for session in &mut self.sessions {
            session.unchoke_peer().await.ok();
        }

        // Determine choke rotation interval from choking_algo config or fallback
        let choke_rotation_secs = self
            .choking_algo
            .as_ref()
            .map(|c| c.config().choke_rotation_interval_secs)
            .unwrap_or(10);
        let mut choke_interval = tokio::time::interval(Duration::from_secs(choke_rotation_secs));
        // Don't let the interval accumulate missed ticks
        choke_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            // --- Choking algorithm rotation (every N seconds) ---
            if let Some(ref mut algo) = self.choking_algo {
                choke_interval.tick().await;
                let actions = algo.rotate_choke();
                for action in actions {
                    match action {
                        ChokeAction::Unchoke(idx) => {
                            if let Some(session) = self.sessions.get_mut(idx)
                                && !session.is_dead()
                                && session.is_peer_choked()
                            {
                                debug!("ChokingAlgo: Unchoke peer #{}", idx);
                                session.unchoke_peer().await.ok();
                            }
                        }
                        ChokeAction::Choke(idx) => {
                            if let Some(session) = self.sessions.get_mut(idx)
                                && !session.is_dead()
                                && !session.is_peer_choked()
                            {
                                debug!("ChokingAlgo: Choke peer #{}", idx);
                                session.choke_peer().await.ok();
                            }
                        }
                        ChokeAction::NoChange(_) => {}
                    }
                }
            }

            // --- Process incoming messages from all sessions ---
            let mut alive_sessions = Vec::new();
            for session in &mut self.sessions {
                if !session.is_dead() {
                    match session
                        .handle_incoming_messages(self.piece_data.as_ref())
                        .await
                    {
                        Ok(uploaded) => {
                            self.total_uploaded += uploaded;
                        }
                        Err(e) => {
                            warn!("Upload session error: {}", e);
                            session.is_dead = true;
                        }
                    }
                }
                if !session.is_dead() {
                    alive_sessions.push(session.uploaded_bytes());
                }
            }

            // Fallback: optimistic unchoke when no choking algorithm is configured
            if self.choking_algo.is_none() {
                self.maybe_optimistic_unchoke().await;
            }

            if self.should_exit() {
                info!(
                    "Seeding exit condition met after {:?}",
                    self.seeding_start_time.elapsed()
                );
                break;
            }

            if alive_sessions.is_empty() && !self.sessions.is_empty() {
                debug!("All upload peers disconnected");
                break;
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        for session in &mut self.sessions {
            if !session.is_dead() {
                session.choke_peer().await.ok();
            }
        }

        Ok(())
    }

    pub fn should_exit(&self) -> bool {
        let elapsed = self.seeding_start_time.elapsed();

        if let Some(max_time) = self.exit_condition.seed_time
            && elapsed >= max_time
        {
            return true;
        }

        if let Some(ratio) = self.exit_condition.seed_ratio
            && self.total_downloaded > 0
        {
            let actual_ratio = self.total_uploaded as f64 / self.total_downloaded as f64;
            if actual_ratio >= ratio {
                return true;
            }
        }

        false
    }

    async fn maybe_optimistic_unchoke(&mut self) {
        let interval = Duration::from_secs(self.config.optimistic_unchoke_interval_secs);
        if self.last_optimistic_unchoke.elapsed() < interval {
            return;
        }
        self.last_optimistic_unchoke = Instant::now();
        self.optimistic_round += 1;

        let choked_indices: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.is_dead() && s.is_peer_choked())
            .map(|(i, _)| i)
            .collect();

        if choked_indices.is_empty() {
            return;
        }

        let idx = self.optimistic_round % choked_indices.len();
        let target = choked_indices[idx];
        if let Some(session) = self.sessions.get_mut(target) {
            debug!("Optimistic unchoke peer #{}", target);
            session.unchoke_peer().await.ok();
        }
    }

    pub fn total_uploaded(&self) -> u64 {
        self.total_uploaded
    }

    pub fn seeding_duration(&self) -> Duration {
        self.seeding_start_time.elapsed()
    }

    pub fn num_alive_peers(&self) -> usize {
        self.sessions.iter().filter(|s| !s.is_dead()).count()
    }

    pub fn num_total_peers(&self) -> usize {
        self.sessions.len()
    }

    // ------------------------------------------------------------------
    // Choking algorithm integration helpers
    // ------------------------------------------------------------------

    /// Sync upload statistics from sessions into the choking algorithm.
    ///
    /// Call this periodically (e.g., after each message handling round) so
    /// the algorithm has up-to-date speed data for scoring.
    pub fn sync_choking_algo_stats(&mut self) {
        if let Some(ref mut algo) = self.choking_algo {
            for (i, session) in self.sessions.iter().enumerate() {
                if let Some(peer) = algo.get_peer_mut(i) {
                    // Update uploaded bytes from the session
                    let session_uploaded = session.uploaded_bytes();
                    if session_uploaded > peer.uploaded_bytes {
                        peer.on_data_sent(session_uploaded - peer.uploaded_bytes);
                    }
                }
            }
        }
    }

    /// Get a reference to the choking algorithm, if configured.
    pub fn choking_algo(&self) -> Option<&ChokingAlgorithm> {
        self.choking_algo.as_ref()
    }

    /// Get a mutable reference to the choking algorithm, if configured.
    pub fn choking_algo_mut(&mut self) -> Option<&mut ChokingAlgorithm> {
        self.choking_algo.as_mut()
    }

    // ------------------------------------------------------------------
    // Upload speed limit API
    // ------------------------------------------------------------------

    /// Get the maximum upload speed limit (bytes/sec).
    ///
    /// Returns `None` if unlimited.
    pub fn max_upload_speed(&self) -> Option<u64> {
        self.max_upload_speed
    }

    /// Set the maximum upload speed limit (bytes/sec).
    ///
    /// Set to `None` or `Some(0)` for unlimited.
    pub fn set_max_upload_speed(&mut self, speed: Option<u64>) {
        self.max_upload_speed = speed.filter(|&s| s > 0);
    }

    /// Get the current upload speed (bytes/sec).
    pub fn current_upload_speed(&self) -> u64 {
        self.upload_speed_atomic.load(Ordering::Relaxed)
    }

    /// Calculate and update the current upload speed.
    ///
    /// This should be called periodically (e.g., every second) to track
    /// the actual upload rate.
    pub fn calculate_upload_speed(&self) -> u64 {
        let now = Instant::now();
        if let Ok(mut last_time) = self.last_upload_time.lock() {
            let elapsed = now.duration_since(*last_time);
            if elapsed >= Duration::from_secs(1) {
                let uploaded = self.uploaded_bytes_atomic.load(Ordering::Relaxed);
                let speed = uploaded / elapsed.as_secs().max(1);
                self.upload_speed_atomic.store(speed, Ordering::Relaxed);
                *last_time = now;
                return speed;
            }
        }
        self.upload_speed_atomic.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::bt_upload_session::InMemoryPieceProvider;
    use crate::engine::choking_algorithm::{ChokeAction, ChokingConfig};
    use crate::engine::peer_stats::PeerStats;

    #[test]
    fn test_exit_condition_default_infinite() {
        let cond = SeedExitCondition::default();
        assert!(cond.seed_time.is_none());
        assert!(cond.seed_ratio.is_none());
    }

    #[test]
    fn test_exit_condition_with_time_zero_is_infinite() {
        let cond = SeedExitCondition::with_time(0);
        assert!(cond.seed_time.is_none());
    }

    #[test]
    fn test_exit_condition_with_time_positive() {
        let cond = SeedExitCondition::with_time(60);
        assert_eq!(cond.seed_time, Some(Duration::from_secs(60)));
    }

    #[test]
    fn test_exit_condition_with_ratio_zero_is_infinite() {
        let cond = SeedExitCondition::with_ratio(0.0);
        assert!(cond.seed_ratio.is_none());
    }

    #[test]
    fn test_exit_condition_with_ratio_positive() {
        let cond = SeedExitCondition::with_ratio(1.5);
        assert_eq!(cond.seed_ratio, Some(1.5));
    }

    #[test]
    fn test_exit_condition_combined() {
        let cond = SeedExitCondition::with_time_and_ratio(120, 2.0);
        assert_eq!(cond.seed_time, Some(Duration::from_secs(120)));
        assert_eq!(cond.seed_ratio, Some(2.0));
    }

    #[test]
    fn test_should_exit_by_time() {
        let manager = make_test_manager(SeedExitCondition::with_time(1), 1000, 500);
        assert!(!manager.should_exit());

        let mut manager = manager;
        manager.seeding_start_time = Instant::now() - Duration::from_secs(2);
        assert!(manager.should_exit());
    }

    #[test]
    fn test_should_exit_by_ratio() {
        let manager = make_test_manager(SeedExitCondition::with_ratio(1.0), 1000, 499);
        assert!(!manager.should_exit());

        let mut manager = manager;
        manager.total_uploaded = 1500;
        assert!(manager.should_exit());
    }

    #[test]
    fn test_should_not_exit_early() {
        let manager = make_test_manager(SeedExitCondition::with_time_and_ratio(10, 3.0), 1000, 100);
        assert!(!manager.should_exit());

        let mut manager = manager;
        manager.total_uploaded = 2000;
        manager.seeding_start_time = Instant::now() - Duration::from_secs(5);
        assert!(!manager.should_exit(), "Neither time nor ratio reached yet");
    }

    #[test]
    fn test_seed_manager_stats() {
        let manager = make_test_manager(SeedExitCondition::infinite(), 1024 * 100, 51200);
        assert_eq!(manager.num_total_peers(), 0);
        assert_eq!(manager.num_alive_peers(), 0);
        assert_eq!(manager.total_uploaded(), 51200);
    }

    fn make_test_manager(
        exit_cond: SeedExitCondition,
        downloaded: u64,
        uploaded: u64,
    ) -> BtSeedManager {
        let provider = Arc::new(InMemoryPieceProvider::new(16384, 10));
        let config = BtSeedingConfig::default();
        let conns: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];
        let mut mgr = BtSeedManager::new(conns, provider, config, exit_cond, downloaded);
        mgr.total_uploaded = uploaded;
        mgr
    }

    fn make_test_manager_with_choking_algo(
        exit_cond: SeedExitCondition,
        downloaded: u64,
        uploaded: u64,
    ) -> BtSeedManager {
        use std::net::SocketAddr;
        let provider = Arc::new(InMemoryPieceProvider::new(16384, 10));
        let config = BtSeedingConfig::default();
        let conns: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];

        // Create a choking algorithm with fast rotation for testing
        let choking_config = ChokingConfig {
            max_upload_slots: 2,
            optimistic_unchoke_interval_secs: 1,
            snubbed_timeout_secs: 1,
            choke_rotation_interval_secs: 1,
        };
        let mut algo = ChokingAlgorithm::new(choking_config);
        // Add dummy peer stats for testing
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        algo.add_peer(PeerStats::new([0u8; 20], addr));

        let mut mgr = BtSeedManager::new_with_choking_algo(
            conns,
            provider,
            config,
            exit_cond,
            downloaded,
            Some(algo),
        );
        mgr.total_uploaded = uploaded;
        mgr
    }

    // ==================================================================
    // Choking algorithm integration tests
    // ==================================================================

    #[test]
    fn test_bt_seed_manager_without_choking_algo_backward_compat() {
        // Verify that BtSeedManager works without choking_algo (backward compatibility)
        let mut manager = make_test_manager(SeedExitCondition::infinite(), 1000, 500);
        assert!(manager.choking_algo.is_none());
        assert!(manager.choking_algo().is_none());
        assert!(manager.choking_algo_mut().is_none());

        // Stats should still work
        assert_eq!(manager.num_total_peers(), 0);
        assert_eq!(manager.num_alive_peers(), 0);
        assert_eq!(manager.total_uploaded(), 500);

        // sync_choking_algo_stats should be a no-op when algo is None
        manager.sync_choking_algo_stats(); // Should not panic
    }

    #[test]
    fn test_bt_seed_manager_with_choking_algo() {
        // Verify BtSeedManager with choking_algo initialized correctly
        let manager = make_test_manager_with_choking_algo(SeedExitCondition::infinite(), 2000, 800);
        assert!(manager.choking_algo.is_some());

        // Check algo has peers
        let algo = manager.choking_algo().unwrap();
        assert_eq!(algo.len(), 1);
        assert!(!algo.is_empty());

        // Stats should work
        assert_eq!(manager.num_total_peers(), 0); // sessions are empty (no real connections)
        assert_eq!(manager.total_uploaded(), 800);
    }

    #[test]
    fn test_bt_seed_manager_choking_algo_rotate_choke() {
        // Verify rotate_choke produces actions through the seed manager
        let mut manager = make_test_manager_with_choking_algo(SeedExitCondition::infinite(), 0, 0);

        // Get mutable access and call rotate_choke
        if let Some(algo) = manager.choking_algo_mut() {
            let actions = algo.rotate_choke();

            // With max_upload_slots=2 and 1 peer, we expect:
            // - The peer should be unchoked (it's in top K)
            let unchoke_count = actions
                .iter()
                .filter(|a| matches!(a, ChokeAction::Unchoke(_)))
                .count();

            assert_eq!(actions.len(), 1, "Should have one action for one peer");
            assert_eq!(unchoke_count, 1, "Single peer should be unchoked (top-K)");
        } else {
            panic!("Expected choking_algo to be present");
        }
    }

    #[test]
    fn test_bt_seed_manager_new_with_none_algo() {
        // new() should produce None for choking_algo (backward compat)
        let provider = Arc::new(InMemoryPieceProvider::new(16384, 10));
        let config = BtSeedingConfig::default();
        let conns: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];

        let mgr = BtSeedManager::new(conns, provider, config, SeedExitCondition::infinite(), 0);
        assert!(mgr.choking_algo.is_none());
    }

    #[test]
    fn test_bt_seed_manager_new_with_some_algo() {
        // new_with_choking_algo(Some(...)) should preserve it
        let provider = Arc::new(InMemoryPieceProvider::new(16384, 10));
        let config = BtSeedingConfig::default();
        let conns: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];

        let choking_config = ChokingConfig::default();
        let algo = ChokingAlgorithm::new(choking_config);

        let mgr = BtSeedManager::new_with_choking_algo(
            conns,
            provider,
            config,
            SeedExitCondition::infinite(),
            0,
            Some(algo),
        );
        assert!(mgr.choking_algo.is_some());
        assert_eq!(mgr.choking_algo.unwrap().len(), 0); // No peers added yet
    }

    // ==================================================================
    // H2: Seed condition utility function tests
    // ==================================================================

    #[test]
    fn test_seed_ratio_met_1_0() {
        // Ratio 1.0: should stop when uploaded >= downloaded
        assert!(!SeedExitCondition::check_seed_condition(500, 1000, 1.0));
        assert!(SeedExitCondition::check_seed_condition(1000, 1000, 1.0));
        assert!(SeedExitCondition::check_seed_condition(1500, 1000, 1.0));
    }

    #[test]
    fn test_seed_ratio_infinite_never_stops() {
        // Ratio 0.0 means infinite seeding - never stops
        assert!(!SeedExitCondition::check_seed_condition(999999, 1, 0.0));
        assert!(!SeedExitCondition::check_seed_condition(u64::MAX, 1, -1.0));
    }

    #[test]
    fn test_seed_ratio_zero_downloaded() {
        // Nothing downloaded yet - can't compute ratio
        assert!(!SeedExitCondition::check_seed_condition(1000, 0, 2.0));
    }

    #[test]
    fn test_seed_time_met_stops() {
        let start = Instant::now();
        // Time not met immediately after start
        assert!(!SeedExitCondition::check_seed_time(start, 5, true));
        // Time met when elapsed >= limit (simulate with negative time)
        let past_start = Instant::now() - Duration::from_secs(10);
        assert!(SeedExitCondition::check_seed_time(past_start, 5, true));
    }

    #[test]
    fn test_seed_time_infinite_zero() {
        // seed_time=0 means infinite seeding
        let start = Instant::now();
        assert!(!SeedExitCondition::check_seed_time(start, 0, true));
    }

    #[test]
    fn test_seed_time_not_pure_seeding() {
        // If still downloading (not pure seeding), time check returns false
        let past_start = Instant::now() - Duration::from_secs(100);
        assert!(
            !SeedExitCondition::check_seed_time(past_start, 5, false),
            "Should not stop if not in pure seeding phase"
        );
    }

    #[test]
    fn test_both_conditions_either_triggers_stop() {
        // Test that either condition being met triggers exit
        let past_start = Instant::now() - Duration::from_secs(100);

        // Time condition met, ratio not met -> should stop (time triggers)
        assert!(
            SeedExitCondition::check_seed_time(past_start, 5, true)
                || SeedExitCondition::check_seed_condition(500, 1000, 2.0),
            "Time condition should trigger stop"
        );

        // Ratio condition met, time not met -> should stop (ratio triggers)
        assert!(
            !SeedExitCondition::check_seed_time(Instant::now(), 100, true)
                && SeedExitCondition::check_seed_condition(2000, 1000, 1.5),
            "Ratio condition should trigger stop"
        );

        // Neither met -> should NOT stop
        assert!(
            !SeedExitCondition::check_seed_time(Instant::now(), 100, true)
                && !SeedExitCondition::check_seed_condition(500, 1000, 2.0),
            "Neither condition met, should continue seeding"
        );
    }

    // ==================================================================
    // New tests for enhanced BtSeedManager
    // ==================================================================

    #[test]
    fn test_upload_session_creation() {
        let addr: SocketAddr = "192.168.1.1:6881".parse().unwrap();
        let session = UploadSession::new(addr);
        assert_eq!(session.peer_addr, addr);
        assert_eq!(session.uploaded_bytes, 0);
        assert_eq!(session.upload_speed, 0);
        assert!(session.is_active);
    }

    #[test]
    fn test_upload_session_record_upload() {
        let addr: SocketAddr = "192.168.1.1:6881".parse().unwrap();
        let mut session = UploadSession::new(addr);
        session.record_upload(1024);
        assert_eq!(session.uploaded_bytes, 1024);
        session.record_upload(2048);
        assert_eq!(session.uploaded_bytes, 3072);
    }

    #[test]
    fn test_upload_session_update_speed() {
        let addr: SocketAddr = "192.168.1.1:6881".parse().unwrap();
        let mut session = UploadSession::new(addr);
        session.update_speed(50000);
        assert_eq!(session.upload_speed, 50000);
    }

    #[test]
    fn test_upload_session_deactivate() {
        let addr: SocketAddr = "192.168.1.1:6881".parse().unwrap();
        let mut session = UploadSession::new(addr);
        assert!(session.is_active);
        session.deactivate();
        assert!(!session.is_active);
    }

    #[test]
    fn test_bt_seed_manager_get_upload_stats() {
        let manager = make_test_manager(SeedExitCondition::infinite(), 1000, 500);
        let (uploaded, speed) = manager.get_upload_stats();
        // Initial stats should be 0 (atomic starts at 0)
        assert_eq!(uploaded, 0);
        assert_eq!(speed, 0);
    }

    #[test]
    fn test_bt_seed_manager_update_upload_speed() {
        let manager = make_test_manager(SeedExitCondition::infinite(), 1000, 500);
        manager.update_upload_speed(100000);
        let (_, speed) = manager.get_upload_stats();
        assert_eq!(speed, 100000);
    }

    #[test]
    fn test_bt_seed_manager_should_stop_seeding_ratio() {
        let manager = make_test_manager(SeedExitCondition::with_ratio(1.0), 1000, 0);
        
        // Should not stop initially (uploaded = 0)
        assert!(!manager.should_stop_seeding(1000));
        
        // Simulate uploading 500 bytes (ratio = 0.5)
        manager.uploaded_bytes_atomic.store(500, Ordering::Relaxed);
        assert!(!manager.should_stop_seeding(1000));
        
        // Simulate uploading 1000 bytes (ratio = 1.0)
        manager.uploaded_bytes_atomic.store(1000, Ordering::Relaxed);
        assert!(manager.should_stop_seeding(1000));
        
        // Simulate uploading 1500 bytes (ratio = 1.5)
        manager.uploaded_bytes_atomic.store(1500, Ordering::Relaxed);
        assert!(manager.should_stop_seeding(1000));
    }

    #[test]
    fn test_bt_seed_manager_should_stop_seeding_time() {
        let mut manager = make_test_manager(SeedExitCondition::with_time(1), 1000, 0);
        
        // Should not stop immediately
        assert!(!manager.should_stop_seeding(1000));
        
        // Simulate time passing by modifying seeding_start_time
        manager.seeding_start_time = Instant::now() - Duration::from_secs(2);
        assert!(manager.should_stop_seeding(1000));
    }

    #[test]
    fn test_bt_seed_manager_should_stop_seeding_infinite() {
        let manager = make_test_manager(SeedExitCondition::infinite(), 1000, 0);
        
        // Should never stop with infinite seeding
        assert!(!manager.should_stop_seeding(1000));
        assert!(!manager.should_stop_seeding(0));
    }

    #[test]
    fn test_bt_seed_manager_info_hash() {
        let mut manager = make_test_manager(SeedExitCondition::infinite(), 1000, 0);
        
        // Default info_hash should be all zeros
        assert_eq!(manager.info_hash(), &[0u8; 20]);
        
        // Set a custom info_hash
        let custom_hash = [0x12u8; 20];
        manager.set_info_hash(custom_hash);
        assert_eq!(manager.info_hash(), &custom_hash);
    }

    #[test]
    fn test_bt_seed_manager_new_with_info_hash() {
        let custom_hash = [0xABu8; 20];
        let provider = Arc::new(InMemoryPieceProvider::new(16384, 10));
        let config = BtSeedingConfig::default();
        let conns: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];
        
        let manager = BtSeedManager::new_with_info_hash(
            custom_hash,
            conns,
            provider,
            config,
            SeedExitCondition::infinite(),
            1000,
        );
        
        assert_eq!(manager.info_hash(), &custom_hash);
    }

    #[test]
    fn test_bt_seed_manager_max_uploads() {
        let mut manager = make_test_manager(SeedExitCondition::infinite(), 1000, 0);
        
        // Default max_uploads should match config
        assert_eq!(manager.max_uploads(), 4); // BtSeedingConfig::default().max_peers_to_unchoke
        
        // Set a new max
        manager.set_max_uploads(8);
        assert_eq!(manager.max_uploads(), 8);
    }

    #[test]
    fn test_bt_seed_manager_seed_ratio_and_time() {
        let manager = make_test_manager(SeedExitCondition::with_time_and_ratio(60, 2.0), 1000, 0);
        
        assert_eq!(manager.seed_ratio(), 2.0);
        assert_eq!(manager.seed_time(), Duration::from_secs(60));
    }

    #[test]
    fn test_bt_seed_manager_total_downloaded() {
        let manager = make_test_manager(SeedExitCondition::infinite(), 5000, 0);
        assert_eq!(manager.total_downloaded(), 5000);
    }

    #[test]
    fn test_bt_seed_manager_num_active_uploads() {
        let manager = make_test_manager(SeedExitCondition::infinite(), 1000, 0);
        
        // Initially no active uploads
        assert_eq!(manager.num_active_uploads(), 0);
    }

    #[tokio::test]
    async fn test_bt_seed_manager_handle_piece_request() {
        let mut provider = InMemoryPieceProvider::new(16384, 10);
        // Set up some test data
        provider.set_piece_data(0, vec![0xABu8; 16384]);
        
        let provider_arc = Arc::new(provider);
        let config = BtSeedingConfig::default();
        let conns: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];
        
        let mut manager = BtSeedManager::new(
            conns,
            provider_arc,
            config,
            SeedExitCondition::infinite(),
            1000,
        );
        
        let peer_addr: SocketAddr = "192.168.1.1:6881".parse().unwrap();
        
        // Request a piece
        let result = manager.handle_piece_request(peer_addr, 0, 0, 1024).await;
        assert!(result.is_ok());
        
        let data = result.unwrap();
        assert_eq!(data.len(), 1024);
        assert!(data.iter().all(|&b| b == 0xAB));
        
        // Check that upload stats were updated
        let (uploaded, _) = manager.get_upload_stats();
        assert_eq!(uploaded, 1024);
        
        // Check that active uploads were tracked
        assert_eq!(manager.num_active_uploads(), 1);
    }

    #[tokio::test]
    async fn test_bt_seed_manager_handle_piece_request_invalid_piece() {
        let provider = Arc::new(InMemoryPieceProvider::new(16384, 10));
        let config = BtSeedingConfig::default();
        let conns: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> = vec![];
        
        let mut manager = BtSeedManager::new(
            conns,
            provider,
            config,
            SeedExitCondition::infinite(),
            1000,
        );
        
        let peer_addr: SocketAddr = "192.168.1.1:6881".parse().unwrap();
        
        // Request a piece that doesn't exist (piece 0 has no data)
        let result = manager.handle_piece_request(peer_addr, 0, 0, 1024).await;
        assert!(result.is_err());
    }

    // ==================================================================
    // Upload speed throttling tests
    // ==================================================================

    #[test]
    fn test_bt_seed_manager_max_upload_speed() {
        let mut manager = make_test_manager(SeedExitCondition::infinite(), 1000, 0);
        
        // Initially no limit
        assert!(manager.max_upload_speed().is_none());
        
        // Set a limit
        manager.set_max_upload_speed(Some(100000));
        assert_eq!(manager.max_upload_speed(), Some(100000));
        
        // Set to 0 should be treated as unlimited
        manager.set_max_upload_speed(Some(0));
        assert!(manager.max_upload_speed().is_none());
        
        // Set to None should be unlimited
        manager.set_max_upload_speed(None);
        assert!(manager.max_upload_speed().is_none());
    }

    #[test]
    fn test_bt_seed_manager_current_upload_speed() {
        let manager = make_test_manager(SeedExitCondition::infinite(), 1000, 0);
        
        // Initially 0
        assert_eq!(manager.current_upload_speed(), 0);
        
        // Update speed
        manager.update_upload_speed(50000);
        assert_eq!(manager.current_upload_speed(), 50000);
    }

    #[tokio::test]
    async fn test_bt_seed_manager_throttle_upload_no_limit() {
        let manager = make_test_manager(SeedExitCondition::infinite(), 1000, 0);
        
        // No limit set, should return immediately
        let result = manager.throttle_upload(0, 10000).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_bt_seed_manager_throttle_upload_within_limit() {
        let manager = make_test_manager(SeedExitCondition::infinite(), 1000, 0);
        
        // Within limit, should not throttle
        let result = manager.throttle_upload(100000, 1000).await;
        assert!(result.is_ok());
        
        // Check that bytes were tracked
        assert_eq!(manager.throttle_window_bytes.load(Ordering::Relaxed), 1000);
    }

    // ==================================================================
    // Seeding statistics tests
    // ==================================================================

    #[test]
    fn test_seed_stats_calculation() {
        let mut manager = make_test_manager(SeedExitCondition::infinite(), 1000, 0);
        
        // Simulate some uploads
        manager.uploaded_bytes_atomic.store(500, Ordering::Relaxed);
        manager.total_uploaded = 500;
        
        let (uploaded, _) = manager.get_upload_stats();
        assert_eq!(uploaded, 500);
        
        // Calculate ratio
        let ratio = uploaded as f64 / manager.total_downloaded as f64;
        assert!((ratio - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_seed_stats_with_elapsed_time() {
        let manager = make_test_manager(SeedExitCondition::infinite(), 1000, 0);
        
        let duration = manager.seeding_duration();
        // Should be very small (just created)
        assert!(duration.as_secs() < 1);
    }

    // ==================================================================
    // Edge cases and stress tests
    // ==================================================================

    #[test]
    fn test_seed_exit_condition_zero_ratio_is_infinite() {
        // Ratio 0.0 should never trigger stop
        let cond = SeedExitCondition::with_ratio(0.0);
        assert!(cond.seed_ratio.is_none());
    }

    #[test]
    fn test_seed_exit_condition_zero_time_is_infinite() {
        // Time 0 should never trigger stop
        let cond = SeedExitCondition::with_time(0);
        assert!(cond.seed_time.is_none());
    }

    #[test]
    fn test_seed_exit_condition_negative_ratio() {
        // Negative ratio should be treated as infinite
        let cond = SeedExitCondition::with_ratio(-1.0);
        assert!(cond.seed_ratio.is_none());
    }

    #[test]
    fn test_should_stop_seeding_with_both_conditions() {
        // Test with both time and ratio set
        let mut manager = make_test_manager(
            SeedExitCondition::with_time_and_ratio(60, 1.0),
            1000,
            0,
        );
        
        // Neither condition met yet
        assert!(!manager.should_stop_seeding(1000));
        
        // Meet ratio condition
        manager.uploaded_bytes_atomic.store(1000, Ordering::Relaxed);
        assert!(manager.should_stop_seeding(1000));
        
        // Reset and meet time condition
        manager.uploaded_bytes_atomic.store(0, Ordering::Relaxed);
        manager.seeding_start_time = Instant::now() - Duration::from_secs(120);
        assert!(manager.should_stop_seeding(1000));
    }

    #[test]
    fn test_max_uploads_enforcement() {
        let mut manager = make_test_manager(SeedExitCondition::infinite(), 1000, 0);
        
        // Default max uploads
        assert_eq!(manager.max_uploads(), 4);
        
        // Increase max uploads
        manager.set_max_uploads(10);
        assert_eq!(manager.max_uploads(), 10);
        
        // Decrease max uploads
        manager.set_max_uploads(2);
        assert_eq!(manager.max_uploads(), 2);
    }

    #[test]
    fn test_active_uploads_tracking() {
        let manager = make_test_manager(SeedExitCondition::infinite(), 1000, 0);
        
        // Initially no active uploads
        assert_eq!(manager.num_active_uploads(), 0);
        assert!(manager.active_uploads().is_empty());
    }
}

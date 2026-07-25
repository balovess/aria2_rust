//! BitTorrent Seed Manager — seeding phase management after download completes
//!
//! This module manages the seeding phase of a BitTorrent download, including:
//! - Uploading pieces to leecher peers
//! - Choking/unchoking peers based on upload choking algorithm
//! - Monitoring seed exit conditions (ratio, time)
//! - Tracking cumulative upload statistics
//!
//! # Architecture
//!
//! - [`BtSeedManager`] — Top-level seeding manager that owns upload sessions
//!   and runs the seeding loop until exit conditions are met.
//! - [`SeedExitCondition`] — Conditions under which seeding should stop
//!   (time limit, ratio limit, or infinite).
//! - [`UploadSession`] — Per-peer upload session tracking (from `types.rs`).
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `BtSeedManager` | `SeedCheckCommand` + upload session management |
//! | `SeedExitCondition` | `--seed-time` / `--seed-ratio` option handling |

pub mod types;

// Re-export key types from the types submodule for convenience
pub use types::{SeedExitCondition, UploadSession, BAD_DATA_THRESHOLD};

use std::sync::Arc;
use std::time::{Duration, Instant};

use aria2_protocol::bittorrent::peer::connection::PeerConnection;
use tracing::info;

use crate::engine::bt_piece_downloader::FileBackedPieceProvider;
use crate::engine::bt_upload_session::{BtSeedingConfig, PieceDataProvider};
use crate::engine::choking_algorithm::ChokingAlgorithm;

// ===========================================================================
// BtSeedManager — top-level seeding phase manager
// ===========================================================================

/// Manages the seeding phase of a completed BitTorrent download.
///
/// After all pieces are downloaded, `BtSeedManager` takes over and:
/// 1. Accepts incoming piece requests from leecher peers
/// 2. Applies the upload choking algorithm to select which peers to unchoke
/// 3. Uploads piece data at the configured rate limit
/// 4. Monitors seed exit conditions (ratio/time) and stops when met
///
/// Mirrors C++ `SeedCheckCommand` combined with upload session management.
pub struct BtSeedManager {
    /// Info hash of the torrent being seeded
    info_hash: [u8; 20],
    /// Active upload sessions (one per connected peer)
    sessions: Vec<UploadSession>,
    /// Piece data provider for reading completed pieces from disk
    /// TODO: will be used when piece request handling is implemented in run_seeding_loop
    #[allow(dead_code)]
    piece_provider: Option<Arc<dyn PieceDataProvider>>,
    /// Seeding configuration (rate limits, unchoke settings)
    /// TODO: will be used when upload rate limiting and unchoke logic are implemented
    #[allow(dead_code)]
    config: BtSeedingConfig,
    /// Exit condition (ratio/time/infinite)
    exit_condition: SeedExitCondition,
    /// Total bytes downloaded (for ratio calculation)
    total_downloaded: u64,
    /// Total bytes uploaded during seeding
    pub total_uploaded: u64,
    /// When seeding started
    pub seeding_start_time: Instant,
    /// Whether seeding is currently active
    is_active: bool,
    /// Choking algorithm (optional, for advanced choke management)
    // TODO: will be used when choking algorithm execution is implemented in run_seeding_loop
    #[allow(dead_code)]
    choking_algo: Option<ChokingAlgorithm>,
}

impl BtSeedManager {
    /// Create a new seed manager with basic parameters.
    ///
    /// This is the simplest constructor, used by tests and simple seeding setups.
    /// For full control, use `new_with_info_hash` or `new_with_choking_algo`.
    pub fn new(
        connections: Vec<PeerConnection>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
    ) -> Self {
        let sessions = Vec::with_capacity(connections.len());
        Self {
            info_hash: [0u8; 20],
            sessions,
            piece_provider: Some(piece_provider),
            config,
            exit_condition,
            total_downloaded,
            total_uploaded: 0,
            seeding_start_time: Instant::now(),
            is_active: true,
            choking_algo: None,
        }
    }

    /// Create a new seed manager with an info hash.
    ///
    /// This is the constructor used by `BtDownloadCommand` when initializing
    /// the seed manager after download completion.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_info_hash(
        info_hash: [u8; 20],
        connections: Vec<PeerConnection>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
    ) -> Self {
        let sessions = Vec::with_capacity(connections.len());
        Self {
            info_hash,
            sessions,
            piece_provider: Some(piece_provider),
            config,
            exit_condition,
            total_downloaded,
            total_uploaded: 0,
            seeding_start_time: Instant::now(),
            is_active: true,
            choking_algo: None,
        }
    }

    /// Create a new seed manager with a choking algorithm.
    ///
    /// This is the constructor used by `BtDownloadCommand::run_seeding_phase()`.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_choking_algo(
        connections: Vec<PeerConnection>,
        piece_provider: Arc<FileBackedPieceProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        choking_algo: Option<ChokingAlgorithm>,
    ) -> Self {
        let sessions = Vec::with_capacity(connections.len());
        Self {
            info_hash: [0u8; 20],
            sessions,
            piece_provider: Some(piece_provider),
            config,
            exit_condition,
            total_downloaded,
            total_uploaded: 0,
            seeding_start_time: Instant::now(),
            is_active: true,
            choking_algo,
        }
    }

    /// Run the main seeding loop until exit conditions are met.
    ///
    /// This is the primary entry point for the seeding phase. It loops:
    /// 1. Check exit conditions (ratio/time)
    /// 2. Process incoming piece requests from peers
    /// 3. Apply choking algorithm
    /// 4. Upload piece data to unchoked peers
    /// 5. Yield to the tokio runtime
    pub async fn run_seeding_loop(&mut self) -> crate::error::Result<()> {
        info!(
            info_hash = ?self.info_hash,
            "Seeding loop started (ratio={:?}, time={:?})",
            self.exit_condition.seed_ratio, self.exit_condition.seed_time
        );

        // TODO: Implement the actual seeding loop with:
        // - Periodic exit condition checks
        // - Piece request handling
        // - Choking algorithm execution
        // - Upload rate limiting
        // - Peer session management

        // For now, mark as inactive after a single check
        self.is_active = false;
        Ok(())
    }

    /// Check if the seed exit conditions have been met.
    ///
    /// Returns `true` if seeding should stop.
    pub fn should_stop_seeding(&self) -> bool {
        // Check seed ratio
        if let Some(ratio) = self.exit_condition.seed_ratio {
            if SeedExitCondition::check_seed_condition(
                self.total_uploaded,
                self.total_downloaded,
                ratio,
            ) {
                return true;
            }
        }

        // Check seed time
        if let Some(time) = self.exit_condition.seed_time {
            if SeedExitCondition::check_seed_time(
                self.seeding_start_time,
                time.as_secs(),
                true,
            ) {
                return true;
            }
        }

        false
    }

    /// Alias for `should_stop_seeding()`, matching C++ `shouldExit()` naming.
    pub fn should_exit(&self) -> bool {
        self.should_stop_seeding()
    }

    /// Return total bytes uploaded during seeding.
    pub fn total_uploaded(&self) -> u64 {
        self.total_uploaded
    }

    /// Return total bytes downloaded (used for seed ratio calculation).
    pub fn total_downloaded(&self) -> u64 {
        self.total_downloaded
    }

    /// Return upload statistics: (total_uploaded, upload_speed).
    pub fn get_upload_stats(&self) -> (u64, u64) {
        // Approximate upload speed from elapsed time
        let elapsed_secs = self.seeding_start_time.elapsed().as_secs();
        let upload_speed = if elapsed_secs > 0 {
            self.total_uploaded / elapsed_secs
        } else {
            0
        };
        (self.total_uploaded, upload_speed)
    }

    /// Return the duration of the seeding phase.
    pub fn seeding_duration(&self) -> Duration {
        self.seeding_start_time.elapsed()
    }

    /// Return whether seeding is currently active.
    pub fn is_active(&self) -> bool {
        self.is_active
    }

    /// Return the info hash of the torrent being seeded.
    pub fn info_hash(&self) -> &[u8; 20] {
        &self.info_hash
    }

    /// Return the number of active upload sessions.
    pub fn num_sessions(&self) -> usize {
        self.sessions.len()
    }

    /// Record bytes uploaded to a peer.
    pub fn record_upload(&mut self, bytes: u64) {
        self.total_uploaded += bytes;
    }
}

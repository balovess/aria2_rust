//! BitTorrent Seed Manager — seeding phase management after download completes
//!
//! This module manages the seeding phase of a BitTorrent download, including:
//! - Uploading pieces to leecher peers
//! - Choking/unchoking peers based on the seeder-state choking algorithm
//! - Monitoring seed exit conditions (ratio, time)
//! - Tracking cumulative upload statistics
//!
//! # Architecture
//!
//! - [`BtSeedManager`] — Top-level seeding manager that owns upload sessions
//!   and runs the seeding loop until exit conditions are met.
//! - [`SeedExitCondition`] — Conditions under which seeding should stop
//!   (time limit, ratio limit, or infinite).
//!
//! # Seeding Loop
//!
//! The loop waits on peer sockets, incoming peers, cancellation, or the next
//! protocol deadline. Each event performs the smallest required state update:
//! 1. Check cancellation / exit conditions
//! 2. Admit incoming peers or process one ready peer message
//! 3. Sync upload-session state -> PeerStats
//! 4. Run the seeder-state choking algorithm when its deadline expires
//! 5. Apply choke/unchoke decisions back to upload sessions
//! 6. Remove dead sessions and report progress
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `BtSeedManager` | `SeedCheckCommand` + upload session management |
//! | `SeedExitCondition` | `--seed-time` / `--seed-ratio` option handling |
//! | `BtSeederStateChoke` | `BtSeederStateChoke` |

mod constructors;
mod seeding_loop;
#[cfg(test)]
mod tests;
pub mod types;

// Re-export the exit-condition type from the types submodule for convenience.
pub use types::SeedExitCondition;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::engine::bt_choke_manager::BtSeederStateChoke;
use crate::engine::bt_tracker_comm::TrackerAnnouncer;
use crate::engine::bt_upload_session::{BtSeedingConfig, BtUploadSession, PieceDataProvider};
use crate::engine::choking_algorithm::ChokingAlgorithm;
use crate::engine::peer_stats::PeerStats;
use crate::request::request_group::{AtomicProgress, BtPeerSnapshot, ConnectionState};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Interval between choke rounds (seconds). Matches C++ rotation interval.
const CHOKE_ROUND_INTERVAL_SECS: u64 = 10;

// ===========================================================================
// BtSeedManager — top-level seeding phase manager
// ===========================================================================

/// Manages the seeding phase of a completed BitTorrent download.
///
/// After all pieces are downloaded, `BtSeedManager` takes over and:
/// 1. Accepts incoming piece requests from leecher peers
/// 2. Applies the seeder-state upload choking algorithm
/// 3. Uploads piece data at the configured rate limit
/// 4. Monitors seed exit conditions (ratio/time) and stops when met
///
/// Mirrors C++ `SeedCheckCommand` combined with upload session management.
/// Top-level manager for the BitTorrent seeding phase.
pub struct BtSeedManager {
    /// Info hash of the torrent being seeded
    info_hash: [u8; 20],
    /// Active upload sessions (one per connected peer)
    upload_sessions: Vec<BtUploadSession>,
    /// Peer statistics synced with the choking algorithm
    peer_stats: Vec<PeerStats>,
    /// Piece data provider for reading completed pieces from disk
    piece_provider: Option<Arc<dyn PieceDataProvider>>,
    /// Seeding configuration (rate limits, unchoke settings)
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
    /// Seeder-state choking algorithm
    seeder_choke: BtSeederStateChoke,
    /// Legacy choking algorithm (used during download phase, kept for
    /// compatibility with BtDownloadCommand)
    #[allow(dead_code)]
    choking_algo: Option<ChokingAlgorithm>,
    /// Cancellation token for graceful shutdown
    cancel_token: CancellationToken,
    /// Shared peer storage used to release seeding sessions on disconnect.
    peer_storage: Option<
        std::sync::Arc<std::sync::Mutex<crate::engine::bt_peer_storage::DefaultPeerStorage>>,
    >,
    /// Set when seed criteria ends the runtime, matching BtRuntime::halt.
    halt_requested: bool,
    /// Timestamp of the last choke round
    last_choke_time: Instant,
    /// Tracker announcer for periodic re-announce while seeding
    /// (mirrors C++ SeedCheckCommand keeping the swarm informed).
    announcer: Option<TrackerAnnouncer>,
    /// Our peer id, sent with tracker announces.
    peer_id: [u8; 20],
    /// Incoming peers routed to this torrent while it remains in seeding mode.
    incoming_peers:
        Option<tokio::sync::mpsc::Receiver<crate::engine::bt_peer_listener::IncomingPeer>>,
    /// Lock-free progress sink for live RPC/UI upload statistics.
    upload_progress: Option<Arc<AtomicProgress>>,
    /// Shared protocol counters and peer snapshots consumed by RPC/TUI.
    connection_state: Option<Arc<ConnectionState>>,
    peer_snapshot_store: Option<Arc<std::sync::RwLock<Vec<BtPeerSnapshot>>>>,
}

impl BtSeedManager {
    // -----------------------------------------------------------------------
    // Public query methods
    // -----------------------------------------------------------------------

    /// Check if the seed exit conditions have been met.
    ///
    /// Returns `true` if seeding should stop.
    pub fn should_stop_seeding(&self) -> bool {
        // Check seed ratio
        if let Some(ratio) = self.exit_condition.seed_ratio
            && SeedExitCondition::check_seed_condition(
                self.total_uploaded,
                self.total_downloaded,
                ratio,
            )
        {
            return true;
        }

        // Check seed time
        if let Some(time) = self.exit_condition.seed_time
            && SeedExitCondition::check_seed_time(self.seeding_start_time, time.as_secs(), true)
        {
            return true;
        }

        false
    }

    /// Alias for `should_stop_seeding()`, matching C++ `shouldExit()` naming.
    pub fn should_exit(&self) -> bool {
        self.should_stop_seeding()
    }

    /// Whether a seed criterion requested runtime halt.
    pub fn halt_requested(&self) -> bool {
        self.halt_requested
    }

    /// Return total bytes uploaded during seeding.
    pub fn total_uploaded(&self) -> u64 {
        self.total_uploaded
    }

    pub fn take_announcer(&mut self) -> Option<TrackerAnnouncer> {
        self.announcer.take()
    }

    /// Return total bytes downloaded (used for seed ratio calculation).
    pub fn total_downloaded(&self) -> u64 {
        self.total_downloaded
    }

    /// Return upload statistics: (total_uploaded, upload_speed).
    pub fn get_upload_stats(&self) -> (u64, u64) {
        let elapsed_secs = self.seeding_start_time.elapsed().as_secs_f64();
        let upload_speed = if elapsed_secs > 0.0 {
            (self.total_uploaded as f64 / elapsed_secs) as u64
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
        self.upload_sessions.len()
    }

    /// Record bytes uploaded to a peer.
    pub fn record_upload(&mut self, bytes: u64) {
        self.total_uploaded = self.total_uploaded.saturating_add(bytes);
    }

    /// Restore upload statistics when a paused BT command resumes seeding.
    pub(crate) fn set_total_uploaded(&mut self, total_uploaded: u64) {
        self.total_uploaded = total_uploaded;
        self.publish_upload_stats();
    }

    /// Cancel the seeding loop (external shutdown signal).
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// Get a clone of the cancellation token for external observers.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Attach the command's lock-free progress counters for live statistics.
    pub(crate) fn set_upload_progress(&mut self, progress: Arc<AtomicProgress>) {
        self.upload_progress = Some(progress);
        self.publish_upload_stats();
    }

    pub(crate) fn set_connection_state(
        &mut self,
        connection_state: Arc<ConnectionState>,
        peer_snapshot_store: Arc<std::sync::RwLock<Vec<BtPeerSnapshot>>>,
    ) {
        self.connection_state = Some(connection_state);
        self.peer_snapshot_store = Some(peer_snapshot_store);
        self.publish_connection_state();
    }

    fn clear_connection_state(&self) {
        if let Some(connection_state) = self.connection_state.as_ref() {
            connection_state.set_bt(0);
        }
        if let Some(store) = self.peer_snapshot_store.as_ref() {
            *store
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Vec::new();
        }
    }

    fn publish_connection_state(&self) {
        if let Some(connection_state) = self.connection_state.as_ref() {
            connection_state.set_bt(self.num_sessions());
        }
        if let Some(store) = self.peer_snapshot_store.as_ref() {
            *store
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = self.peer_snapshots();
        }
    }

    fn peer_snapshots(&self) -> Vec<BtPeerSnapshot> {
        self.upload_sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                let addr = session.remote_endpoint()?;
                let stats = self.peer_stats.get(index);
                Some(BtPeerSnapshot {
                    peer_id: session.remote_peer_id().unwrap_or([0; 20]),
                    addr,
                    is_incoming: true,
                    source: crate::request::request_group::BtPeerSource::Incoming,
                    bitfield: None,
                    uploaded_bytes: session.uploaded_bytes(),
                    downloaded_bytes: 0,
                    upload_speed: stats.map_or(0.0, |stats| stats.upload_speed),
                    download_speed: 0.0,
                    avg_upload_speed: stats.map_or(0, |stats| stats.avg_upload_speed),
                    avg_download_speed: 0,
                    am_choking: session.is_peer_choked(),
                    peer_choking: false,
                    seeder: Some(false),
                    connection_duration_secs: stats
                        .map_or(0, |stats| stats.connection_duration_secs()),
                    last_data_age_secs: stats.map_or(0, |stats| {
                        stats
                            .last_data_time
                            .map(|time| time.elapsed().as_secs())
                            .unwrap_or_else(|| stats.connection_duration_secs())
                    }),
                    is_snubbed: stats.is_some_and(|stats| stats.is_snubbed),
                    is_banned: false,
                })
            })
            .collect()
    }

    fn publish_upload_stats(&self) {
        if let Some(progress) = self.upload_progress.as_ref() {
            progress.set_upload_length(self.total_uploaded);
            progress.set_upload_speed(self.get_upload_stats().1);
        }
    }
}

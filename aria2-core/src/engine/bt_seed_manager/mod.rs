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
//! The loop runs on a ~2 s tick and performs:
//! 1. Check cancellation / exit conditions
//! 2. Handle incoming messages from each peer (with per-session timeout)
//! 3. Sync upload-session state → PeerStats (peer_interested, uploaded_bytes)
//! 4. Execute seeder-state choking algorithm
//! 5. Apply choke/unchoke decisions back to upload sessions
//! 6. Remove dead sessions, report progress
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `BtSeedManager` | `SeedCheckCommand` + upload session management |
//! | `SeedExitCondition` | `--seed-time` / `--seed-ratio` option handling |
//! | `BtSeederStateChoke` | `BtSeederStateChoke` |

pub mod types;

// Re-export key types from the types submodule for convenience
pub use types::{BAD_DATA_THRESHOLD, SeedExitCondition, UploadSession};

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::engine::bt_choke_manager::BtSeederStateChoke;
use crate::engine::bt_piece_downloader::FileBackedPieceProvider;
use crate::engine::bt_tracker_comm::TrackerAnnouncer;
use crate::engine::bt_upload_session::{
    BtSeedingConfig, BtUploadConnection, BtUploadSession, PieceDataProvider,
};
use crate::engine::choking_algorithm::ChokingAlgorithm;
use crate::engine::peer_stats::PeerStats;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Interval between seeding-loop ticks (seconds).
const SEED_TICK_INTERVAL: Duration = Duration::from_secs(2);

/// Per-session timeout for reading incoming messages during one tick.
const MSG_READ_TIMEOUT: Duration = Duration::from_millis(200);

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
}

impl BtSeedManager {
    /// Create a new seed manager with basic parameters.
    ///
    /// This is the simplest constructor, used by tests and simple seeding setups.
    pub fn new(
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
    ) -> Self {
        Self::build(
            [0u8; 20],
            connections
                .into_iter()
                .map(|connection| BtUploadConnection::Plain(Box::new(connection)))
                .collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            None,
            CancellationToken::new(),
            None,
            None,
            [0u8; 20],
        )
    }

    /// Create a new seed manager with an info hash.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_info_hash(
        info_hash: [u8; 20],
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
    ) -> Self {
        Self::build(
            info_hash,
            connections
                .into_iter()
                .map(|connection| BtUploadConnection::Plain(Box::new(connection)))
                .collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            None,
            CancellationToken::new(),
            None,
            None,
            [0u8; 20],
        )
    }

    /// Create a new seed manager with a choking algorithm.
    ///
    /// This is the constructor used by `BtDownloadCommand::run_seeding_phase()`.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_choking_algo(
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_provider: Arc<FileBackedPieceProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        choking_algo: Option<ChokingAlgorithm>,
    ) -> Self {
        Self::build(
            [0u8; 20],
            connections
                .into_iter()
                .map(|connection| BtUploadConnection::Plain(Box::new(connection)))
                .collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            choking_algo,
            CancellationToken::new(),
            None,
            None,
            [0u8; 20],
        )
    }

    /// Create a seed manager with a tracker announcer for periodic
    /// re-announce while seeding (C++ SeedCheckCommand keeps the swarm
    /// informed of the seeder's continued presence).
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_announcer(
        info_hash: [u8; 20],
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        choking_algo: Option<ChokingAlgorithm>,
        announcer: Option<TrackerAnnouncer>,
        peer_id: [u8; 20],
    ) -> Self {
        Self::build(
            info_hash,
            connections
                .into_iter()
                .map(|connection| BtUploadConnection::Plain(Box::new(connection)))
                .collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            choking_algo,
            CancellationToken::new(),
            None,
            announcer,
            peer_id,
        )
    }

    /// Create a seed manager with a cancellation token (for external shutdown).
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_cancel_token(
        info_hash: [u8; 20],
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        cancel_token: CancellationToken,
    ) -> Self {
        Self::build(
            info_hash,
            connections
                .into_iter()
                .map(|connection| BtUploadConnection::Plain(Box::new(connection)))
                .collect(),
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            None,
            cancel_token,
            None,
            None,
            [0u8; 20],
        )
    }

    /// Construct the production seeding manager with the transport variants
    /// already accepted by the download loop and its live incoming-peer route.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_transports(
        info_hash: [u8; 20],
        connections: Vec<BtUploadConnection>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        choking_algo: Option<ChokingAlgorithm>,
        announcer: Option<TrackerAnnouncer>,
        peer_id: [u8; 20],
        incoming_peers: Option<
            tokio::sync::mpsc::Receiver<crate::engine::bt_peer_listener::IncomingPeer>,
        >,
    ) -> Self {
        Self::build(
            info_hash,
            connections,
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
            choking_algo,
            CancellationToken::new(),
            incoming_peers,
            announcer,
            peer_id,
        )
    }

    /// Common builder used by all public constructors.
    #[allow(clippy::too_many_arguments)]
    fn build(
        info_hash: [u8; 20],
        connections: Vec<BtUploadConnection>,
        piece_provider: Arc<dyn PieceDataProvider>,
        config: BtSeedingConfig,
        exit_condition: SeedExitCondition,
        total_downloaded: u64,
        choking_algo: Option<ChokingAlgorithm>,
        cancel_token: CancellationToken,
        incoming_peers: Option<
            tokio::sync::mpsc::Receiver<crate::engine::bt_peer_listener::IncomingPeer>,
        >,
        announcer: Option<TrackerAnnouncer>,
        peer_id: [u8; 20],
    ) -> Self {
        // Create upload sessions from peer connections
        let upload_sessions: Vec<BtUploadSession> = connections
            .into_iter()
            .map(|conn| {
                let mut session = BtUploadSession::new_with_connection(conn, &config);
                session.configure_message_validator(
                    piece_provider.num_pieces(),
                    piece_provider.piece_length(),
                );
                session
            })
            .collect();

        // Initialise PeerStats for each session (the seeder-state algorithm
        // needs peer_interested, upload_speed, etc.). Keep the transport
        // endpoint as the identity used by the choking and reporting layers.
        let peer_stats: Vec<PeerStats> = upload_sessions
            .iter()
            .map(|session| {
                let addr = session
                    .endpoint()
                    .and_then(|(ip, port)| format!("{ip}:{port}").parse().ok())
                    .unwrap_or_else(|| "0.0.0.0:0".parse().expect("valid unspecified address"));
                PeerStats::new([0u8; 20], addr)
            })
            .collect();

        let seeder_choke = BtSeederStateChoke::with_slots(config.max_peers_to_unchoke);

        Self {
            info_hash,
            upload_sessions,
            peer_stats,
            piece_provider: Some(piece_provider),
            config,
            exit_condition,
            total_downloaded,
            total_uploaded: 0,
            seeding_start_time: Instant::now(),
            is_active: true,
            seeder_choke,
            choking_algo,
            cancel_token,
            peer_storage: None,
            halt_requested: false,
            last_choke_time: Instant::now(),
            announcer,
            peer_id,
            incoming_peers,
        }
    }

    /// Attach the session-scoped peer storage used to release seeding peers.
    pub fn with_peer_storage(
        mut self,
        peer_storage: std::sync::Arc<
            std::sync::Mutex<crate::engine::bt_peer_storage::DefaultPeerStorage>,
        >,
    ) -> Self {
        self.peer_storage = Some(peer_storage);
        self
    }

    // -----------------------------------------------------------------------
    // Main seeding loop
    // -----------------------------------------------------------------------

    /// Run the main seeding loop until exit conditions are met or cancelled.
    ///
    /// This is the primary entry point for the seeding phase. It loops:
    /// 1. Check exit conditions (ratio/time)
    /// 2. Process incoming piece requests from peers (with per-session timeout)
    /// 3. Sync upload-session state → PeerStats
    /// 4. Execute seeder-state choking algorithm (every ~10 s)
    /// 5. Apply choke/unchoke decisions to upload sessions
    /// 6. Remove dead sessions, report progress
    /// 7. Yield to the tokio runtime
    ///
    /// Mirrors C++ `SeedCheckCommand::execute()` periodic loop combined
    /// with upload session management.
    pub async fn run_seeding_loop(&mut self) -> crate::error::Result<()> {
        info!(
            info_hash = ?self.info_hash,
            "Seeding loop started (ratio={:?}, time={:?}, peers={})",
            self.exit_condition.seed_ratio,
            self.exit_condition.seed_time,
            self.upload_sessions.len()
        );

        let mut tick = tokio::time::interval(SEED_TICK_INTERVAL);

        loop {
            // -- Cancellation check (non-blocking) ----------------------------
            if self.cancel_token.is_cancelled() {
                info!("Seeding loop cancelled via cancellation token");
                break;
            }

            // -- Exit condition check ----------------------------------------
            if self.should_stop_seeding() {
                self.halt_requested = true;
                info!(
                    "Seed exit conditions met (uploaded={}, downloaded={}, duration={:?})",
                    self.total_uploaded,
                    self.total_downloaded,
                    self.seeding_duration()
                );
                break;
            }

            // -- Tracker re-announce (mirrors C++ SeedCheckCommand keeping the
            // swarm informed via BtAnnounce; the state machine throttles by
            // the tracker-provided interval) ----------------------------------
            if let Some(announcer) = self.announcer.as_mut()
                && announcer.is_default_announce_ready()
                && let Some(result) = announcer
                    .announce(
                        &self.info_hash,
                        &self.peer_id,
                        self.total_downloaded,
                        0,
                        self.total_uploaded,
                    )
                    .await
            {
                debug!(
                    "[Seed] Re-announced to {} ({} seeders, {} leechers)",
                    result.tracker_url, result.seeders, result.leechers
                );
            }

            // The listener remains active after the payload is complete. This
            // is the Rust equivalent of PeerListenCommand continuing beside
            // SeedCheckCommand, including when no peer was connected at the
            // instant the download finished.
            self.drain_incoming_peers();

            // -- Handle incoming messages from peers --------------------------
            self.handle_peer_messages().await;

            // -- Remove dead sessions ----------------------------------------
            self.remove_dead_sessions();

            // -- Sync upload-session state → PeerStats ------------------------
            self.sync_sessions_to_stats();

            // -- Execute choking algorithm (every ~10 s) ----------------------
            if self.last_choke_time.elapsed().as_secs() >= CHOKE_ROUND_INTERVAL_SECS {
                self.run_choke_round();
                self.last_choke_time = Instant::now();
            }

            // -- Apply choke decisions to upload sessions ---------------------
            self.apply_choke_decisions().await;

            // -- Wait for next tick (or cancellation) -------------------------
            tokio::select! {
                _ = tick.tick() => {}
                _ = self.cancel_token.cancelled() => {
                    info!("Seeding loop cancelled during tick wait");
                    break;
                }
            }
        }

        if let Some(announcer) = self.announcer.as_mut() {
            announcer
                .announce_stopped(
                    &self.info_hash,
                    &self.peer_id,
                    self.total_downloaded,
                    0,
                    self.total_uploaded,
                )
                .await;
        }
        self.is_active = false;
        info!(
            "Seeding loop ended: uploaded {} bytes in {:?}",
            self.total_uploaded,
            self.seeding_duration()
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal loop helpers
    // -----------------------------------------------------------------------

    /// Admit handshaken peers that arrive while the torrent is seeding.
    fn drain_incoming_peers(&mut self) {
        let incoming = self
            .incoming_peers
            .as_mut()
            .map(|receiver| std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>())
            .unwrap_or_default();
        let Some(provider) = self.piece_provider.as_ref() else {
            return;
        };
        let num_pieces = provider.num_pieces();
        let piece_length = provider.piece_length();

        for incoming in incoming {
            let endpoint = incoming.endpoint;
            let remote_peer_id = incoming.connection.remote_peer_id();
            let duplicate = remote_peer_id.is_some_and(|peer_id| {
                peer_id == self.peer_id
                    || self
                        .upload_sessions
                        .iter()
                        .any(|session| session.remote_peer_id() == Some(peer_id))
            }) || self.upload_sessions.iter().any(|session| {
                session.endpoint() == Some((endpoint.ip().to_string(), endpoint.port()))
            });

            if duplicate {
                self.release_peer(endpoint);
                debug!(%endpoint, "Rejected duplicate or self BitTorrent seed peer");
                continue;
            }

            let transport = match incoming.connection {
                aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Plain(
                    connection,
                ) => BtUploadConnection::Plain(connection),
                aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Encrypted(
                    connection,
                ) => BtUploadConnection::Encrypted(connection),
            };
            let mut session = BtUploadSession::new_with_connection(transport, &self.config);
            session.configure_message_validator(num_pieces, piece_length);
            let peer_stats = PeerStats::new(remote_peer_id.unwrap_or([0u8; 20]), endpoint);
            self.upload_sessions.push(session);
            self.peer_stats.push(peer_stats);
            info!(%endpoint, "Admitted incoming BitTorrent seed peer");
        }
    }

    fn release_peer(&self, endpoint: std::net::SocketAddr) {
        if let Some(peer_storage) = &self.peer_storage {
            peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
        }
    }

    /// Handle incoming messages from all active upload sessions.
    ///
    /// Each session gets a bounded time window (`MSG_READ_TIMEOUT`) to
    /// process incoming messages. This prevents a single slow peer from
    /// blocking the entire seeding loop.
    async fn handle_peer_messages(&mut self) {
        let provider = match self.piece_provider.as_ref() {
            Some(p) => Arc::clone(p),
            None => return,
        };

        let mut uploaded = 0u64;
        for session in &mut self.upload_sessions {
            if session.is_dead() {
                continue;
            }
            match tokio::time::timeout(
                MSG_READ_TIMEOUT,
                session.handle_incoming_messages(provider.as_ref()),
            )
            .await
            {
                Ok(Ok(bytes_uploaded)) => {
                    if bytes_uploaded > 0 {
                        debug!("Uploaded {} bytes to peer", bytes_uploaded);
                        uploaded = uploaded.saturating_add(bytes_uploaded);
                    }
                }
                Ok(Err(e)) => {
                    warn!("Upload session error: {}", e);
                }
                Err(_) => {
                    // Timeout is expected: we move on to the next session
                }
            }
        }
        self.total_uploaded = self.total_uploaded.saturating_add(uploaded);
    }

    /// Remove upload sessions whose connections have died.
    fn remove_dead_sessions(&mut self) {
        let before = self.upload_sessions.len();
        // Collect indices of dead sessions
        let dead_indices: Vec<usize> = self
            .upload_sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_dead())
            .map(|(i, _)| i)
            .collect();

        // Remove in reverse order to keep indices stable
        for idx in dead_indices.into_iter().rev() {
            if let Some((ip, port)) = self.upload_sessions[idx].endpoint()
                && let Some(peer_storage) = &self.peer_storage
            {
                peer_storage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .return_peer_by_endpoint(&ip, port);
            }
            self.upload_sessions.remove(idx);
            if idx < self.peer_stats.len() {
                self.peer_stats.remove(idx);
            }
        }

        // Keep the parallel statistics vector aligned even if it was already
        // out of sync before dead sessions were removed.
        self.peer_stats.truncate(self.upload_sessions.len());

        let removed = before - self.upload_sessions.len();
        if removed > 0 {
            debug!("Removed {} dead upload sessions", removed);
        }
    }

    /// Sync state from upload sessions to PeerStats.
    ///
    /// The upload sessions own the authoritative `peer_interested` and
    /// `uploaded_bytes` values (updated by incoming message handling).
    /// Before running the choking algorithm, we propagate these values
    /// to the PeerStats so the algorithm sees the latest state.
    fn sync_sessions_to_stats(&mut self) {
        let len = self.upload_sessions.len().min(self.peer_stats.len());
        for i in 0..len {
            let session = &self.upload_sessions[i];
            let stats = &mut self.peer_stats[i];
            stats.peer_interested = session.is_peer_interested();
            stats.uploaded_bytes = session.uploaded_bytes();
            // Estimate upload speed from session's bytes and elapsed time
            let elapsed = self.seeding_start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                stats.upload_speed = session.uploaded_bytes() as f64 / elapsed;
            }
        }
    }

    /// Run one round of the seeder-state choking algorithm.
    fn run_choke_round(&mut self) {
        // Take ownership of peer_stats temporarily so the choking algorithm
        // can modify them through &mut references without aliasing self.
        let mut peer_stats = std::mem::take(&mut self.peer_stats);

        // Build mutable slice references for the choking algorithm
        let mut peers_mut: Vec<&mut PeerStats> = peer_stats.iter_mut().collect();
        self.seeder_choke.execute_choke(&mut peers_mut[..]);

        // Restore peer_stats
        self.peer_stats = peer_stats;
    }

    /// Apply choke/unchoke decisions from PeerStats to upload sessions.
    ///
    /// After the choking algorithm sets `am_choking` on PeerStats, we send
    /// the corresponding Choke/Unchoke messages to peers via their upload
    /// sessions.
    async fn apply_choke_decisions(&mut self) {
        let len = self.upload_sessions.len().min(self.peer_stats.len());
        for i in 0..len {
            let stats_am_choking = self.peer_stats[i].am_choking;
            let session = &mut self.upload_sessions[i];

            if stats_am_choking && !session.is_peer_choked() {
                // Need to choke this peer
                if let Err(e) = session.choke_peer().await {
                    warn!("Failed to choke peer: {}", e);
                }
            } else if !stats_am_choking && session.is_peer_choked() {
                // Need to unchoke this peer
                if let Err(e) = session.unchoke_peer().await {
                    warn!("Failed to unchoke peer: {}", e);
                }
            }
        }
    }

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
        let elapsed_secs = self.seeding_start_time.elapsed().as_secs();
        let upload_speed = self.total_uploaded.checked_div(elapsed_secs).unwrap_or(0);
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
        self.total_uploaded += bytes;
    }

    /// Cancel the seeding loop (external shutdown signal).
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// Get a clone of the cancellation token for external observers.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria2_protocol::bittorrent::peer::connection::PeerConnection;
    use aria2_protocol::bittorrent::peer::incoming::IncomingConnection;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn seeding_accepts_a_peer_after_download_has_no_initial_peers() {
        let provider =
            Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(1024, 1));
        let (sender, receiver) = mpsc::channel(1);
        let mut manager = BtSeedManager::new_with_transports(
            [7u8; 20],
            Vec::new(),
            provider,
            BtSeedingConfig::default(),
            SeedExitCondition::infinite(),
            1024,
            None,
            None,
            [1u8; 20],
            Some(receiver),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client_task = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
        let (server_stream, endpoint) = listener.accept().await.unwrap();
        let _client_stream = client_task.await.unwrap();
        let peer_connection = PeerConnection::from_stream_with_peer(server_stream, [2u8; 20]);
        sender
            .send(crate::engine::bt_peer_listener::IncomingPeer {
                connection: IncomingConnection::Plain(Box::new(peer_connection)),
                endpoint,
            })
            .await
            .unwrap();

        manager.drain_incoming_peers();

        assert_eq!(manager.num_sessions(), 1);
    }

    #[tokio::test]
    async fn seeding_does_not_end_just_because_all_peers_disconnect() {
        let provider =
            Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(1024, 1));
        let mut manager = BtSeedManager::new(
            Vec::new(),
            provider,
            BtSeedingConfig::default(),
            SeedExitCondition::with_time(1),
            1024,
        );
        let cancel = manager.cancellation_token();
        let task = tokio::spawn(async move {
            let result = manager.run_seeding_loop().await;
            (result, manager.seeding_duration(), manager.halt_requested())
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let (_, duration, halt_requested) = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();

        assert!(duration >= Duration::from_millis(40));
        assert!(!halt_requested);
    }
}

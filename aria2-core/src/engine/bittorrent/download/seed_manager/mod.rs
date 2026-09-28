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
//! - [`BtSeedManager`] — Top-level seeding manager that owns seeding peer
//!   actors and runs until exit conditions are met.
//! - [`SeedExitCondition`] — Conditions under which seeding should stop
//!   (time limit, ratio limit, or infinite).
//!
//! # Seeding Loop
//!
//! The loop waits on peer sockets, incoming peers, cancellation, or the next
//! protocol deadline. Each event performs the smallest required state update:
//! 1. Check cancellation / exit conditions
//! 2. Admit incoming peers or process one ready peer message
//! 3. Sync peer-worker state -> PeerStats
//! 4. Run the seeder-state choking algorithm on its deadline, peer-state
//!    mismatch, or return of an unchoked and interested peer
//! 5. Apply choke/unchoke decisions through peer-worker command channels
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
mod seeding_connections;
mod seeding_loop;
#[cfg(test)]
mod tests;
pub mod types;

// Re-export the exit-condition type from the types submodule for convenience.
pub use types::SeedExitCondition;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::engine::bittorrent::download::execute::BtTrackerAnnouncerActor;
use crate::engine::bittorrent::download::execute::DhtPeriodicLookup;
use crate::engine::bittorrent::peer::choke_manager::BtSeederStateChoke;
use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::interaction::BtPeerConnectionOptions;
use crate::engine::bittorrent::peer::message_handler::PeerSwarm;
use crate::engine::bittorrent::peer::upload_session::{BtSeedingConfig, PieceDataProvider};
use crate::engine::bittorrent::tracker::communication::TrackerAnnouncer;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{AtomicProgress, BtPeerSnapshot, ConnectionState};

/// Discovery and connection state transferred from download execution to the
/// long-lived seeding coordinator. This keeps torrent-scoped peer discovery
/// alive across the leech-to-seed transition.
pub(crate) struct SeedPeerDiscovery {
    pub(crate) group: Arc<std::sync::RwLock<crate::request::request_group::RequestGroup>>,
    pub(crate) dht_engines: crate::engine::bittorrent::dht::engine_set::DhtEngineSet,
    pub(crate) dht_lookup: DhtPeriodicLookup,
    pub(crate) listen_port: u16,
    pub(crate) connection_options: BtPeerConnectionOptions,
    pub(crate) total_size: u64,
    pub(crate) utp_socket:
        Option<Arc<tokio::sync::Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>>,
    pub(crate) outbound_network_policy: Arc<crate::network::OutboundNetworkPolicy>,
    pub(crate) enable_peer_exchange: bool,
}

pub(super) struct SeedPeerConnectionAttempt {
    pub(super) task: tokio::task::JoinHandle<
        crate::error::Result<crate::engine::bittorrent::peer::interaction::PeerConnectionResult>,
    >,
    pub(super) checked_out: Vec<crate::engine::bittorrent::peer::storage::PeerEntry>,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Interval between choke rounds (seconds). Matches C++ rotation interval.
const CHOKE_ROUND_INTERVAL_SECS: u64 = 10;
const PEER_UPLOAD_SPEED_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

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
/// Mirrors C++ `SeedCheckCommand` combined with peer connection management.
/// Top-level manager for the BitTorrent seeding phase.
pub struct BtSeedManager {
    /// Info hash of the torrent being seeded
    info_hash: [u8; 20],
    /// Synchronous constructors stage established transports until the async
    /// loop starts; after startup, every live connection belongs to the swarm.
    pending_connections: Vec<BtPeerConn>,
    swarm: PeerSwarm,
    /// Piece data provider for reading completed pieces from disk
    piece_provider: Arc<dyn PieceDataProvider>,
    /// Seeding configuration (rate limits, unchoke settings)
    #[allow(dead_code)]
    config: BtSeedingConfig,
    /// Exit condition (ratio/time/infinite)
    exit_condition: SeedExitCondition,
    /// Total bytes downloaded (for ratio calculation)
    total_downloaded: u64,
    /// Total bytes uploaded during seeding
    pub total_uploaded: u64,
    /// Authoritative upload counter shared with the connection-owned workers.
    upload_counter: Arc<AtomicU64>,
    /// Torrent-wide upload limiter shared with the download-phase actors.
    torrent_upload_limiter: RateLimiter,
    /// When seeding started
    pub seeding_start_time: Instant,
    /// Whether seeding is currently active
    is_active: bool,
    /// Seeder-state choking algorithm
    seeder_choke: BtSeederStateChoke,
    /// Cancellation token for graceful shutdown
    cancel_token: CancellationToken,
    /// Shared peer storage used to release seeding sessions on disconnect.
    peer_storage: Option<
        std::sync::Arc<
            std::sync::Mutex<crate::engine::bittorrent::peer::storage::DefaultPeerStorage>,
        >,
    >,
    /// DHT/tracker/PEX discovered peers are connected through the same swarm
    /// after the piece scheduler has ended.
    peer_discovery: Option<SeedPeerDiscovery>,
    /// At most one bounded outgoing batch is active. Checked-out peer entries
    /// remain here until its result is reconciled into the swarm.
    pending_peer_connection: Option<SeedPeerConnectionAttempt>,
    /// Set when seed criteria ends the runtime, matching BtRuntime::halt.
    halt_requested: bool,
    /// Timestamp of the last choke round
    last_choke_time: Instant,
    /// Tracker announcer for periodic re-announce while seeding
    /// (mirrors C++ SeedCheckCommand keeping the swarm informed).
    announcer: Option<Arc<tokio::sync::Mutex<TrackerAnnouncer>>>,
    /// The production download path keeps one tracker actor across the
    /// leech-to-seed handoff; direct public constructors are adapted on run.
    tracker_actor: Option<BtTrackerAnnouncerActor>,
    pending_tracker_announce: Option<
        tokio::task::JoinHandle<
            Option<crate::engine::bittorrent::tracker::communication::AnnounceResult>,
        >,
    >,
    /// Our peer id, sent with tracker announces.
    peer_id: [u8; 20],
    /// Incoming peers routed to this torrent while it remains in seeding mode.
    incoming_peers: Option<crate::engine::bittorrent::peer::listener::IncomingPeerReceiver>,
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

    /// Whether a seed criterion requested runtime halt.
    pub fn halt_requested(&self) -> bool {
        self.halt_requested
    }

    /// Return total bytes uploaded during seeding.
    pub fn total_uploaded(&self) -> u64 {
        self.total_uploaded
    }

    pub fn take_announcer(&mut self) -> Option<TrackerAnnouncer> {
        let announcer = self.announcer.take()?;
        match Arc::try_unwrap(announcer) {
            Ok(announcer) => Some(announcer.into_inner()),
            Err(announcer) => {
                self.announcer = Some(announcer);
                None
            }
        }
    }

    /// Return total bytes downloaded (used for seed ratio calculation).
    pub fn total_downloaded(&self) -> u64 {
        self.total_downloaded
    }

    /// Return upload statistics: (total_uploaded, upload_speed).
    pub fn get_upload_stats(&self) -> (u64, u64) {
        (self.total_uploaded, self.current_upload_speed())
    }

    pub(super) fn current_upload_speed(&self) -> u64 {
        let now = Instant::now();
        self.swarm
            .iter()
            .filter(|actor| {
                actor.stats.last_upload_time.is_some_and(|last_upload| {
                    now.saturating_duration_since(last_upload) < PEER_UPLOAD_SPEED_IDLE_TIMEOUT
                })
            })
            .map(|actor| actor.stats.upload_speed.max(0.0) as u64)
            .sum()
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

    /// Return the number of connected peers owned by this manager.
    pub fn num_sessions(&self) -> usize {
        self.pending_connections.len() + self.swarm.len()
    }

    /// Record bytes uploaded to a peer.
    pub fn record_upload(&mut self, bytes: u64) {
        self.total_uploaded = self.total_uploaded.saturating_add(bytes);
    }

    /// Restore upload statistics when a paused BT command resumes seeding.
    pub(crate) fn set_total_uploaded(&mut self, total_uploaded: u64) {
        self.total_uploaded = total_uploaded;
        self.upload_counter
            .store(total_uploaded, std::sync::atomic::Ordering::Relaxed);
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
        if let Some(actor) = self.tracker_actor.as_ref() {
            actor.set_active_connections(self.swarm.len());
        }
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
        let mut snapshots = self
            .pending_connections
            .iter()
            .filter_map(|session| {
                let addr = session.remote_endpoint()?;
                Some(BtPeerSnapshot {
                    peer_id: session.remote_peer_id().unwrap_or([0; 20]),
                    client: session
                        .remote_client
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone(),
                    addr,
                    is_incoming: session.incoming,
                    source: session.source,
                    bitfield: None,
                    uploaded_bytes: session.stats.uploaded_bytes,
                    downloaded_bytes: 0,
                    upload_speed: session.stats.upload_speed,
                    download_speed: 0.0,
                    avg_upload_speed: session.stats.avg_upload_speed,
                    avg_download_speed: 0,
                    am_choking: session.stats.am_choking,
                    peer_choking: false,
                    am_interested: session.stats.am_interested,
                    peer_interested: session.stats.peer_interested,
                    outstanding_upload_requests: session.stats.outstanding_upload_count,
                    outstanding_download_requests: 0,
                    seeder: Some(false),
                    connection_duration_secs: session.stats.connection_duration_secs(),
                    last_data_age_secs: session
                        .stats
                        .last_data_time
                        .map(|time| time.elapsed().as_secs())
                        .unwrap_or_else(|| session.stats.connection_duration_secs()),
                    is_snubbed: session.stats.is_snubbed,
                    is_banned: false,
                })
            })
            .collect::<Vec<_>>();
        snapshots.extend(self.swarm.iter().map(|actor| {
            let addr = actor.endpoint;
            let stats = &actor.stats;
            BtPeerSnapshot {
                peer_id: stats.peer_id,
                client: actor
                    .client
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
                addr,
                is_incoming: actor.incoming,
                source: actor.source,
                bitfield: actor.has_bitfield.then(|| actor.bitfield.clone()),
                uploaded_bytes: stats.uploaded_bytes,
                downloaded_bytes: stats.downloaded_bytes,
                upload_speed: stats.upload_speed,
                download_speed: stats.download_speed,
                avg_upload_speed: stats.avg_upload_speed,
                avg_download_speed: stats.avg_download_speed,
                am_choking: stats.am_choking,
                peer_choking: stats.peer_choking,
                am_interested: stats.am_interested,
                peer_interested: stats.peer_interested,
                outstanding_upload_requests: stats.outstanding_upload_count,
                outstanding_download_requests: actor
                    .pending_download_requests
                    .load(std::sync::atomic::Ordering::Relaxed),
                seeder: Some(actor.seeder),
                connection_duration_secs: stats.connection_duration_secs(),
                last_data_age_secs: stats
                    .last_data_time
                    .map(|time| time.elapsed().as_secs())
                    .unwrap_or_else(|| stats.connection_duration_secs()),
                is_snubbed: stats.is_snubbed,
                is_banned: false,
            }
        }));
        snapshots
    }

    fn publish_upload_stats(&self) {
        if let Some(progress) = self.upload_progress.as_ref() {
            progress.set_upload_length(self.total_uploaded);
            progress.set_upload_speed(self.get_upload_stats().1);
        }
    }
}

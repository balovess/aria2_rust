use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::engine::bt_choke_manager::BtSeederStateChoke;
use crate::engine::bt_piece_downloader::FileBackedPieceProvider;
use crate::engine::bt_tracker_comm::TrackerAnnouncer;
use crate::engine::bt_upload_session::{
    BtSeedingConfig, BtUploadConnection, BtUploadSession, PieceDataProvider,
};
use crate::engine::choking_algorithm::ChokingAlgorithm;
use crate::engine::peer_stats::PeerStats;

use super::{BtSeedManager, CHOKE_ROUND_INTERVAL_SECS, SeedExitCondition};

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
            // Run the first choke round immediately after the first peer state
            // update.  A newly admitted interested peer should not wait for
            // the full rotation interval before receiving an unchoke.
            last_choke_time: Instant::now() - Duration::from_secs(CHOKE_ROUND_INTERVAL_SECS),
            announcer,
            peer_id,
            incoming_peers,
            upload_progress: None,
            connection_state: None,
            peer_snapshot_store: None,
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
}

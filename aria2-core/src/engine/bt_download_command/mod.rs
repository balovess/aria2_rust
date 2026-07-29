mod feature_api;
mod peer_methods;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::constants;
use crate::download::DownloadContext;
use crate::engine::bt_progress_info_file::BtProgressManager;
use crate::engine::bt_tracker_comm::announce_to_public_tracker;
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::http_tracker_client::TrackerState;
use crate::engine::lpd_manager::LpdManager;
use crate::engine::multi_file_layout::MultiFileLayout;
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::file_lock::DownloadPathLock;
use crate::request::request_group::{AtomicProgress, DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

// Re-export public items from other modules that were previously re-exported here.
pub use crate::engine::bt_message_handler::{
    BLOCK_REQUEST_TIMEOUT_SECS, BLOCK_SIZE, MAX_BLOCK_READ_MESSAGES, MAX_RETRIES,
};
pub use crate::engine::bt_peer_interaction::{
    MAX_UNCHOKE_WAIT_ATTEMPTS, PEER_CONNECTION_DELAY_MS, PEER_MESSAGE_TIMEOUT_SECS,
};
pub use crate::engine::bt_piece_selector::ENDGAME_THRESHOLD;

// Re-export items from submodules so external code can still use
// `crate::engine::bt_download_command::BtDownloadCommand` etc.
pub use feature_api::SeedStats;
pub use peer_methods::{BAD_DATA_THRESHOLD, BAD_DATA_THRESHOLD as _BAD_DATA_THRESHOLD_REEXPORT};

pub(crate) const PUBLIC_TRACKER_PEER_THRESHOLD: usize = 15;
pub(crate) const MAX_PUBLIC_TRACKERS_TO_TRY: usize = 10;

/// BitTorrent download command — holds all state for a single torrent download.
pub struct BtDownloadCommand {
    pub(crate) group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters — avoids `RwLock` on the hot path.
    pub(crate) progress: Arc<AtomicProgress>,
    pub(crate) output_path: std::path::PathBuf,
    pub(crate) started: bool,
    pub(crate) completed_bytes: u64,
    pub(crate) torrent_data: Vec<u8>,
    pub(crate) seed_enabled: bool,
    pub(crate) seed_time: Option<std::time::Duration>,
    pub(crate) seed_ratio: Option<f64>,
    pub(crate) total_uploaded: u64,
    pub(crate) udp_client: Option<crate::engine::udp_tracker_client::SharedUdpClient>,
    /// Unified tracker announcer (HTTP + UDP) using BtAnnounce state machine.
    /// Created during execute() from the torrent's announce list.
    pub(crate) tracker_announcer: Option<crate::engine::bt_tracker_comm::TrackerAnnouncer>,
    pub(crate) dht_engine:
        Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    pub(crate) public_trackers:
        Option<std::sync::Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>>,
    pub(crate) choking_algo: Option<ChokingAlgorithm>,
    pub(crate) multi_file_layout: Option<MultiFileLayout>,

    // P1/P2 integration fields (all use Option for backward compatibility)
    /// BT progress persistence manager
    pub(crate) progress_manager: Option<BtProgressManager>,
    /// Progress save interval (default 60 seconds)
    pub(crate) progress_save_interval: Duration,
    /// LPD LAN peer discovery manager
    pub(crate) lpd_manager: Option<Arc<LpdManager>>,
    /// Post-download handler manager
    pub(crate) hook_manager: Option<Arc<crate::engine::hook_manager::HookManager>>,

    // PEX (Peer Exchange, BEP 11) integration fields
    /// Track known peers for PEX exchange
    pub(crate) pex_known_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    /// Timestamp of last PEX message sent (for rate limiting)
    pub(crate) pex_last_send_time: Option<Instant>,
    /// Interval between PEX messages (default 60 seconds)
    pub(crate) pex_send_interval: Duration,

    // Endgame mode (Phase 14 - B1/B2): duplicate request tracking for final pieces
    /// Tracks duplicate block requests during endgame mode
    pub(crate) endgame_state: super::bt_download_execute::EndgameState,

    // BEP 6 (Fast Extension): track AllowedFast messages sent to peers
    /// Track which AllowedFast pieces have been sent to each peer
    /// Key: peer identifier (using connection index for now)
    #[allow(dead_code)]
    pub(crate) allowed_fast_sent_peers: HashMap<usize, HashSet<u32>>,

    /// Track suggest counts per peer to avoid spamming
    pub(crate) suggest_sent_counts: HashMap<usize, usize>,

    // Tracker event state machine (Phase 15 - H5): manages Started/Completed/Stopped events
    /// State machine for tracker announce events
    #[allow(dead_code)]
    pub(crate) tracker_state: TrackerState,

    // Web-seed (BEP 19 / HTTP fallback) integration
    /// URLs extracted from torrent's url-list field for HTTP piece download fallback
    pub(crate) web_seed_urls: Vec<String>,
    /// Web seed manager for HTTP piece downloads (initialized on first use)
    pub(crate) web_seed_manager: Option<crate::engine::bt_web_seed::WebSeedManager>,

    // Periodic DHT peer lookup (C++ DHTGetPeersCommand)
    /// Tracks timing and retry state for periodic DHT get_peers lookups.
    /// C++: `DHTGetPeersCommand` runs as a per-torrent command that
    /// triggers DHT lookups at adaptive intervals (15min normal,
    /// 5min low peers, 1min zero peers, 5s retry).
    /// TODO: Wire into BT download loop for periodic DHT peer discovery.
    #[allow(dead_code)]
    pub(crate) dht_periodic_lookup: super::bt_download_execute::execute::DhtPeriodicLookup,

    // File lock (J6): prevents concurrent aria2 instances from writing to same output dir
    /// Download path lock held for the lifetime of this command.
    /// Prevents other aria2 instances from writing to the same output directory.
    pub download_path_lock: Option<DownloadPathLock>,

    // Seeding mode (Phase 16 - Complete BitTorrent seeding)
    /// Seed manager for uploading after download completes
    pub(crate) seed_manager: Option<super::bt_seed_manager::BtSeedManager>,

    // BEP 0027 (Private Torrent): when true, DHT/PEX/LPD and public tracker
    // announcement are disabled to enforce the privacy guarantees of the
    // torrent's `private` flag.
    pub(crate) is_private: bool,

    // BtRegistry integration: the command registers itself into the engine's
    // BtRegistry during execute() so that info-hash reverse lookup, peer
    // blocklist, and cross-download coordination work end-to-end.
    // Set via `set_bt_registry()` after construction by the engine or caller.
    pub(crate) bt_registry: Option<Arc<std::sync::RwLock<super::bt_registry::BtRegistry>>>,
}

impl BtDownloadCommand {
    pub fn new(
        gid: GroupId,
        torrent_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(torrent_bytes)
            .map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!("Torrent parse failed: {}", e)))
        })?;

        // BEP 0027 (Private Torrent): capture the private flag at parse time.
        // When true, the engine must disable DHT, PEX, LPD and public tracker
        // announcement to honour the privacy contract.
        let is_private = meta.is_private();
        if is_private {
            info!(
                "[BT] Private torrent detected (BEP 0027): DHT/PEX/LPD and public trackers will be disabled"
            );
        }

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let filename = meta.info.name.clone();
        let path = std::path::PathBuf::from(&dir).join(&filename);

        let group = RequestGroup::new(
            gid,
            vec![format!("bt://{}", meta.info_hash.as_hex())],
            options.clone(),
        );

        // Set BT metadata for session persistence (Task 3)
        group.set_bt_metadata(
            meta.num_pieces() as u32,
            meta.info.piece_length,
            meta.info_hash.as_hex(),
        );

        // Create DownloadContext from torrent metadata and set TorrentAttribute.
        // In C++ aria2, this is done by `bittorrent_helper::processRootDictionary()`
        // which calls `ctx->setAttribute(CTX_ATTR_BT, torrent)` with all torrent
        // metadata fields. We replicate this here.
        {
            use crate::download::download_context::{
                BtFileMode, ContextAttributeType, TorrentAttribute,
            };

            let total_size = meta.total_size();
            let piece_length = meta.info.piece_length;
            let file_path_str = path.to_string_lossy().to_string();

            // Create DownloadContext with piece length, total size, and output path
            let mut ctx = DownloadContext::new(piece_length, total_size, file_path_str);

            // Set piece hashes from torrent info dict (sha-1 hashes in hex format)
            let piece_hashes_hex: Vec<String> = meta
                .info
                .pieces
                .iter()
                .map(hex::encode)
                .collect();
            ctx.set_piece_hashes("sha-1".to_string(), piece_hashes_hex);

            // Build TorrentAttribute from torrent metadata (C++ TorrentAttribute fields)
            let bt_file_mode = if meta.is_single_file() {
                BtFileMode::Single
            } else {
                BtFileMode::Multi
            };
            let torrent_attr = TorrentAttribute {
                name: meta.info.name.clone(),
                mode: bt_file_mode,
                announce_list: meta.announce_list.clone(),
                nodes: Vec::new(), // DHT nodes not parsed from TorrentMeta yet
                info_hash: meta.info_hash.as_hex(),
                metadata: Vec::new(), // Regular torrent has metadata on disk, not via ut_metadata
                metadata_size: 0,
                private_torrent: meta.is_private(),
                creation_date: meta.creation_date.unwrap_or(0),
                comment: meta.comment.clone().unwrap_or_default(),
                created_by: meta.created_by.clone().unwrap_or_default(),
                url_list: meta.web_seeds.clone(),
            };

            ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(torrent_attr));

            group.set_download_context(std::sync::Arc::new(ctx));
        }

        let seed_time = options.seed_time.and_then(|t| {
            if t == 0.0 {
                None
            } else {
                Some(std::time::Duration::from_secs_f64(t))
            }
        });
        let seed_ratio = options.seed_ratio.filter(|&r| r > 0.0);

        info!(
            "BtDownloadCommand created: {} -> {} ({} bytes, {} pieces) seed={:?} ratio={:?}",
            meta.info.name,
            path.display(),
            meta.total_size(),
            meta.num_pieces(),
            seed_time,
            seed_ratio
        );

        let choking_algo = if options.bt_max_upload_slots.is_some()
            || options.bt_optimistic_unchoke_interval.is_some()
            || options.bt_snubbed_timeout.is_some()
        {
            let config = ChokingConfig {
                max_upload_slots: options
                    .bt_max_upload_slots
                    .unwrap_or(constants::BT_DEFAULT_MAX_UPLOAD_SLOTS as u32)
                    as usize,
                optimistic_unchoke_interval_secs: options
                    .bt_optimistic_unchoke_interval
                    .unwrap_or(constants::BT_OPTIMISTIC_UNCHOKE_INTERVAL_SECS),
                snubbed_timeout_secs: options
                    .bt_snubbed_timeout
                    .unwrap_or(constants::BT_SNUBBED_TIMEOUT_SECS),
                choke_rotation_interval_secs: constants::BT_CHOKE_ROTATION_INTERVAL_SECS,
            };
            Some(ChokingAlgorithm::new(config))
        } else {
            None
        };

        let multi_file_layout = if !meta.is_single_file() {
            let layout_base_dir = std::path::PathBuf::from(&dir);
            match MultiFileLayout::from_info_dict(&meta.info, &layout_base_dir) {
                Ok(layout) => Some(layout),
                Err(e) => {
                    return Err(Aria2Error::Fatal(FatalError::Config(format!(
                        "MultiFileLayout creation failed: {}",
                        e
                    ))));
                }
            }
        } else {
            None
        };

        let effective_output_path = if multi_file_layout.is_some() {
            std::path::PathBuf::from(&dir)
        } else {
            path.clone()
        };

        info!(
            "BtDownloadCommand created: {} -> {} ({} bytes, {} pieces) seed={:?} ratio={:?} multi_file={}",
            meta.info.name,
            effective_output_path.display(),
            meta.total_size(),
            meta.num_pieces(),
            seed_time,
            seed_ratio,
            multi_file_layout.is_some()
        );

        // Acquire download path lock (J6): prevents concurrent instances from
        // writing to the same output directory. If acquisition fails, log a
        // warning but do not fail the download -- the lock is a best-effort guard.
        // NOTE: always pass the output DIRECTORY, not the file path. For
        // single-file torrents `effective_output_path` is `dir/filename` (a file
        // path); passing it to acquire_for_download would cause create_dir_all to
        // create `filename` as a directory, which then makes File::create fail
        // with "Access denied" (os error 5) on Windows.
        let download_path_lock =
            match DownloadPathLock::acquire_for_download(std::path::Path::new(&dir)) {
                Ok(lock) => Some(lock),
                Err(e) => {
                    warn!(
                        "Failed to acquire download path lock: {}. Proceeding without lock.",
                        e
                    );
                    None
                }
            };

        let progress = group.progress.clone();
        Ok(Self {
            group: Arc::new(std::sync::RwLock::new(group)),
            progress,
            output_path: effective_output_path,
            started: false,
            completed_bytes: 0,
            torrent_data: torrent_bytes.to_vec(),
            seed_enabled: options.seed_time.unwrap_or(0.0) > 0.0
                || options.seed_ratio.unwrap_or(0.0) > 0.0,
            seed_time,
            seed_ratio,
            total_uploaded: 0,
            udp_client: None,
            tracker_announcer: None,
            dht_engine: None,
            public_trackers: None,
            choking_algo,
            multi_file_layout,

            // P1/P2 integration field defaults (all None, backward compatible)
            progress_manager: None,
            progress_save_interval: Duration::from_secs(60),
            lpd_manager: None,
            hook_manager: None,

            // PEX integration fields default values
            pex_known_peers: Vec::new(),
            pex_last_send_time: None,
            pex_send_interval: Duration::from_secs(60),

            // BEP 6 Fast Extension tracking
            allowed_fast_sent_peers: HashMap::new(),
            suggest_sent_counts: HashMap::new(),

            // Endgame mode default values
            endgame_state: super::bt_download_execute::EndgameState::new(),

            // Tracker event state machine default
            tracker_state: TrackerState::new(),

            // Web-seed URLs (extracted from torrent url-list field)
            web_seed_urls: meta.web_seeds.clone(),
            // Web seed manager (initialized lazily when needed)
            web_seed_manager: None,

            // Periodic DHT peer lookup (C++ DHTGetPeersCommand)
            dht_periodic_lookup: super::bt_download_execute::execute::DhtPeriodicLookup::new(),

            // Download path lock (J6)
            download_path_lock,

            // Seeding mode
            seed_manager: None,

            // BEP 0027 (Private Torrent) enforcement flag
            is_private,

            // BtRegistry integration (set via set_bt_registry after construction)
            bt_registry: None,
        })
    }

    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.recover()
    }

    pub async fn announce_to_public_tracker(
        tracker_url: &str,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        total_size: u64,
    ) -> std::result::Result<Vec<(String, u16)>, String> {
        announce_to_public_tracker(tracker_url, info_hash, peer_id, total_size).await
    }

    /// Wrapper around [`crate::engine::bt_piece_downloader::write_piece_to_multi_files`].
    pub async fn write_piece_to_multi_files(
        layout: &MultiFileLayout,
        piece_idx: u32,
        piece_data: &[u8],
        piece_length: u32,
    ) -> Result<()> {
        crate::engine::bt_piece_downloader::write_piece_to_multi_files(
            layout,
            piece_idx,
            piece_data,
            piece_length,
        )
        .await
    }

    /// Wrapper around [`crate::engine::bt_piece_downloader::write_piece_to_multi_files_coalesced`].
    ///
    /// Prefer this over `write_piece_to_multi_files` for production use — it
    /// merges adjacent writes within a 4 KiB gap, reducing syscall count.
    pub async fn write_piece_to_multi_files_coalesced(
        layout: &MultiFileLayout,
        piece_idx: u32,
        piece_data: &bytes::Bytes,
        piece_length: u32,
    ) -> Result<()> {
        crate::engine::bt_piece_downloader::write_piece_to_multi_files_coalesced(
            layout,
            piece_idx,
            piece_data,
            piece_length,
        )
        .await
    }

    pub fn is_multi_file(&self) -> bool {
        self.multi_file_layout
            .as_ref()
            .is_some_and(|l| l.is_multi_file())
    }

    pub fn get_multi_file_layout(&self) -> Option<&MultiFileLayout> {
        self.multi_file_layout.as_ref()
    }
}

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::constants;
use crate::download::DownloadContext;
use crate::engine::bt_choke_manager::{
    add_peer_to_tracking, check_snubbed_peers, handle_snubbed_peer, on_data_received_from_peer,
    on_peer_choke, on_peer_unchoke, on_piece_received, select_best_peer_for_request,
};
use crate::engine::bt_post_download_handler::HookManager;
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

pub use crate::engine::bt_message_handler::{
    BLOCK_REQUEST_TIMEOUT_SECS, BLOCK_SIZE, MAX_BLOCK_READ_MESSAGES, MAX_RETRIES,
};
pub use crate::engine::bt_peer_interaction::{
    MAX_UNCHOKE_WAIT_ATTEMPTS, PEER_CONNECTION_DELAY_MS, PEER_MESSAGE_TIMEOUT_SECS,
};
pub use crate::engine::bt_piece_selector::ENDGAME_THRESHOLD;

pub(crate) const PUBLIC_TRACKER_PEER_THRESHOLD: usize = 15;
pub(crate) const MAX_PUBLIC_TRACKERS_TO_TRY: usize = 10;

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
    pub(crate) dht_engine:
        Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    pub(crate) public_trackers:
        Option<std::sync::Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>>,
    pub choking_algo: Option<ChokingAlgorithm>,
    pub multi_file_layout: Option<MultiFileLayout>,

    // P1/P2 integration fields (all use Option for backward compatibility)
    /// BT progress persistence manager
    pub(crate) progress_manager: Option<BtProgressManager>,
    /// Progress save interval (default 60 seconds)
    pub(crate) progress_save_interval: Duration,
    /// LPD LAN peer discovery manager
    pub(crate) lpd_manager: Option<Arc<LpdManager>>,
    /// Post-download handler manager
    pub(crate) hook_manager: Option<Arc<HookManager>>,

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
                .map(|hash_bytes| hex::encode(hash_bytes))
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

    pub fn on_peer_choke(&mut self, peer_idx: usize) {
        on_peer_choke(&mut self.choking_algo, peer_idx);
    }

    pub fn on_peer_unchoke(&mut self, peer_idx: usize) {
        on_peer_unchoke(&mut self.choking_algo, peer_idx);
    }

    pub fn on_data_received_from_peer(&mut self, peer_idx: usize, bytes: u64) {
        on_data_received_from_peer(&mut self.choking_algo, peer_idx, bytes);
    }

    pub fn check_snubbed_peers(&mut self) -> Vec<usize> {
        check_snubbed_peers(&mut self.choking_algo)
    }

    pub fn add_peer_to_tracking(&mut self, peer_id: [u8; 8], addr: std::net::SocketAddr) -> usize {
        add_peer_to_tracking(&mut self.choking_algo, peer_id, addr)
    }

    pub fn select_best_peer_for_request(&self) -> Option<usize> {
        select_best_peer_for_request(&self.choking_algo)
    }

    pub async fn handle_snubbed_peer(&mut self, peer_idx: usize) -> Result<()> {
        handle_snubbed_peer(&mut self.choking_algo, peer_idx)
            .await
            .map_err(|_| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "Failed to handle snubbed peer {}",
                    peer_idx
                )))
            })
    }

    pub fn on_piece_received(&mut self, peer_idx: usize, bytes: u64) {
        on_piece_received(&mut self.choking_algo, peer_idx, bytes as u32);
    }

    /// Explicitly mark a peer as snubbed (algorithm-level snubbing).
    ///
    /// This adds the peer to the explicit snubbed set, causing them to receive
    /// a score of -1000 on the next choke rotation, ensuring they are always choked.
    pub fn mark_peer_snubbed(&mut self, peer_idx: usize) {
        if let Some(algo) = &mut self.choking_algo {
            algo.mark_peer_snubbed(peer_idx);
        }
    }

    /// Check if a peer is explicitly snubbed at the algorithm level.
    pub fn is_explicitly_snubbed(&self, peer_idx: usize) -> bool {
        self.choking_algo
            .as_ref()
            .map(|a| a.is_explicitly_snubbed(peer_idx))
            .unwrap_or(false)
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

    // ==================== P1/P2 Integration API ====================

    /// Set the BT progress manager
    ///
    /// Enable BT download progress persistence for resume support.
    ///
    /// When enabled, the engine periodically saves piece completion bitfield,
    /// peer list, and download statistics to a `.aria2` file in INI format.
    /// On restart, the progress is loaded to skip already-completed pieces.
    ///
    /// # Arguments
    ///
    /// * `manager` - An initialized [`BtProgressManager`](super::bt_progress_info_file::BtProgressManager) instance
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use aria2_core::engine::bt_progress_info_file::BtProgressManager;
    /// use std::path::PathBuf;
    ///
    /// let save_dir = PathBuf::from("/tmp/aria2");
    /// let progress_mgr = BtProgressManager::new(&save_dir).expect("failed to create progress manager");
    /// // Pass progress_mgr to BtDownloadCommand::set_progress_manager()
    /// let _mgr = progress_mgr;
    /// ```
    pub fn set_progress_manager(&mut self, manager: BtProgressManager) {
        info!("BT progress manager enabled");
        self.progress_manager = Some(manager);
    }

    /// Set the interval (in seconds) between progress save operations.
    ///
    /// # Arguments
    ///
    /// * `interval_secs` - Save interval in seconds (default: 60)
    pub fn set_progress_save_interval(&mut self, interval_secs: u64) {
        self.progress_save_interval = Duration::from_secs(interval_secs);
        info!(interval_secs, "Progress save interval updated");
    }

    /// Enable Local Peer Discovery (LPD, BEP 14) for LAN peer finding.
    ///
    /// When enabled, the engine announces its active downloads via UDP multicast
    /// to `239.192.152.143:6771` and listens for peers on the same network.
    ///
    /// # Arguments
    ///
    /// * `manager` - An initialized [`LpdManager`](super::lpd_manager::LpdManager), wrapped in `Arc`
    pub fn set_lpd_manager(&mut self, manager: Arc<LpdManager>) {
        info!("LPD manager enabled for local peer discovery");
        self.lpd_manager = Some(manager);
    }

    /// Register a post-download hook chain for completion/error callbacks.
    ///
    /// Hooks execute sequentially after download completes or fails.
    /// Built-in hook types: Move, Rename, Touch, Exec (shell command).
    /// A single hook failure does not block subsequent hooks.
    ///
    /// # Arguments
    ///
    /// * `manager` - A configured [`HookManager`](super::bt_post_download_handler::HookManager) with registered hooks, wrapped in `Arc`
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::collections::HashMap;
    /// use std::sync::Arc;
    /// use aria2_core::engine::bt_post_download_handler::{HookManager, HookConfig, MoveHook, ExecHook};
    ///
    /// let config = HookConfig::default();
    /// let mut hooks = HookManager::new(config);
    /// hooks.add_hook(Box::new(MoveHook::new("/completed".into(), true)));
    /// hooks.add_hook(Box::new(ExecHook::new("notify.sh".into(), HashMap::new())));
    /// // Pass Arc::new(hooks) to BtDownloadCommand::set_hook_manager()
    /// let _hooks = Arc::new(hooks);
    /// ```
    pub fn set_hook_manager(&mut self, manager: Arc<HookManager>) {
        info!(
            hook_count = manager.hook_count(),
            "Hook manager enabled with {} hooks",
            manager.hook_count()
        );
        self.hook_manager = Some(manager);
    }

    /// Get progress manager reference (for testing and external access)
    pub fn get_progress_manager(&self) -> Option<&BtProgressManager> {
        self.progress_manager.as_ref()
    }

    /// Get LPD manager reference (for testing and external access)
    pub fn get_lpd_manager(&self) -> Option<&Arc<LpdManager>> {
        self.lpd_manager.as_ref()
    }

    /// Get hook manager reference (for testing and external access)
    pub fn get_hook_manager(&self) -> Option<&Arc<HookManager>> {
        self.hook_manager.as_ref()
    }

    /// Set the engine's BtRegistry reference for self-registration.
    ///
    /// When set, the command registers its [`DownloadContext`] and [`BtRuntime`]
    /// into the registry during [`execute()`], enabling info-hash reverse lookup,
    /// peer blocklist checks, and cross-download BT coordination.
    /// On download completion or failure, the entry is automatically removed.
    ///
    /// In C++ aria2, this registration is done by `BtSetup::setup()` which
    /// calls `BtRegistry::put(gid, btObject)`. Here the command self-registers
    /// for a cleaner architecture.
    ///
    /// # Arguments
    ///
    /// * `registry` - The engine's `Arc<std::sync::RwLock<BtRegistry>>`
    pub fn set_bt_registry(
        &mut self,
        registry: Arc<std::sync::RwLock<super::bt_registry::BtRegistry>>,
    ) {
        info!("BtRegistry reference set for BT download self-registration");
        self.bt_registry = Some(registry);
    }

    // ==================== PEX (BEP 11) Integration API ====================

    /// Add a peer address to the known peers list for PEX exchange
    pub fn add_pex_peer(
        &mut self,
        peer_addr: aria2_protocol::bittorrent::peer::connection::PeerAddr,
    ) {
        if !self.pex_known_peers.contains(&peer_addr) {
            debug!(addr = %format!("{}:{}", peer_addr.ip, peer_addr.port), "Adding peer to PEX known list");
            self.pex_known_peers.push(peer_addr);
        }
    }

    /// Set the list of known peers for PEX exchange
    pub fn set_pex_known_peers(
        &mut self,
        peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    ) {
        self.pex_known_peers = peers;
        info!(
            count = self.pex_known_peers.len(),
            "PEX known peers updated"
        );
    }

    /// Get reference to PEX known peers list
    pub fn get_pex_known_peers(&self) -> &[aria2_protocol::bittorrent::peer::connection::PeerAddr] {
        &self.pex_known_peers
    }

    /// Set custom PEX send interval (default 60 seconds)
    pub fn set_pex_send_interval(&mut self, interval_secs: u64) {
        self.pex_send_interval = Duration::from_secs(interval_secs);
        info!(interval_secs, "PEX send interval updated");
    }

    /// Check if it's time to send a PEX message based on rate limiting
    pub fn should_send_pex(&self) -> bool {
        match self.pex_last_send_time {
            Some(last) => last.elapsed() >= self.pex_send_interval,
            None => true,
        }
    }

    /// Update the last PEX send timestamp
    pub fn update_pex_last_send(&mut self) {
        self.pex_last_send_time = Some(Instant::now());
    }

    // ==================== Endgame Mode (Phase 14 - B1/B2) API ====================

    /// Get a mutable reference to the EndgameState for tracking duplicate requests
    pub fn endgame_state_mut(&mut self) -> &mut super::bt_download_execute::EndgameState {
        &mut self.endgame_state
    }

    /// Get an immutable reference to the EndgameState
    pub fn endgame_state(&self) -> &super::bt_download_execute::EndgameState {
        &self.endgame_state
    }

    // ==================== H3: Bad Peer Detection / Ban System API ====================

    /// Record that a specific peer sent invalid piece data (hash verification failed).
    ///
    /// This method:
    /// 1. Increments the peer's `bad_data_count` in the choking algorithm's PeerStats
    /// 2. If the count reaches [`crate::engine::peer_stats::BAD_DATA_THRESHOLD`],
    ///    automatically bans the peer with a reason message
    /// 3. Logs the event at WARN level
    ///
    /// # Arguments
    ///
    /// * `peer_idx` - The index of the peer in the choking algorithm's peer list
    /// * `piece_index` - The index of the piece that failed verification
    ///
    /// # Returns
    ///
    /// * `Ok(true)` if the peer was banned as a result of this call
    /// * `Ok(false)` if the peer was not banned (count below threshold)
    /// * `Err(())` if the peer index is invalid or choking algorithm is not configured
    #[allow(clippy::result_unit_err)]
    pub fn record_bad_piece_for_peer(
        &mut self,
        peer_idx: usize,
        piece_index: u32,
    ) -> std::result::Result<bool, ()> {
        use crate::engine::peer_stats::BAD_DATA_THRESHOLD;

        if let Some(ref mut algo) = self.choking_algo
            && let Some(peer) = algo.get_peer_mut(peer_idx)
        {
            let should_ban = peer.increment_bad_data();

            warn!(
                "[BT] Peer {} sent invalid data for piece {} (bad count: {}/{})",
                peer_idx, piece_index, peer.bad_data_count, BAD_DATA_THRESHOLD
            );

            if should_ban {
                let reason = format!(
                    "Too many invalid pieces ({} >= {})",
                    peer.bad_data_count, BAD_DATA_THRESHOLD
                );
                warn!("[BT] BANNING peer {}: {}", peer_idx, reason);
                peer.ban_peer(reason);
                return Ok(true); // Peer was banned
            }

            return Ok(false); // Count incremented but not banned yet
        }

        Err(()) // Invalid peer index or no choking algorithm
    }

    /// Record that a valid, verified piece was received from a peer.
    ///
    /// This triggers gradual recovery by decrementing the peer's `bad_data_count`.
    /// Call this after successful hash verification to allow peers to recover reputation.
    ///
    /// # Arguments
    ///
    /// * `peer_idx` - The index of the peer in the choking algorithm's peer list
    pub fn record_valid_piece_for_peer(&mut self, peer_idx: usize) {
        if let Some(ref mut algo) = self.choking_algo
            && let Some(peer) = algo.get_peer_mut(peer_idx)
        {
            peer.decrement_bad_data();
            debug!(
                "[BT] Peer {} sent valid piece, bad count decremented to {}",
                peer_idx, peer.bad_data_count
            );
        }
    }

    /// Check if a peer is currently banned.
    ///
    /// # Arguments
    ///
    /// * `peer_idx` - The index of the peer in the choking algorithm's peer list
    ///
    /// # Returns
    ///
    /// * `true` if the peer is banned or peer not found
    /// * `false` if the peer exists and is not banned
    pub fn is_peer_banned(&self, peer_idx: usize) -> bool {
        self.choking_algo
            .as_ref()
            .and_then(|algo| algo.get_peer(peer_idx))
            .map(|p| p.is_banned)
            .unwrap_or(true) // If not found, treat as banned for safety
    }

    /// Get a reference to a peer's stats for RPC/display purposes.
    ///
    /// Returns `None` if the peer doesn't exist or no choking algorithm is configured.
    pub fn get_peer_stats(&self, peer_idx: usize) -> Option<&crate::engine::peer_stats::PeerStats> {
        self.choking_algo.as_ref()?.get_peer(peer_idx)
    }

    // ==================== Web Seed (BEP 19) Integration API ====================

    /// Initialize the web seed manager if web seeds are configured.
    ///
    /// This should be called after the torrent metadata is parsed and before
    /// the download loop starts.
    ///
    /// # Arguments
    ///
    /// * `piece_length` - Length of each piece in the torrent
    /// * `total_length` - Total file length
    pub fn init_web_seed_manager(&mut self, piece_length: u32, total_length: u64) {
        if !self.web_seed_urls.is_empty() && self.web_seed_manager.is_none() {
            info!(
                count = self.web_seed_urls.len(),
                "Initializing web seed manager with {} URL(s)",
                self.web_seed_urls.len()
            );
            self.web_seed_manager = Some(crate::engine::bt_web_seed::WebSeedManager::new(
                self.web_seed_urls.clone(),
                piece_length,
                total_length,
            ));
        }
    }

    /// Get a reference to the web seed manager.
    pub fn get_web_seed_manager(&self) -> Option<&crate::engine::bt_web_seed::WebSeedManager> {
        self.web_seed_manager.as_ref()
    }

    /// Get a mutable reference to the web seed manager.
    pub fn get_web_seed_manager_mut(
        &mut self,
    ) -> Option<&mut crate::engine::bt_web_seed::WebSeedManager> {
        self.web_seed_manager.as_mut()
    }

    /// Check if web seeds are available.
    pub fn has_web_seeds(&self) -> bool {
        !self.web_seed_urls.is_empty()
    }

    /// Get web seed download statistics.
    pub fn web_seed_stats(&self) -> Option<&crate::engine::bt_web_seed::WebSeedStats> {
        self.web_seed_manager.as_ref().map(|m| m.stats())
    }

    // ==================== Seeding Mode (Phase 16) API ====================

    /// Check if download is complete and start seeding if enabled.
    ///
    /// This method should be called after the download loop completes.
    /// It initializes the seed manager if:
    /// - All pieces are complete
    /// - Seeding is enabled (seed_ratio > 0 or seed_time > 0)
    ///
    /// # Arguments
    ///
    /// * `piece_picker` - Reference to the piece picker to check completion
    /// * `meta` - Torrent metadata
    /// * `connections` - Active peer connections to use for seeding
    /// * `piece_provider` - Provider for piece data (from downloaded files)
    ///
    /// # Returns
    ///
    /// * `Ok(true)` if seeding was started
    /// * `Ok(false)` if seeding was not started (not complete or disabled)
    pub fn check_and_start_seeding(
        &mut self,
        piece_picker: &aria2_protocol::bittorrent::piece::picker::PiecePicker,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_provider: std::sync::Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>,
    ) -> Result<bool> {
        // Check if download is complete
        if !piece_picker.is_complete() {
            debug!("Download not complete, skipping seeding");
            return Ok(false);
        }

        // Check if seeding is enabled
        if !self.seed_enabled {
            info!("Seeding disabled, not starting seed manager");
            return Ok(false);
        }

        // Initialize seed manager
        use crate::engine::bt_seed_manager::{BtSeedManager, SeedExitCondition};
        use crate::engine::bt_upload_session::BtSeedingConfig;

        let seed_ratio = self.seed_ratio.unwrap_or(0.0);
        let seed_time = self.seed_time.map(|d| d.as_secs());

        let exit_condition =
            SeedExitCondition::with_time_and_ratio(seed_time.unwrap_or(0), seed_ratio);

        let config = BtSeedingConfig {
            max_upload_bytes_per_sec: None, // Will be set from options if needed
            max_peers_to_unchoke: 4,
            optimistic_unchoke_interval_secs: 30,
        };

        let total_downloaded = meta.total_size();

        let seed_manager = BtSeedManager::new_with_info_hash(
            meta.info_hash.bytes,
            connections,
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
        );

        self.seed_manager = Some(seed_manager);

        info!(
            "Seeding started: ratio={}, time={:?}, info_hash={}",
            seed_ratio,
            self.seed_time,
            meta.info_hash.as_hex()
        );

        Ok(true)
    }

    /// Get a reference to the seed manager.
    pub fn get_seed_manager(&self) -> Option<&super::bt_seed_manager::BtSeedManager> {
        self.seed_manager.as_ref()
    }

    /// Get a mutable reference to the seed manager.
    pub fn get_seed_manager_mut(&mut self) -> Option<&mut super::bt_seed_manager::BtSeedManager> {
        self.seed_manager.as_mut()
    }

    /// Check if seeding is active.
    pub fn is_seeding(&self) -> bool {
        self.seed_manager.is_some()
    }

    /// Get seeding statistics.
    ///
    /// Returns `None` if not seeding.
    pub fn get_seed_stats(&self) -> Option<SeedStats> {
        self.seed_manager.as_ref().map(|mgr| {
            let (total_uploaded, upload_speed) = mgr.get_upload_stats();
            let total_downloaded = mgr.total_downloaded();
            let ratio = if total_downloaded > 0 {
                total_uploaded as f64 / total_downloaded as f64
            } else {
                0.0
            };

            SeedStats {
                total_uploaded,
                upload_speed,
                ratio,
                elapsed: mgr.seeding_duration(),
            }
        })
    }
}

/// Seeding statistics for a completed download.
#[derive(Debug, Clone)]
pub struct SeedStats {
    /// Total bytes uploaded during seeding
    pub total_uploaded: u64,
    /// Current upload speed in bytes/sec
    pub upload_speed: u64,
    /// Upload/download ratio
    pub ratio: f64,
    /// Time elapsed since seeding started
    pub elapsed: std::time::Duration,
}

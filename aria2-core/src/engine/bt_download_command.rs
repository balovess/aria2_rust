use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

use crate::engine::bt_choke_manager::{
    add_peer_to_tracking, check_snubbed_peers, handle_snubbed_peer, on_data_received_from_peer,
    on_peer_choke, on_peer_unchoke, on_piece_received, select_best_peer_for_request,
};
use crate::engine::bt_piece_downloader::write_piece_to_multi_files;
use crate::engine::bt_post_download_handler::HookManager;
use crate::engine::bt_progress_info_file::BtProgressManager;
use crate::engine::bt_tracker_comm::announce_to_public_tracker;
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::lpd_manager::LpdManager;
use crate::engine::multi_file_layout::MultiFileLayout;
use crate::error::{Aria2Error, FatalError, Result};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

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
    pub(crate) group: Arc<tokio::sync::RwLock<RequestGroup>>,
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
    #[allow(dead_code)] // Reserved for peer bitfield tracking in future optimizations
    peer_tracker: Option<aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker>,
    pub choking_algo: Option<ChokingAlgorithm>,
    pub multi_file_layout: Option<MultiFileLayout>,

    // P1/P2 集成字段（全部使用 Option 保持向后兼容）
    /// BT进度持久化管理器
    pub(crate) progress_manager: Option<BtProgressManager>,
    /// 进度保存间隔（默认60秒）
    pub(crate) progress_save_interval: Duration,
    /// LPD局域网peer发现管理器
    pub(crate) lpd_manager: Option<Arc<LpdManager>>,
    /// 下载后处理钩子管理器
    pub(crate) hook_manager: Option<Arc<HookManager>>,

    // PEX (Peer Exchange, BEP 11) integration fields
    /// Track known peers for PEX exchange
    pub(crate) pex_known_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    /// Timestamp of last PEX message sent (for rate limiting)
    pub(crate) pex_last_send_time: Option<Instant>,
    /// Interval between PEX messages (default 60 seconds)
    pub(crate) pex_send_interval: Duration,
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

        let seed_time = options.seed_time.and_then(|t| {
            if t == 0 {
                None
            } else {
                Some(std::time::Duration::from_secs(t))
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
                max_upload_slots: options.bt_max_upload_slots.unwrap_or(4) as usize,
                optimistic_unchoke_interval_secs: options
                    .bt_optimistic_unchoke_interval
                    .unwrap_or(30),
                snubbed_timeout_secs: options.bt_snubbed_timeout.unwrap_or(60),
                choke_rotation_interval_secs: 10,
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

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            output_path: effective_output_path,
            started: false,
            completed_bytes: 0,
            torrent_data: torrent_bytes.to_vec(),
            seed_enabled: options.seed_time.unwrap_or(0) > 0
                || options.seed_ratio.unwrap_or(0.0) > 0.0,
            seed_time,
            seed_ratio,
            total_uploaded: 0,
            udp_client: None,
            dht_engine: None,
            public_trackers: None,
            peer_tracker: None,
            choking_algo,
            multi_file_layout,

            // P1/P2 集成字段默认值（全部为 None，保持向后兼容）
            progress_manager: None,
            progress_save_interval: Duration::from_secs(60),
            lpd_manager: None,
            hook_manager: None,

            // PEX integration fields default values
            pex_known_peers: Vec::new(),
            pex_last_send_time: None,
            pex_send_interval: Duration::from_secs(60),
        })
    }

    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
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
        on_piece_received(&mut self.choking_algo, peer_idx, bytes);
    }

    pub async fn announce_to_public_tracker(
        tracker_url: &str,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        total_size: u64,
    ) -> std::result::Result<Vec<(String, u16)>, String> {
        announce_to_public_tracker(tracker_url, info_hash, peer_id, total_size).await
    }

    pub async fn write_piece_to_multi_files(
        layout: &MultiFileLayout,
        piece_idx: u32,
        piece_data: &[u8],
        piece_length: u32,
    ) -> Result<()> {
        write_piece_to_multi_files(layout, piece_idx, piece_data, piece_length).await
    }

    pub fn is_multi_file(&self) -> bool {
        self.multi_file_layout
            .as_ref()
            .is_some_and(|l| l.is_multi_file())
    }

    pub fn get_multi_file_layout(&self) -> Option<&MultiFileLayout> {
        self.multi_file_layout.as_ref()
    }

    // ==================== P1/P2 集成 API ====================

    /// 设置 BT 进度管理器
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

    /// 获取进度管理器引用（用于测试和外部访问）
    pub fn get_progress_manager(&self) -> Option<&BtProgressManager> {
        self.progress_manager.as_ref()
    }

    /// 获取 LPD 管理器引用（用于测试和外部访问）
    pub fn get_lpd_manager(&self) -> Option<&Arc<LpdManager>> {
        self.lpd_manager.as_ref()
    }

    /// 获取钩子管理器引用（用于测试和外部访问）
    pub fn get_hook_manager(&self) -> Option<&Arc<HookManager>> {
        self.hook_manager.as_ref()
    }

    // ==================== PEX (BEP 11) Integration API ====================

    /// Add a peer address to the known peers list for PEX exchange
    pub fn add_pex_peer(
        &mut self,
        peer_addr: aria2_protocol::bittorrent::peer::connection::PeerAddr,
    ) {
        if !self.pex_known_peers.iter().any(|p| *p == peer_addr) {
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
}

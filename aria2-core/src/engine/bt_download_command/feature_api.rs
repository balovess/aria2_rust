use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info};

use crate::engine::bt_progress_info_file::BtProgressManager;
use crate::engine::hook_manager::HookManager;
use crate::engine::lpd_manager::LpdManager;
use crate::error::Result;

use super::BtDownloadCommand;

// ==================== Seeding Statistics ====================

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

impl BtDownloadCommand {
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
    /// * `manager` - A configured [`HookManager`](super::hook_manager::HookManager) with registered hooks, wrapped in `Arc`
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::collections::HashMap;
    /// use std::sync::Arc;
    /// use aria2_core::engine::hook_manager::{HookManager, HookConfig, MoveHook, ExecHook};
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
    ///
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
        registry: Arc<std::sync::RwLock<super::super::bt_registry::BtRegistry>>,
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
        info!(count = self.pex_known_peers.len(), "PEX known peers updated");
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
    pub fn endgame_state_mut(&mut self) -> &mut super::super::bt_download_execute::EndgameState {
        &mut self.endgame_state
    }

    /// Get an immutable reference to the EndgameState
    pub fn endgame_state(&self) -> &super::super::bt_download_execute::EndgameState {
        &self.endgame_state
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
            max_upload_bytes_per_sec: None,
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
            seed_ratio, self.seed_time, meta.info_hash.as_hex()
        );

        Ok(true)
    }

    /// Get a reference to the seed manager.
    pub fn get_seed_manager(&self) -> Option<&super::super::bt_seed_manager::BtSeedManager> {
        self.seed_manager.as_ref()
    }

    /// Get a mutable reference to the seed manager.
    pub fn get_seed_manager_mut(&mut self) -> Option<&mut super::super::bt_seed_manager::BtSeedManager> {
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
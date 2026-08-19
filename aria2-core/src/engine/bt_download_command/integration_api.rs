use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

use crate::engine::bt_progress_info_file::BtProgressManager;
use crate::engine::hook_manager::HookManager;
use crate::engine::lpd_manager::LpdManager;

use super::BtDownloadCommand;

// ==================== P1/P2 Integration API ====================

impl BtDownloadCommand {
    /// Set the BT progress manager
    ///
    /// Enable BT download progress persistence for resume support.
    ///
    /// When enabled, the engine periodically saves the piece completion
    /// bitfield and transfer statistics to a C++-compatible binary `.aria2`
    /// file. On restart, a compatible file is used as a fallback to skip
    /// already-completed pieces when no newer Rust-owned checkpoint exists.
    pub fn set_progress_manager(&mut self, manager: BtProgressManager) {
        info!("BT progress manager enabled");
        self.progress_manager = Some(manager);
    }

    /// Set the interval (in seconds) between progress save operations.
    pub fn set_progress_save_interval(&mut self, interval_secs: u64) {
        self.progress_save_interval = Duration::from_secs(interval_secs);
        info!(interval_secs, "Progress save interval updated");
    }

    /// Enable Local Peer Discovery (LPD, BEP 14) for LAN peer finding.
    pub fn set_lpd_manager(&mut self, manager: Arc<LpdManager>) {
        info!("LPD manager enabled for local peer discovery");
        self.lpd_manager = Some(manager);
    }

    /// Register a post-download hook chain for completion/error callbacks.
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
    /// Check the download-scoped temporary bad-peer state.
    pub(crate) fn is_peer_temporarily_rejected(&self, ipaddr: &str) -> bool {
        self.peer_rejection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_rejected(ipaddr)
    }

    /// Record a verified bad peer in the shared download-scoped state.
    pub(crate) fn reject_peer_temporarily(&self, ipaddr: &str) {
        self.peer_rejection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reject(ipaddr);
    }

    pub fn set_bt_registry(
        &mut self,
        registry: Arc<std::sync::RwLock<super::super::bt_registry::BtRegistry>>,
    ) {
        info!("BtRegistry reference set for BT download self-registration");
        self.bt_registry = Some(registry);
    }

    /// Attach the engine-owned process listener used for info-hash routing.
    pub fn set_bt_listener(
        &mut self,
        listener: Arc<crate::engine::bt_peer_listener::BtPeerListenerManager>,
    ) {
        self.bt_listener = Some(listener);
    }

    /// Set the process-wide public tracker catalog shared by all BT commands.
    pub fn set_public_tracker_catalog(
        &mut self,
        catalog: Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>,
    ) {
        self.public_trackers = Some(catalog);
    }
}

// ==================== PEX (BEP 11) Integration API ====================

impl BtDownloadCommand {
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
}

// ==================== Endgame Mode (Phase 14 - B1/B2) API ====================

impl BtDownloadCommand {
    /// Get a mutable reference to the EndgameState for tracking duplicate requests
    pub fn endgame_state_mut(&mut self) -> &mut super::super::bt_download_execute::EndgameState {
        &mut self.endgame_state
    }

    /// Get an immutable reference to the EndgameState
    pub fn endgame_state(&self) -> &super::super::bt_download_execute::EndgameState {
        &self.endgame_state
    }
}

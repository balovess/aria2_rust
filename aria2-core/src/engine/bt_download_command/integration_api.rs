use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

use crate::engine::bt_progress_info_file::BtProgressManager;
use crate::engine::hook_manager::HookManager;
use crate::engine::lpd_manager::LpdManager;
use crate::util::rwlock_ext::RwLockRecover;

use super::BtDownloadCommand;

// ==================== P1/P2 Integration API ====================

impl BtDownloadCommand {
    /// Enable BT progress persistence for resume support.
    pub fn set_progress_manager(&mut self, manager: BtProgressManager) {
        info!("BT progress manager enabled");
        self.progress_manager = Some(manager);
    }

    /// Set the interval between progress save operations.
    pub fn set_progress_save_interval(&mut self, interval_secs: u64) {
        self.progress_save_interval = Duration::from_secs(interval_secs);
        info!(interval_secs, "Progress save interval updated");
    }

    /// Enable Local Peer Discovery (LPD, BEP 14) for LAN peer finding.
    pub fn set_lpd_manager(&mut self, manager: Arc<LpdManager>) {
        info!("LPD manager enabled for local peer discovery");
        self.lpd_manager = Some(manager);
    }

    /// Register post-download hooks for completion and error callbacks.
    pub fn set_hook_manager(&mut self, manager: Arc<HookManager>) {
        info!(
            hook_count = manager.hook_count(),
            "Hook manager enabled with {} hooks",
            manager.hook_count()
        );
        self.hook_manager = Some(manager);
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

    /// Publish the command's live DHT engine so RPC status can aggregate it.
    pub(crate) fn register_dht_engine(&self) {
        let (Some(registry), Some(engine)) = (&self.bt_registry, &self.dht_engine) else {
            return;
        };
        if let Ok(mut registry) = registry.write() {
            registry.set_dht_engine_for_gid(self.group.recover().gid().value(), Arc::clone(engine));
        }
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

    /// Set custom PEX send interval (default 60 seconds)
    pub fn set_pex_send_interval(&mut self, interval_secs: u64) {
        self.pex_send_interval = Duration::from_secs(interval_secs);
        info!(interval_secs, "PEX send interval updated");
    }

    /// Check if it's time to send a PEX message based on rate limiting
    pub(crate) fn should_send_pex(&self) -> bool {
        match self.pex_last_send_time {
            Some(last) => last.elapsed() >= self.pex_send_interval,
            None => true,
        }
    }

    /// Update the last PEX send timestamp
    pub(crate) fn update_pex_last_send(&mut self) {
        self.pex_last_send_time = Some(Instant::now());
    }
}

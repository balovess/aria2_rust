use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use super::BtDownloadCommand;
use crate::engine::bt_progress_info_file::BtProgressManager;
use crate::engine::hook_manager::HookManager;
use crate::engine::lpd_manager::LpdManager;

// ==================== P1/P2 Integration API ====================

impl BtDownloadCommand {
    /// Set the process-wide policy used for outbound peer TCP connections.
    pub fn set_outbound_network_policy(
        &mut self,
        policy: Arc<crate::network::OutboundNetworkPolicy>,
    ) {
        if !policy.is_direct()
            && let Some(socket) = self.utp_socket.take()
        {
            let local_address = socket.try_lock().ok().map(|socket| socket.local_addr());
            if let Some(old_address) = local_address {
                // The old socket must be dropped before rebinding its fixed
                // listen port. This setter runs before command execution, so
                // there are no active uTP sessions to preserve here.
                drop(socket);
                let source = policy
                    .addresses()
                    .into_iter()
                    .find(|address| address.is_ipv4() == old_address.is_ipv4())
                    .or_else(|| policy.addresses().into_iter().next());
                let rebind = source.map(|source| {
                    aria2_protocol::bittorrent::utp::UtpSocket::bind_addr(
                        std::net::SocketAddr::new(source, old_address.port()),
                    )
                });
                match rebind {
                    Some(Ok(replacement)) => {
                        self.utp_socket = Some(Arc::new(tokio::sync::Mutex::new(replacement)));
                    }
                    Some(Err(error)) => {
                        warn!(%error, "Failed to rebind BT uTP socket to outbound policy source; retaining the original socket");
                        self.utp_socket =
                            aria2_protocol::bittorrent::utp::UtpSocket::bind_addr(old_address)
                                .ok()
                                .map(|replacement| Arc::new(tokio::sync::Mutex::new(replacement)));
                    }
                    None => {
                        warn!(
                            "Outbound policy has no source address for the existing BT uTP socket"
                        );
                        self.utp_socket = None;
                    }
                }
            } else {
                warn!(
                    "BT uTP socket was busy while applying outbound policy; retaining the original socket"
                );
                self.utp_socket = Some(socket);
            }
        }
        self.outbound_network_policy = policy;
    }

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

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::util::rwlock_ext::RwLockRecover;

pub(super) fn effective_peer_speed_threshold(
    configured: u64,
    max_download_limit: Option<u64>,
) -> u64 {
    match max_download_limit.filter(|limit| *limit > 0) {
        Some(limit) => configured.min(limit),
        None => configured,
    }
}

pub(super) fn download_speed_is_below_peer_request_limit(
    current_speed: u64,
    configured: u64,
    max_download_limit: Option<u64>,
) -> bool {
    let threshold = effective_peer_speed_threshold(configured, max_download_limit);
    threshold > 0 && current_speed < threshold
}

impl BtDownloadCommand {
    pub(in crate::engine::bt_download_execute::execute) fn peer_exchange_enabled(&self) -> bool {
        !self.is_private && self.group.recover().options().enable_peer_exchange
    }

    pub(in crate::engine::bt_download_execute::execute) fn apply_peer_exchange_policy(
        &self,
        conn: &mut BtPeerConn,
    ) {
        conn.set_pex_enabled(self.peer_exchange_enabled());
    }

    /// Return the number of peers currently owned by this torrent's storage.
    ///
    /// This is the Rust equivalent of C++ `PeerStorage::countAllPeer()` and
    /// intentionally includes both queued and connected peers.
    pub(in crate::engine::bt_download_execute::execute) fn tracked_peer_count(&self) -> usize {
        self.peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .count_all_peers()
    }

    /// Update tracker demand from the live connection count.
    ///
    /// The C++ `BtRuntime::lessThanMinPeers()` is derived from active peer
    /// commands and its configured max-peer limit, not from the last tracker
    /// response. Keep the Rust announce state synchronized at the same boundary.
    pub(in crate::engine::bt_download_execute::execute) fn update_tracker_peer_state(
        &mut self,
        active_connections: usize,
    ) {
        let max_peers = self.group.recover().options().bt_max_peers;
        self.bt_runtime.set_max_peers(max_peers);
        self.bt_runtime.set_connections(active_connections);
    }

    pub(in crate::engine::bt_download_execute::execute) fn should_admit_incoming_peer(
        &self,
        active_connections: usize,
    ) -> bool {
        let group = self.group.recover();
        if group.options().bt_max_peers == 0 || active_connections < group.options().bt_max_peers {
            return true;
        }

        download_speed_is_below_peer_request_limit(
            group.download_speed(),
            group.options().bt_request_peer_speed_limit,
            group.options().max_download_limit,
        )
    }
}

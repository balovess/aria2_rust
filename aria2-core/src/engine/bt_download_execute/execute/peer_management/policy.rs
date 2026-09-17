use std::collections::HashMap;
use std::time::Instant;

use tracing::debug;

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_download_execute::types::PeerKey;
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

    /// Check all tracked peers for snubbing (no data received within timeout).
    /// Called periodically from the download loop.
    pub(in crate::engine::bt_download_execute::execute) fn check_and_mark_snubbed_peers(
        &mut self,
        last_snub_check: &mut Instant,
        peer_last_data_time: &HashMap<PeerKey, Instant>,
        active_connections: &[BtPeerConn],
    ) {
        const SNUB_CHECK_INTERVAL_SECS: u64 = 10;
        const SNUB_TIMEOUT_SECS: u64 = 30;

        if last_snub_check.elapsed().as_secs() < SNUB_CHECK_INTERVAL_SECS {
            return;
        }
        *last_snub_check = Instant::now();

        let mut newly_snubbed = Vec::new();
        for (&peer_id, &last_time) in peer_last_data_time {
            if last_time.elapsed().as_secs() > SNUB_TIMEOUT_SECS {
                if let Some(index) = active_connections
                    .iter()
                    .position(|conn| PeerKey::from_peer(&conn.ip_addr, conn.port) == Some(peer_id))
                {
                    self.mark_peer_snubbed(index);
                }
                newly_snubbed.push(peer_id);
                debug!(
                    "[BT] Peer {} marked as snubbed (no data for {}s)",
                    peer_id.address(),
                    last_time.elapsed().as_secs()
                );
            }
        }
        if !newly_snubbed.is_empty() {
            debug!(
                "[BT] Snub check: {} peers newly snubbed",
                newly_snubbed.len()
            );
        }

        // Also run the PeerStats-level snub check (timeout-based)
        let stats_snubbed = self.check_snubbed_peers();
        if !stats_snubbed.is_empty() {
            debug!(
                "[BT] PeerStats snub check: {} peers timed out",
                stats_snubbed.len()
            );
        }
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
        if let Some(announcer) = self.tracker_announcer.as_mut() {
            announcer.set_less_than_min_peers(self.bt_runtime.less_than_min_peers());
        }
    }

    pub(in crate::engine::bt_download_execute::execute) fn should_discover_more_peers(
        &self,
        active_connections: usize,
    ) -> bool {
        if self.peer_coordinator.should_replenish(active_connections) {
            return true;
        }

        let group = self.group.recover();
        download_speed_is_below_peer_request_limit(
            group.download_speed(),
            group.options().bt_request_peer_speed_limit,
            group.options().max_download_limit,
        )
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

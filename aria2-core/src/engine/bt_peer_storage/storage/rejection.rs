use std::time::{Duration, Instant};

use rand::Rng;
use tracing::debug;

use super::DefaultPeerStorage;
use crate::engine::bt_peer_storage::constants::{
    TEMP_PEER_CLEANUP_INTERVAL_SECS, TEMP_REJECT_TIMEOUT_MIN_SECS, TEMP_REJECT_TIMEOUT_RANGE_SECS,
};

impl DefaultPeerStorage {
    // ==================================================================
    // Temporary rejection
    // ==================================================================

    /// Check whether a peer IP is temporarily rejected.
    ///
    /// If the timeout has expired, the entry is removed and false is returned.
    /// Matches C++ DefaultPeerStorage::isTemporarilyRejectedPeer.
    pub fn is_temporarily_rejected(&mut self, ipaddr: &str) -> bool {
        let Some(timeout) = self.temporarily_rejected_peers.get(ipaddr) else {
            return false;
        };

        if *timeout <= Instant::now() {
            // Timeout has expired -- remove entry.
            self.temporarily_rejected_peers.remove(ipaddr);
            return false;
        }

        true
    }

    /// Temporarily reject a peer IP with a variable timeout.
    ///
    /// The timeout is randomly chosen in [120, 720] seconds to avoid
    /// thundering herd effects when many bad peers wake up simultaneously.
    /// Expired entries are cleaned up once per hour.
    ///
    /// Matches C++ DefaultPeerStorage::rejectPeerTemporarily.
    pub fn reject_peer_temporarily(&mut self, ipaddr: &str) {
        let now = Instant::now();

        // Periodic cleanup of expired entries (C++ checks every 1 hour).
        if now.duration_since(self.last_temp_peer_cleanup)
            >= Duration::from_secs(TEMP_PEER_CLEANUP_INTERVAL_SECS)
        {
            self.temporarily_rejected_peers.retain(|ip, timeout| {
                if *timeout <= now {
                    debug!("Purge temporarily rejected peer {}", ip);
                    false
                } else {
                    true
                }
            });
            self.last_temp_peer_cleanup = now;
        }

        // Variable timeout: [120, 720] seconds (C++: 120 + getRandomNumber(601)).
        let mut rng = rand::thread_rng();
        let extra_secs: u64 = rng.gen_range(0..TEMP_REJECT_TIMEOUT_RANGE_SECS);
        let timeout_secs = TEMP_REJECT_TIMEOUT_MIN_SECS + extra_secs;

        debug!("Temporarily rejected peer {} for {}s", ipaddr, timeout_secs);

        self.temporarily_rejected_peers.insert(
            ipaddr.to_owned().into_boxed_str(),
            now + Duration::from_secs(timeout_secs),
        );
    }
}

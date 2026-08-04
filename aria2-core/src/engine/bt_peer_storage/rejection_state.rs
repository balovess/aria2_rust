use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand::Rng;

use super::constants::{
    TEMP_PEER_CLEANUP_INTERVAL_SECS, TEMP_REJECT_TIMEOUT_MIN_SECS, TEMP_REJECT_TIMEOUT_RANGE_SECS,
};

/// Shared temporary rejection state for one BitTorrent download.
///
/// The state is deliberately separate from the lifecycle trait: that trait
/// exposes borrowed collections and cannot be implemented safely through a
/// mutex-backed trait object. Rejection is the cross-component state that
/// tracker, DHT, PEX, and piece verification need to share.
#[derive(Debug)]
pub struct PeerRejectionState {
    rejected: HashMap<String, Instant>,
    last_cleanup: Instant,
}

pub type SharedPeerRejection = Arc<Mutex<PeerRejectionState>>;

impl PeerRejectionState {
    pub fn new() -> Self {
        Self {
            rejected: HashMap::new(),
            last_cleanup: Instant::now(),
        }
    }

    pub fn shared() -> SharedPeerRejection {
        Arc::new(Mutex::new(Self::new()))
    }

    pub fn is_rejected(&mut self, ipaddr: &str) -> bool {
        let Some(timeout) = self.rejected.get(ipaddr).copied() else {
            return false;
        };
        if timeout <= Instant::now() {
            self.rejected.remove(ipaddr);
            false
        } else {
            true
        }
    }

    pub fn reject(&mut self, ipaddr: &str) {
        let now = Instant::now();
        if now.duration_since(self.last_cleanup)
            >= Duration::from_secs(TEMP_PEER_CLEANUP_INTERVAL_SECS)
        {
            self.rejected.retain(|_, timeout| *timeout > now);
            self.last_cleanup = now;
        }
        let extra = rand::thread_rng().gen_range(0..TEMP_REJECT_TIMEOUT_RANGE_SECS);
        self.rejected.insert(
            ipaddr.to_string(),
            now + Duration::from_secs(TEMP_REJECT_TIMEOUT_MIN_SECS + extra),
        );
    }
}

impl Default for PeerRejectionState {
    fn default() -> Self {
        Self::new()
    }
}

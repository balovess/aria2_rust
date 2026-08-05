//! Local Peer Discovery (LPD) Manager - Phase 15 H8
//!
//! Implements BitTorrent Local Peer Discovery using UDP multicast on
//! the standard LPD multicast group 239.192.152.143:6771 per BEP 14.
//!
//! # Architecture
//!
//! ```text
//! lpd_manager.rs (this anchor file)
//!   ├── LpdPeer       - Discovered peer information
//!   ├── LpdManager    - High-level coordinator for LPD operations
//!   ├── constants     - MAX_PEERS_PER_HASH
//!   └── re-exports    - Public API from submodules
//!
//! lpd_manager/ (submodules)
//!   ├── announce.rs   - LpdAnnouncer (UDP multicast sender/receiver)
//!   ├── discovery.rs  - BEP14 message parsing + private address detection
//!   └── tests.rs      - Comprehensive test suite
//!
//! LPD Protocol (BEP 14):
//!   Multicast Group: 239.192.152.143:6771
//!   Message Format (HTTP-like):
//!     BT-SEARCH * HTTP/1.1\r\n
//!     Host: 239.192.152.143:6771\r\n
//!     Port: <listen_port>\r\n
//!     Infohash: <40-char-hex-info-hash>\r\n
//!     \r\n\r\n
//!
//!   Announce Interval: Every 5 minutes while active
//! ```

mod announce;
mod discovery;
#[cfg(test)]
mod tests;

// Re-export public API for backward compatibility
pub use announce::LpdAnnouncer;
pub use discovery::{is_private_address, parse_lpd_announcement};

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::constants;
use crate::engine::lpd_receive_loop::LpdReceiveLoop;

// Re-export LPD constants for backward compatibility with test imports
pub use constants::{
    LPD_DEFAULT_ANNOUNCE_INTERVAL_SECS as DEFAULT_ANNOUNCE_INTERVAL_SECS,
    LPD_MULTICAST_ADDRESS as LPD_MULTICAST_ADDR, LPD_PORT,
};

// =========================================================================
// Constants
// =========================================================================

/// Maximum number of peers to track per info hash
pub const MAX_PEERS_PER_HASH: usize = 50;

// =========================================================================
// LpdPeer - Discovered peer from LPD announcement
// =========================================================================

/// Information about a peer discovered via LPD
#[derive(Debug, Clone)]
pub struct LpdPeer {
    /// The torrent info hash this peer is sharing
    pub info_hash: String,
    /// The peer's listen port
    pub port: u16,
    /// The peer's IP address (from recv_from)
    pub addr: IpAddr,
    /// When this peer was last announced
    pub last_seen: Instant,
    /// Whether this peer is on the local network (private IP range).
    /// C++: `peer->setLocalPeer(util::inPrivateAddress(remoteEndpoint.addr))`
    pub is_local: bool,
}

impl LpdPeer {
    /// Create a new LpdPeer
    pub fn new(info_hash: impl Into<String>, port: u16, addr: IpAddr) -> Self {
        Self {
            info_hash: info_hash.into(),
            port,
            addr,
            last_seen: Instant::now(),
            is_local: is_private_address(&addr),
        }
    }

    /// Get the peer's address as SocketAddr
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.addr, self.port)
    }

    /// Check if this peer has expired based on age
    pub fn is_expired(&self, max_age: Duration) -> bool {
        self.last_seen.elapsed() > max_age
    }
}

impl PartialEq for LpdPeer {
    fn eq(&self, other: &Self) -> bool {
        // Two peers are considered equal if they share same info_hash and IP
        self.info_hash == other.info_hash && self.addr == other.addr
    }
}

impl Eq for LpdPeer {}

impl std::hash::Hash for LpdPeer {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.info_hash.hash(state);
        self.addr.hash(state);
    }
}

// =========================================================================
// LpdManager - High-level coordinator
// =========================================================================

/// Manages LPD operations for all active torrents.
///
/// Maintains a registry of known peers discovered via LPD, handles periodic
/// announcements for active downloads, and coordinates with the download engine.
///
/// The receive loop (`LpdReceiveLoop`) runs as a background tokio task
/// that continuously reads LPD multicast announcements and updates the
/// peer registry. This mirrors the C++ `LpdReceiveCommand` which
/// re-adds itself to the event loop after each receive.
pub struct LpdManager {
    /// The underlying UDP announcer
    announcer: Arc<LpdAnnouncer>,
    /// Registry of discovered peers keyed by info_hash
    peers: Arc<RwLock<HashMap<String, HashSet<LpdPeer>>>>,
    /// Track which info hashes we're currently announcing
    pub active_hashes: Arc<RwLock<HashSet<String>>>,
    /// Handle to the background announce task
    _announce_task: Option<tokio::task::JoinHandle<()>>,
    /// Background receive loop for continuous LPD announcement processing
    receive_loop: LpdReceiveLoop,
}

impl Default for LpdManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LpdManager {
    /// Create a new LpdManager
    ///
    /// Initializes the UDP socket, joins multicast group, and sets up
    /// internal state tracking. The receive loop is not started
    /// automatically; call `start_receive_loop()` to begin receiving.
    pub fn new() -> Self {
        let announcer = LpdAnnouncer::new().unwrap_or_else(|_| {
            // If we can't create real socket, create a dummy one for testing
            // In production this would be an error
            warn!("Could not create LPD announcer, LPD will be disabled");
            LpdAnnouncer::with_config(constants::LPD_DEFAULT_ANNOUNCE_INTERVAL_SECS)
                .unwrap_or_else(|_| panic!("Fatal: cannot create LPD announcer"))
        });

        Self {
            announcer: Arc::new(announcer),
            peers: Arc::new(RwLock::new(HashMap::new())),
            active_hashes: Arc::new(RwLock::new(HashSet::new())),
            _announce_task: None,
            receive_loop: LpdReceiveLoop::new(),
        }
    }

    /// Create LpdManager with custom configuration
    pub fn with_interval(announce_interval_secs: u64) -> Result<Self, String> {
        Self::with_interval_and_interface(announce_interval_secs, None)
    }

    /// Create an LPD manager with an optional local IPv4 multicast interface.
    pub fn with_interval_and_interface(
        announce_interval_secs: u64,
        interface: Option<std::net::Ipv4Addr>,
    ) -> Result<Self, String> {
        if announce_interval_secs == 0 {
            return Err("LPD announce interval must be greater than zero".to_string());
        }

        let announcer = LpdAnnouncer::with_interface(announce_interval_secs, interface)?;

        Ok(Self {
            announcer: Arc::new(announcer),
            peers: Arc::new(RwLock::new(HashMap::new())),
            active_hashes: Arc::new(RwLock::new(HashSet::new())),
            _announce_task: None,
            receive_loop: LpdReceiveLoop::new(),
        })
    }

    /// Register a torrent for LPD announcements
    ///
    /// Adds the info_hash to the active set so it gets periodically announced.
    ///
    /// Per BEP 0027, private torrents MUST NOT be announced via LPD.
    /// If `private_torrent` is true, this method returns an error without
    /// registering the hash. C++ `BtRegistry::add()` checks
    /// `torrentAttribute->private_torrent` before enabling LPD.
    pub async fn register_torrent(
        &self,
        info_hash: &str,
        private_torrent: bool,
    ) -> Result<(), String> {
        if private_torrent {
            debug!(
                info_hash = %&info_hash[..8],
                "Skipping LPD registration for private torrent (BEP 0027)"
            );
            return Err("Private torrents must not be announced via LPD (BEP 0027)".to_string());
        }

        let mut active = self.active_hashes.write().await;
        active.insert(info_hash.to_string());

        // Ensure peer set exists
        let mut peers_map = self.peers.write().await;
        peers_map.entry(info_hash.to_string()).or_default();

        info!(info_hash = %&info_hash[..8], "Torrent registered for LPD");
        Ok(())
    }

    /// Unregister a torrent from LPD announcements
    pub async fn unregister_torrent(&self, info_hash: &str) {
        let mut active = self.active_hashes.write().await;
        active.remove(info_hash);

        info!(info_hash = %&info_hash[..8], "Torrent unregistered from LPD");
    }

    /// Manual announce for a specific torrent
    pub async fn announce_torrent(&self, info_hash: &str, port: u16) -> Result<(), String> {
        self.announcer.announce(info_hash, port)?;
        Ok(())
    }

    /// Discover peers for a specific info_hash via LPD
    pub async fn discover_peers(&self, _info_hash: &str, timeout_ms: Option<u64>) -> Vec<LpdPeer> {
        let timeout =
            Duration::from_millis(timeout_ms.unwrap_or(constants::LPD_DEFAULT_RECEIVE_TIMEOUT_MS));
        self.announcer.receive_announcements(timeout)
    }

    /// Get all known peers for a given info_hash
    pub async fn get_peers_for(&self, info_hash: &str) -> Vec<LpdPeer> {
        let peers_map = self.peers.read().await;
        peers_map
            .get(info_hash)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Start periodic background announce task
    ///
    /// Spawns a Tokio task that announces all registered torrents every
    /// N seconds (default 5 min).
    ///
    /// # Arguments
    ///
    /// * `port` - Our BT client listen port
    ///
    /// # Returns
    ///
    /// JoinHandle that can be used to cancel the task
    pub fn start_background_announce(&mut self, port: u16) -> Option<tokio::task::JoinHandle<()>> {
        if !self.announcer.is_enabled() {
            debug!("LPD is disabled, not starting background announce");
            return None;
        }

        let announcer = Arc::clone(&self.announcer);
        let active_hashes = Arc::clone(&self.active_hashes);
        let interval = self.announcer.announce_interval();

        info!(
            interval_secs = interval.as_secs(),
            port, "Starting LPD background announce task"
        );

        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);

            loop {
                ticker.tick().await;

                let hashes: Vec<String> = {
                    let active = active_hashes.read().await;
                    active.iter().cloned().collect()
                };

                for info_hash in &hashes {
                    if let Err(e) = announcer.announce(info_hash, port) {
                        warn!(
                            info_hash = %&info_hash[..8.min(info_hash.len())],
                            error = %e,
                            "Background LPD announce failed"
                        );
                    }
                }

                debug!(
                    count = hashes.len(),
                    "LPD background announce cycle completed"
                );
            }
        });

        self._announce_task = Some(handle);
        // Note: JoinHandle is not Clone, so we cannot return a copy.
        // The task is stored internally and can be stopped via stop_background_announce().
        None
    }

    /// Stop background announce task
    pub fn stop_background_announce(&mut self) {
        if let Some(handle) = self._announce_task.take() {
            handle.abort();
            info!("LPD background announce task stopped");
        }
    }

    /// Start the background LPD receive loop.
    ///
    /// Spawns a tokio task that continuously receives LPD multicast
    /// announcements on the standard LPD port (6771), parses them,
    /// and updates the peer registry. This is the Rust equivalent of
    /// the C++ `LpdReceiveCommand` which runs in the event loop.
    ///
    /// The receive loop:
    /// - Binds to 0.0.0.0:6771 and joins multicast group 239.192.152.143
    /// - Continuously receives and processes LPD announcements
    /// - Only processes announcements for info hashes in `active_hashes`
    /// - Updates the peer registry automatically
    /// - Can be stopped via `stop_receive_loop()` or cancellation token
    ///
    /// # Errors
    ///
    /// Returns `Err` if the UDP socket cannot be bound (e.g., port
    /// already in use, multicast unavailable in this environment).
    pub async fn start_receive_loop(&mut self) -> Result<(), String> {
        if !self.announcer.is_enabled() {
            debug!("LPD is disabled, not starting receive loop");
            return Ok(());
        }

        self.receive_loop
            .start(Arc::clone(&self.peers), Arc::clone(&self.active_hashes))
            .await
    }

    /// Stop the background LPD receive loop gracefully.
    ///
    /// Signals the receive task to stop via `CancellationToken` and
    /// waits for it to finish. After stopping, `start_receive_loop()`
    /// can be called again to restart.
    pub async fn stop_receive_loop(&mut self) {
        self.receive_loop.stop().await;
    }

    /// Check if the LPD receive loop is currently running.
    pub fn is_receive_loop_running(&self) -> bool {
        self.receive_loop.is_running()
    }

    /// Get a clone of the receive loop's cancellation token.
    ///
    /// This allows external code (e.g., the download engine shutdown
    /// sequence) to cancel the receive loop without holding a mutable
    /// reference to `LpdManager`.
    pub fn receive_loop_cancellation_token(&self) -> CancellationToken {
        self.receive_loop.cancellation_token()
    }

    /// Update peer registry with newly discovered peers
    pub async fn update_peers(&self, info_hash: &str, new_peers: Vec<LpdPeer>) {
        let mut peers_map = self.peers.write().await;
        let entry = peers_map.entry(info_hash.to_string()).or_default();

        for peer in new_peers {
            // Limit total peers per hash
            if entry.len() >= MAX_PEERS_PER_HASH {
                // Remove oldest expired peer first
                let oldest = entry.iter().max_by_key(|p| p.last_seen.elapsed()).cloned();
                if let Some(oldest_peer) = oldest {
                    entry.remove(&oldest_peer);
                }
            }
            entry.insert(peer);
        }
    }

    /// Clean up expired peers from all registries
    pub async fn cleanup_expired_peers(&self, max_age: Duration) -> usize {
        let mut peers_map = self.peers.write().await;
        let mut removed = 0usize;

        for peers in peers_map.values_mut() {
            let before = peers.len();
            peers.retain(|p| !p.is_expired(max_age));
            removed += before - peers.len();
        }

        if removed > 0 {
            debug!(removed, "Cleaned up expired LPD peers");
        }
        removed
    }

    /// Check if LPD is available and working
    pub fn is_available(&self) -> bool {
        self.announcer.is_enabled()
    }
}

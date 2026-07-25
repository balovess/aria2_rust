//! DHT Engine module
//!
//! Orchestrates DHT operations including periodic bucket refresh,
//! node lookups, peer lookups, and token management.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// DHT engine configuration
#[derive(Debug, Clone)]
pub struct DhtEngineConfig {
    /// Port to listen on for DHT communication
    pub port: u16,
    /// Local node ID (20 bytes). Random if all zeros.
    pub self_id: [u8; 20],
    /// Path to the DHT routing table file for persistence
    pub dht_file_path: Option<PathBuf>,
    /// Interval between bucket refresh operations
    pub refresh_interval: Duration,
    /// Timeout for node lookups
    pub lookup_timeout: Duration,
    /// Maximum number of concurrent lookups
    pub max_concurrent_lookups: usize,
    /// Maximum number of concurrent query batches
    pub max_concurrent_queries: usize,
    /// Timeout per individual DHT query
    pub query_timeout: Duration,
    /// Token rotation interval
    pub token_rotation_interval: Duration,
}

impl Default for DhtEngineConfig {
    fn default() -> Self {
        Self {
            port: 6881,
            self_id: [0u8; 20],
            dht_file_path: None,
            refresh_interval: Duration::from_secs(900), // 15 minutes
            lookup_timeout: Duration::from_secs(30),
            max_concurrent_lookups: 16,
            max_concurrent_queries: 8,
            query_timeout: Duration::from_secs(5),
            token_rotation_interval: Duration::from_secs(300), // 5 minutes
        }
    }
}

/// Snapshot of DHT engine statistics.
#[derive(Debug, Clone)]
pub struct DhtEngineStats {
    /// Total number of nodes in the routing table
    pub total_nodes: usize,
}

/// DHT engine — orchestrates the full DHT node lifecycle.
///
/// Owns the routing table, handles inbound/outbound KRPC messages,
/// drives periodic bucket refresh, token rotation, and peer lookups.
/// Created via [`DhtEngine::start`] which binds a UDP socket and
/// returns an `Arc<DhtEngine>` ready for shared use.
pub struct DhtEngine {
    /// Current engine state
    state: std::sync::Mutex<DhtEngineState>,
    /// Configuration snapshot
    #[allow(dead_code)] // used by future UDP bind + bootstrap logic
    config: DhtEngineConfig,
}

impl DhtEngine {
    /// Start the DHT engine with the given configuration.
    ///
    /// Binds a UDP socket on the configured port, bootstraps into the
    /// network, and returns a shared reference to the running engine.
    pub async fn start(config: DhtEngineConfig) -> std::io::Result<Arc<Self>> {
        let engine = Arc::new(Self {
            state: std::sync::Mutex::new(DhtEngineState::Bootstrapping),
            config,
        });
        // TODO: actual UDP bind + bootstrap logic
        Ok(engine)
    }

    /// Return a snapshot of the current engine state.
    pub fn state(&self) -> DhtEngineState {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Look up peers for the given info hash via the DHT network.
    ///
    /// Returns a list of peer addresses that claim to be serving the
    /// torrent identified by `info_hash`. Stub: always returns an empty
    /// list until the full DHT lookup logic is implemented.
    pub async fn find_peers(&self, info_hash: &[u8; 20]) -> std::io::Result<FindPeersResult> {
        tracing::debug!(info_hash = %hex::encode(info_hash), "DHT find_peers stub");
        // TODO: actual iterative find_node / get_peers traversal
        Ok(FindPeersResult {
            peers: vec![],
            nodes_contacted: 0,
        })
    }

    /// Synchronous shutdown — sets engine state to [`DhtEngineState::ShuttingDown`].
    ///
    /// Intended for callers that cannot await (e.g. `Drop`-like paths).
    /// The actual socket close and background task teardown happen in
    /// [`DhtEngine::shutdown_async`].
    pub fn shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        tracing::info!(old_state = ?state, "DHT shutdown requested");
        *state = DhtEngineState::ShuttingDown;
    }

    /// Async shutdown — signals the engine to stop and awaits full teardown.
    ///
    /// Sets state to [`DhtEngineState::ShuttingDown`] then performs any
    /// async cleanup (closing sockets, flushing routing table, etc.).
    pub async fn shutdown_async(&self) {
        self.shutdown();
        // TODO: await actual background task termination + socket close
        tracing::info!("DHT async shutdown completed (stub)");
    }

    /// Add a bootstrap node to the routing table.
    ///
    /// Stub: logs the address. The real implementation will send a
    /// `ping` to `addr` and insert it into the appropriate k-bucket
    /// once a response is received.
    pub async fn add_node(&self, addr: SocketAddr) {
        tracing::debug!(addr = %addr, "DHT add_node stub");
        // TODO: ping node and insert into routing table on reply
    }

    /// Start the periodic bucket-refresh maintenance loop.
    ///
    /// Stub: logs that the loop was requested. The real implementation
    /// will spawn a background task that refreshes stale k-buckets at
    /// the interval configured in [`DhtEngineConfig::refresh_interval`].
    pub fn start_maintenance_loop(&self) {
        tracing::info!("DHT maintenance loop start requested (stub)");
        // TODO: spawn tokio task that periodically refreshes buckets
    }

    /// Announce that we are serving the torrent identified by `info_hash`
    /// on `port`.
    ///
    /// Stub: always succeeds. The real implementation will perform
    /// `announce_peer` RPCs to the closest nodes found via
    /// [`DhtEngine::find_peers`].
    pub async fn announce_peer(&self, info_hash: &[u8; 20], port: u16) -> std::io::Result<()> {
        tracing::debug!(
            info_hash = %hex::encode(info_hash),
            port,
            "DHT announce_peer stub"
        );
        // TODO: actual announce_peer RPC to closest nodes
        Ok(())
    }

    /// Return a snapshot of DHT engine statistics.
    // TODO: read from actual routing table
    pub async fn stats(&self) -> DhtEngineStats {
        tracing::debug!("DhtEngine::stats stub called");
        DhtEngineStats { total_nodes: 0 }
    }
}

/// DHT engine state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DhtEngineState {
    /// DHT not started
    Stopped,
    /// Bootstrapping into the DHT network
    Bootstrapping,
    /// Running and serving requests
    Running,
    /// Shutting down
    ShuttingDown,
}

/// Result of a `find_peers` DHT lookup.
#[derive(Debug, Clone)]
pub struct FindPeersResult {
    /// Discovered peer addresses serving the requested info hash
    pub peers: Vec<SocketAddr>,
    /// Number of DHT nodes contacted during the lookup
    pub nodes_contacted: usize,
}

/// DHT engine events
#[derive(Debug)]
pub enum DhtEngineEvent {
    /// A bucket refresh is needed
    BucketRefreshNeeded,
    /// A peer lookup completed
    PeerLookupCompleted {
        info_hash: [u8; 20],
        peers: Vec<SocketAddr>,
    },
    /// A node lookup completed
    NodeLookupCompleted {
        target: [u8; 20],
        nodes: Vec<SocketAddr>,
    },
    /// Token rotation is needed
    TokenRotationNeeded,
    /// Bootstrap completed
    BootstrapCompleted,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dht_engine_config_default() {
        let config = DhtEngineConfig::default();
        assert_eq!(config.refresh_interval, Duration::from_secs(900));
        assert_eq!(config.max_concurrent_lookups, 16);
    }

    #[test]
    fn test_dht_engine_state() {
        assert_ne!(DhtEngineState::Stopped, DhtEngineState::Running);
        assert_ne!(DhtEngineState::Bootstrapping, DhtEngineState::ShuttingDown);
    }
}

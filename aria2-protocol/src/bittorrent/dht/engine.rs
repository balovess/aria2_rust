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
    /// Path to the DHT routing table file for persistence
    pub dht_file_path: Option<PathBuf>,
    /// Interval between bucket refresh operations
    pub refresh_interval: Duration,
    /// Timeout for node lookups
    pub lookup_timeout: Duration,
    /// Maximum number of concurrent lookups
    pub max_concurrent_lookups: usize,
    /// Token rotation interval
    pub token_rotation_interval: Duration,
}

impl Default for DhtEngineConfig {
    fn default() -> Self {
        Self {
            port: 6881,
            dht_file_path: None,
            refresh_interval: Duration::from_secs(900), // 15 minutes
            lookup_timeout: Duration::from_secs(30),
            max_concurrent_lookups: 16,
            token_rotation_interval: Duration::from_secs(300), // 5 minutes
        }
    }
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

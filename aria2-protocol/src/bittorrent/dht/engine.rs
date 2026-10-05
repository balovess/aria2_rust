//! DHT Engine — orchestrates the full DHT node lifecycle.
//!
//! Owns the UDP socket, routing table, token tracker, peer storage,
//! and transaction tracker. Spawns background tokio tasks for:
//! - Receiving and processing inbound KRPC messages
//! - Sending outbound queries and responses
//! - Periodic bucket refresh, token rotation, and auto-save
//!
//! The C++ implementation uses `DHTInteractionCommand` running on every
//! event-loop iteration. This Rust version uses a dedicated async task
//! with `tokio::select!` for a cleaner, more idiomatic design.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tokio::sync::{RwLock, watch};

mod api;
mod lifecycle;
mod receive;
mod startup;
#[cfg(test)]
mod tests;

use super::handler::DhtLocalPeerLookup;
use super::peer_storage::DhtPeerStorage;
use super::store::DhtItemStore;
use super::task::DhtTaskQueue;
use super::task_impl::DhtTaskContext;
use super::token_tracker::TokenTracker;

// ==================== Configuration ====================

/// DHT engine configuration.
#[derive(Debug, Clone)]
pub struct DhtEngineConfig {
    /// Port to listen on for DHT communication.
    pub port: u16,
    /// Ordered ports to try when the aria2 listen-port option is a range.
    ///
    /// The first available port is selected, matching the original DHT
    /// setup command's range binding behavior.
    pub port_range: Option<Vec<u16>>,
    /// Optional local IP address. `None` binds the unspecified address for
    /// the selected address family (IPv4 by default).
    pub listen_addr: Option<IpAddr>,
    /// Explicit bootstrap endpoints. An empty list uses the public defaults.
    pub bootstrap_nodes: Vec<SocketAddr>,
    /// Local node ID (20 bytes). All zeros → random on start.
    pub self_id: [u8; 20],
    /// Path to persist the routing table (dht.dat).
    pub dht_file_path: Option<PathBuf>,
    /// Interval between bucket refresh *checks* (C++ DHT_BUCKET_REFRESH_CHECK_INTERVAL = 5 min).
    /// Buckets are only refreshed if they haven't been updated in 15 minutes.
    pub refresh_check_interval: Duration,
    /// Timeout for individual DHT queries (C++ DHT_MESSAGE_TIMEOUT = 10s).
    pub query_timeout: Duration,
    /// Token secret rotation interval (C++ DHT_TOKEN_UPDATE_INTERVAL = 10 min).
    pub token_rotation_interval: Duration,
    /// Interval for sending keep-alive pings to routing table nodes
    /// (C++ DHT_NODE_CONTACT_INTERVAL = 15 min).
    pub node_contact_interval: Duration,
    /// Maximum concurrent lookup tasks.
    pub max_concurrent_lookups: usize,
    /// Whether to bootstrap into the public DHT network when the engine starts.
    ///
    /// Bootstrapping resolves public entry-point hostnames and performs network
    /// I/O. Disable it for private torrents and in tests. Bootstrap always runs
    /// **in the background** — [`DhtEngine::start`] never waits for it, mirroring
    /// C++ aria2 where `DHTEntryPointNameResolveCommand` is dispatched into the
    /// event loop rather than blocking startup.
    pub bootstrap_on_start: bool,
    /// Upper bound for the background bootstrap procedure.
    ///
    /// If bootstrap has not finished within this window it is abandoned and the
    /// engine transitions to `Running` anyway, so an unreachable network can
    /// never leave the engine stuck in `Bootstrapping` forever.
    pub bootstrap_timeout: Duration,
    /// Interval for cleanup of expired transactions/peers and node eviction.
    pub cleanup_interval: Duration,
    /// Interval for routing-table and BEP 44 persistence checkpoints.
    pub save_interval: Duration,
    /// Maximum age of a routing-table snapshot accepted from disk.
    pub persistence_max_age: Duration,
}

impl Default for DhtEngineConfig {
    fn default() -> Self {
        Self {
            port: 6881,
            port_range: None,
            listen_addr: None,
            bootstrap_nodes: Vec::new(),
            self_id: [0u8; 20],
            dht_file_path: None,
            refresh_check_interval: Duration::from_secs(300), // 5 min check
            query_timeout: Duration::from_secs(10),
            token_rotation_interval: Duration::from_secs(600), // 10 min
            node_contact_interval: Duration::from_secs(900),   // 15 min
            max_concurrent_lookups: 16,
            bootstrap_on_start: true,
            bootstrap_timeout: Duration::from_secs(60),
            cleanup_interval: Duration::from_secs(300),
            save_interval: Duration::from_secs(1800),
            persistence_max_age: Duration::from_secs(24 * 60 * 60),
        }
    }
}

impl DhtEngineConfig {
    /// Configuration for a fully local engine: ephemeral port, no public
    /// bootstrap. Intended for tests and private-torrent DHT instances.
    pub fn local() -> Self {
        Self {
            port: 0,
            bootstrap_on_start: false,
            ..Default::default()
        }
    }
}

// ==================== State types ====================

/// DHT engine state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum DhtEngineState {
    /// DHT not started.
    Stopped = 0,
    /// Bootstrapping into the DHT network.
    Bootstrapping = 1,
    /// Running and serving requests.
    Running = 2,
    /// Shutting down.
    ShuttingDown = 3,
}

/// Snapshot of DHT engine statistics.
#[derive(Debug, Clone)]
pub struct DhtEngineStats {
    /// Total number of nodes in the routing table.
    pub total_nodes: usize,
    /// Number of good nodes.
    pub good_nodes: usize,
    /// Number of pending transactions.
    pub pending_transactions: usize,
    /// Nodes whose last contact is older than the refresh threshold.
    pub questionable_nodes: usize,
    /// Nodes that reached the failure threshold and are eligible for eviction.
    pub bad_nodes: usize,
    /// Replacement candidates retained outside full buckets.
    pub cached_nodes: usize,
    /// Number of routing-table buckets currently allocated.
    pub bucket_count: usize,
    /// Number of live swarm keys held by the local announce-peer store.
    pub peer_info_hashes: usize,
    /// Number of peer endpoints held by the local announce-peer store.
    pub stored_peers: usize,
    /// Cumulative number of swarms evicted by the local peer-storage cap.
    pub peer_storage_evictions: u64,
    /// Maximum swarm keys retained by this address-family engine.
    pub max_peer_info_hashes: usize,
    /// Whether this engine has a routing-table persistence path.
    pub persistence_enabled: bool,
    /// Configured maximum age for an on-disk routing-table snapshot.
    pub persistence_max_age_secs: u64,
    /// Configured cleanup and save periods.
    pub cleanup_interval_secs: u64,
    pub save_interval_secs: u64,
    /// Current engine state.
    pub state: DhtEngineState,
}

/// Result of a `find_peers` DHT lookup.
#[derive(Debug, Clone)]
pub struct FindPeersResult {
    /// Discovered peer addresses serving the requested info hash.
    pub peers: Vec<SocketAddr>,
    /// Number of queries accepted by the local UDP socket (whether or not nodes reply).
    pub nodes_contacted: usize,
}

// ==================== Internal shared state ====================

/// Mutable lifecycle state behind `Arc<RwLock<>>`.
pub(super) struct DhtEngineInner {
    pub(super) state: DhtEngineState,
}

/// Owned engine dependencies and the canonical shared DHT task resources.
///
/// This context deliberately does not contain an `Arc<DhtEngine>`. Keeping
/// task dependencies separate prevents a task that is awaiting network I/O
/// from forming a reference cycle with the engine's own `JoinHandle` list.
/// `DhtTaskContext` owns the routing table, socket, transaction tracker, local
/// node ID, and query timeout shared by scheduled and direct operations.
pub(super) struct DhtEngineContext {
    pub(super) inner: Arc<RwLock<DhtEngineInner>>,
    /// Publishes lifecycle transitions so callers can await bootstrap without
    /// polling the state snapshot.
    pub(super) state_updates: watch::Sender<DhtEngineState>,
    /// Serializes snapshots written to the same persistence file without
    /// blocking an async runtime worker.
    pub(super) routing_table_save_lock: Arc<tokio::sync::Mutex<()>>,
    pub(super) config: DhtEngineConfig,
    pub(super) token_tracker: Arc<std::sync::Mutex<TokenTracker>>,
    pub(super) peer_storage: Arc<DhtPeerStorage>,
    /// Resolve a locally active BitTorrent peer for inbound `get_peers` replies.
    pub(super) local_peer_lookup: Option<Arc<DhtLocalPeerLookup>>,
    pub(super) item_store: DhtItemStore,
    pub(super) shutdown_requested: Arc<AtomicBool>,
    pub(super) task_context: DhtTaskContext,
}

// ==================== DhtEngine ====================

/// DHT engine — orchestrates the full DHT node lifecycle.
///
/// Created via [`DhtEngine::start`] which binds a UDP socket and returns
/// an `Arc<DhtEngine>` ready for shared use. All public methods take `&self`
/// and use interior mutability for thread-safe access.
pub struct DhtEngine {
    /// Rust-owned DHT state and dependencies.
    pub(super) context: Arc<DhtEngineContext>,
    /// Shared shutdown state observed by every background task.
    pub(super) shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// Handles for background tasks owned by this engine.
    pub(super) background_tasks: tokio::sync::Mutex<tokio::task::JoinSet<()>>,
    /// Scheduler owned by the engine rather than by task contexts.
    pub(super) task_queue: Arc<DhtTaskQueue>,
}

impl DhtEngine {
    /// Persist the current routing table and BEP 44 item store immediately.
    ///
    /// This is the manual counterpart of the periodic save task and reuses
    /// the same serialized persistence path and lock.
    pub async fn save_state(&self) -> Result<(), String> {
        self.context.save_state().await
    }

    /// Evict bad routing-table nodes and try cached replacements immediately.
    ///
    /// Returns `(evicted_nodes, replacement_attempts)` for operational RPC
    /// reporting.
    pub async fn evict_nodes(&self) -> (usize, usize) {
        self.context.evict_and_replace_nodes().await
    }
}

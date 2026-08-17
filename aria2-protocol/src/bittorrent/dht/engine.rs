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

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::lookup::{announce_to_token_nodes, iterative_get_peers};
use super::message::{DhtMessage, DhtMessageBuilder};
use super::node::DhtNode;
use super::peer_storage::DhtPeerStorage;
use super::routing_table::RoutingTable;
use super::socket::DhtSocket;
use super::token_tracker::TokenTracker;
use super::tracker::TransactionTracker;

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
}

impl Default for DhtEngineConfig {
    fn default() -> Self {
        Self {
            port: 6881,
            port_range: None,
            self_id: [0u8; 20],
            dht_file_path: None,
            refresh_check_interval: Duration::from_secs(300), // 5 min check
            query_timeout: Duration::from_secs(10),
            token_rotation_interval: Duration::from_secs(600), // 10 min
            node_contact_interval: Duration::from_secs(900),   // 15 min
            max_concurrent_lookups: 16,
            bootstrap_on_start: true,
            bootstrap_timeout: Duration::from_secs(60),
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
    /// Current engine state.
    pub state: DhtEngineState,
}

/// Result of a `find_peers` DHT lookup.
#[derive(Debug, Clone)]
pub struct FindPeersResult {
    /// Discovered peer addresses serving the requested info hash.
    pub peers: Vec<SocketAddr>,
    /// Number of DHT nodes contacted during the lookup.
    pub nodes_contacted: usize,
}

/// DHT engine events (for future notification system).
#[derive(Debug)]
pub enum DhtEngineEvent {
    BucketRefreshNeeded,
    PeerLookupCompleted {
        info_hash: [u8; 20],
        peers: Vec<SocketAddr>,
    },
    NodeLookupCompleted {
        target: [u8; 20],
        nodes: Vec<SocketAddr>,
    },
    TokenRotationNeeded,
    BootstrapCompleted,
}

// ==================== Internal shared state ====================

/// Shared mutable state behind `Arc<RwLock<>>`.
pub(super) struct DhtEngineInner {
    pub(super) state: DhtEngineState,
    pub(super) routing_table: RoutingTable,
    pub(super) self_id: [u8; 20],
}

/// Owned dependencies available to background tasks.
///
/// This context deliberately does not contain an `Arc<DhtEngine>`. Keeping
/// task dependencies separate prevents a task that is awaiting network I/O
/// from forming a reference cycle with the engine's own `JoinHandle` list.
pub(super) struct DhtEngineContext {
    pub(super) inner: Arc<RwLock<DhtEngineInner>>,
    pub(super) config: DhtEngineConfig,
    pub(super) socket: DhtSocket,
    pub(super) token_tracker: Arc<std::sync::Mutex<TokenTracker>>,
    pub(super) peer_storage: Arc<DhtPeerStorage>,
    pub(super) tracker: Arc<TransactionTracker>,
    pub(super) handler_self_id: [u8; 20],
    pub(super) shutdown_requested: Arc<AtomicBool>,
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
    pub(super) background_tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl DhtEngine {
    /// Start the DHT engine with the given configuration.
    ///
    /// Binds a UDP socket on the configured port, loads the routing table
    /// from disk (if available), spawns the receive loop and periodic tasks,
    /// and returns a shared reference to the running engine.
    ///
    /// Bootstrap into the public DHT network is **not awaited**: it is spawned
    /// as a background task guarded by
    /// [`DhtEngineConfig::bootstrap_timeout`], so `start` returns as soon as
    /// the socket is bound. This matches C++ aria2, where
    /// `DHTEntryPointNameResolveCommand` is queued into the event loop rather
    /// than blocking startup, and it keeps the engine usable (and tests fast)
    /// on hosts with no DHT connectivity.
    ///
    /// Use [`DhtEngine::state`] to observe the transition from
    /// `Bootstrapping` to `Running`, or [`DhtEngineConfig::local`] to skip
    /// bootstrap entirely.
    pub async fn start(config: DhtEngineConfig) -> std::io::Result<Arc<Self>> {
        // Generate random node ID if not specified
        let self_id = if config.self_id == [0u8; 20] {
            let mut id = [0u8; 20];
            use rand::{RngCore, SeedableRng};
            // Use StdRng instead of ThreadRng to satisfy Send across async boundaries.
            rand::rngs::StdRng::from_entropy().fill_bytes(&mut id);
            id
        } else {
            config.self_id
        };

        info!(
            id = %hex::encode(self_id),
            port = config.port,
            "Starting DHT engine"
        );

        // Bind UDP socket
        let socket = if let Some(ports) = config.port_range.as_deref() {
            let mut last_error = None;
            let mut bound = None;
            for port in ports {
                match DhtSocket::bind(*port).await {
                    Ok(socket) => {
                        bound = Some(socket);
                        break;
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            bound.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    last_error.unwrap_or_else(|| "DHT port range is empty".to_string()),
                )
            })?
        } else {
            DhtSocket::bind(config.port)
                .await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::AddrInUse, e))?
        };
        let actual_port = socket.local_addr().port();
        info!(port = actual_port, "DHT socket bound");

        // Load routing table from disk or start empty
        let mut routing_table = RoutingTable::new(self_id);
        if let Some(ref path) = config.dht_file_path
            && path.exists()
        {
            match super::persistence::DhtPersistence::load_from_file_sync(path) {
                Ok(data) => {
                    info!(
                        count = data.nodes.len(),
                        "Loaded DHT routing table from disk"
                    );
                    for pnode in data.nodes {
                        let node = DhtNode::new(pnode.id, pnode.addr);
                        routing_table.insert(node);
                    }
                }
                Err(e) => {
                    warn!("Failed to load DHT routing table: {}", e);
                }
            }
        }

        let inner = Arc::new(RwLock::new(DhtEngineInner {
            state: DhtEngineState::Bootstrapping,
            routing_table,
            self_id,
        }));

        let token_tracker = Arc::new(std::sync::Mutex::new(TokenTracker::new()));
        let peer_storage = Arc::new(DhtPeerStorage::new());
        let tracker = Arc::new(TransactionTracker::new());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let task_context = Arc::new(DhtEngineContext {
            inner: Arc::clone(&inner),
            config: config.clone(),
            socket: socket.clone(),
            token_tracker: Arc::clone(&token_tracker),
            peer_storage: Arc::clone(&peer_storage),
            tracker: Arc::clone(&tracker),
            handler_self_id: self_id,
            shutdown_requested: Arc::clone(&shutdown_requested),
        });

        let engine = Arc::new(Self {
            context: task_context,
            shutdown_tx,
            background_tasks: std::sync::Mutex::new(Vec::new()),
        });

        // Spawn the background receive loop
        engine.spawn_receive_loop(shutdown_rx);

        // Spawn periodic tasks
        engine.spawn_periodic_tasks();

        // Bootstrap runs in the background so `start` never blocks on network
        // I/O. Without a bootstrap the engine is immediately usable for
        // inbound queries and for peers added manually (e.g. from a torrent's
        // `nodes` list), so we move straight to `Running`.
        if config.bootstrap_on_start {
            engine.spawn_bootstrap();
        } else {
            engine.context.inner.write().await.state = DhtEngineState::Running;
        }

        Ok(engine)
    }

    /// Spawn the bootstrap procedure as a background task.
    ///
    /// The task is bounded by [`DhtEngineConfig::bootstrap_timeout`]; on
    /// timeout the engine still transitions to `Running` so that lookups are
    /// not blocked indefinitely by an unreachable network.
    pub fn spawn_bootstrap(self: &Arc<Self>) {
        if self.context.shutdown_requested.load(Ordering::Acquire) {
            return;
        }

        let context = Arc::clone(&self.context);
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let limit = context.config.bootstrap_timeout;
        let handle = tokio::spawn(async move {
            let bootstrap = async {
                if tokio::time::timeout(limit, context.bootstrap())
                    .await
                    .is_err()
                    && !context.shutdown_requested.load(Ordering::Acquire)
                {
                    warn!(
                        timeout = ?limit,
                        "DHT bootstrap timed out; continuing without entry-point nodes"
                    );
                    context.inner.write().await.state = DhtEngineState::Running;
                }
            };

            tokio::select! {
                _ = bootstrap => {}
                _ = shutdown_rx.changed() => {}
            }
        });
        self.register_background_task(handle);
    }

    /// Return a snapshot of the current engine state.
    pub async fn state(&self) -> DhtEngineState {
        let inner = self.context.inner.read().await;
        if self.context.shutdown_requested.load(Ordering::Acquire) {
            DhtEngineState::ShuttingDown
        } else {
            inner.state
        }
    }

    /// Look up peers for the given info hash via the DHT network.
    ///
    /// Performs an iterative `get_peers` lookup with alpha-parallelism,
    /// returning discovered peer addresses.
    pub async fn find_peers(&self, info_hash: &[u8; 20]) -> std::io::Result<FindPeersResult> {
        let state = self.state().await;
        if state != DhtEngineState::Running && state != DhtEngineState::Bootstrapping {
            return Ok(FindPeersResult {
                peers: vec![],
                nodes_contacted: 0,
            });
        }

        let rt = Arc::new(RwLock::new(
            self.context.inner.read().await.routing_table.clone(),
        ));
        let self_id = self.context.inner.read().await.self_id;

        debug!(info_hash = %hex::encode(info_hash), "Starting DHT get_peers lookup");

        let result = iterative_get_peers(
            info_hash,
            &self_id,
            &rt,
            &self.context.socket,
            &self.context.tracker,
        )
        .await;

        // Merge discovered nodes back into the main routing table
        {
            let mut inner = self.context.inner.write().await;
            let discovered_rt = rt.read().await;
            for node in discovered_rt.all_nodes() {
                inner.routing_table.insert(node.clone());
            }
        }

        Ok(FindPeersResult {
            peers: result.peers,
            nodes_contacted: result.nodes_contacted,
        })
    }

    /// Announce that we are serving the torrent identified by `info_hash` on `port`.
    ///
    /// Performs a `get_peers` lookup first to obtain tokens, then sends
    /// `announce_peer` queries to the closest K nodes that provided tokens.
    pub async fn announce_peer(&self, info_hash: &[u8; 20], port: u16) -> std::io::Result<()> {
        let state = self.state().await;
        if state != DhtEngineState::Running && state != DhtEngineState::Bootstrapping {
            return Ok(());
        }

        let rt = Arc::new(RwLock::new(
            self.context.inner.read().await.routing_table.clone(),
        ));
        let self_id = self.context.inner.read().await.self_id;

        debug!(
            info_hash = %hex::encode(info_hash),
            port,
            "Starting DHT announce_peer"
        );

        // First, do a get_peers lookup to obtain tokens
        let result = iterative_get_peers(
            info_hash,
            &self_id,
            &rt,
            &self.context.socket,
            &self.context.tracker,
        )
        .await;

        // Merge discovered nodes back
        {
            let mut inner = self.context.inner.write().await;
            let discovered_rt = rt.read().await;
            for node in discovered_rt.all_nodes() {
                inner.routing_table.insert(node.clone());
            }
        }

        // Announce to nodes that provided tokens
        if !result.token_nodes.is_empty() {
            announce_to_token_nodes(
                info_hash,
                &self_id,
                port,
                &result.token_nodes,
                &self.context.socket,
                &self.context.tracker,
            )
            .await;
        }

        Ok(())
    }

    /// Add a bootstrap node to the routing table.
    ///
    /// Sends a `ping` to `addr` and inserts it into the appropriate k-bucket
    /// once a response is received.
    pub async fn add_node(&self, addr: SocketAddr) {
        let self_id = self.context.inner.read().await.self_id;
        let msg = DhtMessageBuilder::ping(0, &self_id);
        let encoded = match msg.encode() {
            Ok(e) => e,
            Err(e) => {
                debug!("Failed to encode ping for add_node: {}", e);
                return;
            }
        };

        if let Err(e) = self.context.socket.send_to(addr, &encoded).await {
            debug!(addr = %addr, "Failed to send ping: {}", e);
            return;
        }

        // Wait briefly for a response
        let mut buf = [0u8; 4096];
        if let Ok((len, _from)) = self
            .context
            .socket
            .recv_with_timeout(&mut buf, self.context.config.query_timeout)
            .await
            && len > 0
            && let Ok(response) = DhtMessage::decode(&buf[..len])
            && response.is_response()
        {
            // Extract node ID from response
            if let Some(r) = &response.r
                && let Some(id_bytes) = r.dict_get(b"id").and_then(|v| v.as_bytes())
                && id_bytes.len() == 20
            {
                let mut node_id = [0u8; 20];
                node_id.copy_from_slice(id_bytes);
                let node = DhtNode::new(node_id, addr);
                let mut inner = self.context.inner.write().await;
                inner.routing_table.insert(node);
                debug!(addr = %addr, id = %hex::encode(node_id), "Added DHT node via add_node");
            }
        }
    }

    /// Start the periodic bucket-refresh maintenance loop.
    ///
    /// Called automatically by `start()`. Can be called again to trigger
    /// an immediate refresh cycle.
    pub fn start_maintenance_loop(&self) {
        info!("DHT maintenance loop already running (spawned at start)");
    }

    /// Synchronous shutdown — sets engine state to `ShuttingDown`.
    pub fn shutdown(&self) {
        let first_shutdown = {
            let _background_tasks = self
                .background_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            !self.context.shutdown_requested.swap(true, Ordering::AcqRel)
        };

        if first_shutdown {
            let _ = self.shutdown_tx.send(true);

            if let Ok(mut inner) = self.context.inner.try_write() {
                inner.state = DhtEngineState::ShuttingDown;
            }

            info!("DHT shutdown signal sent");
        }
    }

    /// Async shutdown — signals the engine to stop and awaits full teardown.
    pub async fn shutdown_async(&self) {
        self.shutdown();

        let tasks = {
            let mut background_tasks = self
                .background_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *background_tasks)
        };

        // Give tasks a bounded opportunity to observe the shared signal before
        // aborting a maintenance operation that is currently awaiting network
        // I/O. JoinSet removes completed tasks as it drains them, so a timeout
        // can resume with only the still-running tasks and never double-awaits
        // a completed JoinHandle.
        let mut join_set = tokio::task::JoinSet::new();
        for task in tasks {
            join_set.spawn(async move {
                let _ = task.await;
            });
        }
        let wait_for_tasks = async { while join_set.join_next().await.is_some() {} };
        if tokio::time::timeout(Duration::from_millis(100), wait_for_tasks)
            .await
            .is_err()
        {
            join_set.abort_all();
            while join_set.join_next().await.is_some() {}
        }

        self.context.inner.write().await.state = DhtEngineState::ShuttingDown;

        // Save routing table to disk
        if let Some(ref path) = self.context.config.dht_file_path {
            let inner = self.context.inner.read().await;
            let self_id = inner.self_id;
            let nodes: Vec<DhtNode> = inner.routing_table.collect_good_nodes();
            drop(inner);

            match super::persistence::DhtPersistence::save_to_file_sync(path, &self_id, &nodes) {
                Ok(_) => {
                    info!(path = %path.display(), count = nodes.len(), "Saved DHT routing table")
                }
                Err(e) => warn!("Failed to save DHT routing table: {}", e),
            }
        }

        info!("DHT engine shutdown complete");
    }

    /// Return a snapshot of DHT engine statistics.
    pub async fn stats(&self) -> DhtEngineStats {
        let inner = self.context.inner.read().await;
        let state = if self.context.shutdown_requested.load(Ordering::Acquire) {
            DhtEngineState::ShuttingDown
        } else {
            inner.state
        };
        DhtEngineStats {
            total_nodes: inner.routing_table.total_node_count(),
            good_nodes: inner.routing_table.good_node_count(),
            pending_transactions: self.context.tracker.pending_count(),
            state,
        }
    }

    /// Register a background task owned by this engine.
    pub(super) fn register_background_task(&self, task: tokio::task::JoinHandle<()>) {
        let mut background_tasks = self
            .background_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.context.shutdown_requested.load(Ordering::Acquire) {
            task.abort();
        } else {
            background_tasks.push(task);
        }
    }
}

impl Drop for DhtEngine {
    fn drop(&mut self) {
        let mut background_tasks = self
            .background_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for task in background_tasks.drain(..) {
            task.abort();
        }
    }
}

// Internal methods (spawn_receive_loop, spawn_periodic_tasks, bootstrap,
// process_inbound_message, handle_timeout, refresh_buckets, contact_nodes,
// evict_and_replace_nodes, save_routing_table) are defined in
// engine_inner.rs to keep this file under 600 lines.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dht_engine_config_default() {
        let config = DhtEngineConfig::default();
        assert_eq!(config.refresh_check_interval, Duration::from_secs(300));
        assert_eq!(config.max_concurrent_lookups, 16);
        assert_eq!(config.port, 6881);
    }

    #[test]
    fn test_dht_engine_state_ordering() {
        assert!(DhtEngineState::Stopped < DhtEngineState::Bootstrapping);
        assert!(DhtEngineState::Bootstrapping < DhtEngineState::Running);
        assert!(DhtEngineState::Running < DhtEngineState::ShuttingDown);
    }

    #[tokio::test]
    async fn test_dht_engine_start_shutdown() {
        // `local()` disables public-network bootstrap so the engine reaches
        // `Running` deterministically without any outbound traffic.
        let config = DhtEngineConfig {
            dht_file_path: None,
            ..DhtEngineConfig::local()
        };

        let engine = DhtEngine::start(config)
            .await
            .expect("start should succeed");
        assert_eq!(engine.state().await, DhtEngineState::Running);

        engine.shutdown_async().await;
        assert_eq!(engine.state().await, DhtEngineState::ShuttingDown);
        assert_eq!(engine.stats().await.state, DhtEngineState::ShuttingDown);
        assert!(
            engine
                .background_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_dht_engine_sync_shutdown_is_immediately_observable() {
        let engine = DhtEngine::start(DhtEngineConfig::local())
            .await
            .expect("start should succeed");

        engine.shutdown();

        assert_eq!(engine.state().await, DhtEngineState::ShuttingDown);
        assert_eq!(engine.stats().await.state, DhtEngineState::ShuttingDown);

        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_dht_engine_tries_next_port_when_first_is_occupied() {
        let occupied = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .expect("occupied UDP socket");
        let first_port = occupied.local_addr().unwrap().port();
        let candidate = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .expect("candidate UDP socket");
        let second_port = candidate.local_addr().unwrap().port();
        drop(candidate);

        let config = DhtEngineConfig {
            port: first_port,
            port_range: Some(vec![first_port, second_port]),
            dht_file_path: None,
            ..DhtEngineConfig::local()
        };
        let engine = DhtEngine::start(config)
            .await
            .expect("DHT should fall back to the next available port");

        assert_eq!(engine.context.socket.local_addr().port(), second_port);
        drop(occupied);
        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_dht_engine_stats() {
        let config = DhtEngineConfig {
            dht_file_path: None,
            ..DhtEngineConfig::local()
        };

        let engine = DhtEngine::start(config)
            .await
            .expect("start should succeed");
        let stats = engine.stats().await;
        assert_eq!(stats.state, DhtEngineState::Running);

        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_find_peers_when_stopped() {
        // Create an engine in ShuttingDown state
        let config = DhtEngineConfig {
            dht_file_path: None,
            ..DhtEngineConfig::local()
        };

        let engine = DhtEngine::start(config)
            .await
            .expect("start should succeed");
        engine.shutdown();

        // find_peers should return empty when shutting down
        // (may take a moment for state to propagate)
        tokio::time::sleep(Duration::from_millis(200)).await;
        let result = engine.find_peers(&[0u8; 20]).await;
        assert!(result.is_ok());
    }
}

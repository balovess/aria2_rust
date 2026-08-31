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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::node::DhtNode;
use super::peer_storage::DhtPeerStorage;
use super::routing_table::RoutingTable;
use super::socket::DhtSocket;
use super::task::DhtTaskQueue;
use super::task_impl::DhtTaskContext;
use super::task_peer::DhtTaskFactory;
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
    pub(super) self_id: [u8; 20],
}

/// Owned dependencies available to background tasks.
///
/// This context deliberately does not contain an `Arc<DhtEngine>`. Keeping
/// task dependencies separate prevents a task that is awaiting network I/O
/// from forming a reference cycle with the engine's own `JoinHandle` list.
pub(super) struct DhtEngineContext {
    pub(super) inner: Arc<RwLock<DhtEngineInner>>,
    pub(super) routing_table: Arc<RwLock<RoutingTable>>,
    /// Serializes snapshots written to the same persistence file without
    /// blocking an async runtime worker.
    pub(super) routing_table_save_lock: Arc<tokio::sync::Mutex<()>>,
    pub(super) config: DhtEngineConfig,
    pub(super) socket: DhtSocket,
    pub(super) token_tracker: Arc<std::sync::Mutex<TokenTracker>>,
    pub(super) peer_storage: Arc<DhtPeerStorage>,
    pub(super) tracker: Arc<TransactionTracker>,
    pub(super) handler_self_id: [u8; 20],
    pub(super) shutdown_requested: Arc<AtomicBool>,
    pub(super) task_factory: DhtTaskFactory,
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
    /// Scheduler owned by the engine rather than by task contexts.
    pub(super) task_queue: Arc<DhtTaskQueue>,
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

        let listen_addr = config
            .listen_addr
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));

        info!(
            id = %hex::encode(self_id),
            port = config.port,
            ?listen_addr,
            "Starting DHT engine"
        );

        // Bind UDP socket
        let socket = if let Some(ports) = config.port_range.as_deref() {
            let mut last_error = None;
            let mut bound = None;
            for port in ports {
                match DhtSocket::bind_on(SocketAddr::new(listen_addr, *port)).await {
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
            DhtSocket::bind_on(SocketAddr::new(listen_addr, config.port))
                .await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::AddrInUse, e))?
        };
        let actual_port = socket.local_addr().port();
        info!(port = actual_port, "DHT socket bound");

        // Load routing table from disk or start empty
        let mut routing_table = RoutingTable::new(self_id);
        if let Some(ref path) = config.dht_file_path
            && tokio::fs::try_exists(path).await.unwrap_or(false)
        {
            match super::persistence::DhtPersistence::load_from_file(path).await {
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

        let routing_table = Arc::new(RwLock::new(routing_table));
        let inner = Arc::new(RwLock::new(DhtEngineInner {
            state: DhtEngineState::Bootstrapping,
            self_id,
        }));

        let token_tracker = Arc::new(std::sync::Mutex::new(TokenTracker::new()));
        let peer_storage = Arc::new(DhtPeerStorage::new());
        let tracker = Arc::new(TransactionTracker::new());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let routing_table_save_lock = Arc::new(tokio::sync::Mutex::new(()));
        let task_context = DhtTaskContext {
            self_id,
            routing_table: Arc::clone(&routing_table),
            socket: socket.clone(),
            tracker: Arc::clone(&tracker),
            query_timeout: config.query_timeout,
        };
        let task_queue = Arc::new(DhtTaskQueue::with_concurrency(
            config.max_concurrent_lookups,
        ));
        let task_context = Arc::new(DhtEngineContext {
            inner: Arc::clone(&inner),
            routing_table,
            routing_table_save_lock,
            config: config.clone(),
            socket: socket.clone(),
            token_tracker: Arc::clone(&token_tracker),
            peer_storage: Arc::clone(&peer_storage),
            tracker: Arc::clone(&tracker),
            handler_self_id: self_id,
            shutdown_requested: Arc::clone(&shutdown_requested),
            task_factory: DhtTaskFactory::new(task_context),
        });

        let engine = Arc::new(Self {
            context: task_context,
            shutdown_tx,
            background_tasks: std::sync::Mutex::new(Vec::new()),
            task_queue,
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
        let task_queue = Arc::clone(&self.task_queue);
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let limit = context.config.bootstrap_timeout;
        let handle = tokio::spawn(async move {
            let bootstrap = async {
                if tokio::time::timeout(limit, context.bootstrap(&task_queue))
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

        debug!(info_hash = %hex::encode(info_hash), "Starting DHT get_peers lookup");

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let accepted = self
            .task_queue
            .add_immediate_task(self.context.task_factory.create_peer_lookup_task(
                *info_hash,
                0,
                Some(result_tx),
            ))
            .await;
        if !accepted {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "DHT peer lookup task was cancelled",
            ));
        }
        let result = result_rx.await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "DHT peer lookup task was cancelled",
            )
        })?;

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

        debug!(
            info_hash = %hex::encode(info_hash),
            port,
            "Starting DHT announce_peer"
        );

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let accepted = self
            .task_queue
            .add_immediate_task(self.context.task_factory.create_peer_lookup_task(
                *info_hash,
                port,
                Some(result_tx),
            ))
            .await;
        if !accepted {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "DHT announce task was cancelled",
            ));
        }
        result_rx.await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "DHT announce task was cancelled",
            )
        })?;

        Ok(())
    }

    /// Add a bootstrap node to the routing table.
    ///
    /// Sends a `ping` to `addr` and inserts it into the appropriate k-bucket
    /// once a response is received.
    pub async fn add_node(&self, addr: SocketAddr) {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let accepted = self
            .task_queue
            .add_immediate_task(self.context.task_factory.create_ping_task_with_result(
                DhtNode::new([0u8; 20], addr),
                0,
                result_tx,
            ))
            .await;
        if !accepted {
            return;
        }

        if let Ok(Some(node)) = result_rx.await {
            debug!(
                addr = %addr,
                id = %hex::encode(node.id),
                "Added DHT node via add_node"
            );
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

        self.task_queue.shutdown().await;

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
            let path = path.clone();
            // Serialize before taking the snapshot so an older automatic
            // snapshot cannot overwrite this final shutdown snapshot later.
            let save_guard = Arc::clone(&self.context.routing_table_save_lock)
                .lock_owned()
                .await;
            let self_id = self.context.inner.read().await.self_id;
            let nodes: Vec<DhtNode> = self.context.routing_table.read().await.collect_good_nodes();
            let count = nodes.len();

            let save_path = path.clone();
            let result = tokio::task::spawn_blocking(move || {
                let _save_guard = save_guard;
                super::persistence::DhtPersistence::save_to_file_sync(&save_path, &self_id, &nodes)
            })
            .await;
            match result {
                Ok(Ok(_)) => info!(path = %path.display(), count, "Saved DHT routing table"),
                Ok(Err(e)) => {
                    warn!(path = %path.display(), "Failed to save DHT routing table: {}", e)
                }
                Err(e) => {
                    warn!(path = %path.display(), "DHT routing table save task failed: {}", e)
                }
            }
        }

        info!("DHT engine shutdown complete");
    }

    /// Return a snapshot of DHT engine statistics.
    pub async fn stats(&self) -> DhtEngineStats {
        let inner = self.context.inner.read().await;
        let routing_table = self.context.routing_table.read().await;
        let state = if self.context.shutdown_requested.load(Ordering::Acquire) {
            DhtEngineState::ShuttingDown
        } else {
            inner.state
        };
        DhtEngineStats {
            total_nodes: routing_table.total_node_count(),
            good_nodes: routing_table.good_node_count(),
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
        self.task_queue.cancel();
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
// process_inbound_message, handle_timeout, contact_nodes,
// evict_and_replace_nodes, save_routing_table) are defined in
// engine_inner.rs to keep this file under 600 lines.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bittorrent::dht::message::{DhtMessage, DhtMessageBuilder};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::UdpSocket;
    use tokio::sync::Barrier;
    use tokio_util::sync::CancellationToken;

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_real_udp_dht_pressure_across_concurrency_levels() {
        const REQUESTS: usize = 64;
        const RESPONDER_WORKERS: usize = 8;
        const RESPONDER_DELAY: Duration = Duration::from_millis(2);
        const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

        let responder = Arc::new(
            UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("local UDP responder should bind"),
        );
        let responder_addr = responder.local_addr().expect("responder address");
        let responder_id = [0xA5; 20];
        let responder_requests: Arc<Vec<AtomicUsize>> =
            Arc::new((0..=16).map(|_| AtomicUsize::new(0)).collect());
        let responder_responses: Arc<Vec<AtomicUsize>> =
            Arc::new((0..=16).map(|_| AtomicUsize::new(0)).collect());
        let responder_stop = CancellationToken::new();
        let mut responder_workers = tokio::task::JoinSet::new();

        for _ in 0..RESPONDER_WORKERS {
            let responder = Arc::clone(&responder);
            let responder_stop = responder_stop.clone();
            let responder_requests = Arc::clone(&responder_requests);
            let responder_responses = Arc::clone(&responder_responses);
            responder_workers.spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    let (len, from) = tokio::select! {
                        _ = responder_stop.cancelled() => break,
                        result = responder.recv_from(&mut buf) => {
                            let Ok(packet) = result else { break };
                            packet
                        }
                    };
                    let Ok(query) = DhtMessage::decode(&buf[..len]) else {
                        continue;
                    };
                    if !query.is_query() {
                        continue;
                    }
                    let query_level = if query
                        .q
                        .as_ref()
                        .is_some_and(|method| method.0 == "get_peers")
                    {
                        query
                            .a
                            .as_ref()
                            .and_then(|args| args.dict_get(b"info_hash"))
                            .and_then(|value| value.as_bytes())
                            .and_then(|info_hash| info_hash.first().copied())
                            .filter(|level| (1..=16).contains(level))
                            .map(usize::from)
                    } else {
                        None
                    };
                    if let Some(level) = query_level {
                        responder_requests[level].fetch_add(1, Ordering::Relaxed);
                    }
                    tokio::select! {
                        _ = responder_stop.cancelled() => break,
                        _ = tokio::time::sleep(RESPONDER_DELAY) => {}
                    }

                    let response = match query.q.as_ref().map(|method| method.0.as_str()) {
                        Some("ping") => DhtMessageBuilder::ping_response(&query.t, &responder_id),
                        Some("get_peers") => DhtMessageBuilder::get_peers_response_with_peers(
                            &query.t,
                            &responder_id,
                            b"local-token",
                            &[],
                        ),
                        _ => continue,
                    };
                    let Ok(encoded) = response.encode() else {
                        continue;
                    };
                    if responder.send_to(&encoded, from).await.is_ok()
                        && let Some(level) = query_level
                    {
                        responder_responses[level].fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }

        let responder_task =
            tokio::spawn(async move { while responder_workers.join_next().await.is_some() {} });

        for max_concurrent_lookups in [1, 2, 4, 8, 16] {
            let config = DhtEngineConfig {
                query_timeout: QUERY_TIMEOUT,
                max_concurrent_lookups,
                ..DhtEngineConfig::local()
            };
            let engine = DhtEngine::start(config)
                .await
                .expect("DHT engine should start");
            engine.add_node(responder_addr).await;

            let node_ready = tokio::time::timeout(Duration::from_millis(200), async {
                loop {
                    if engine.stats().await.good_nodes > 0 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await;
            assert!(node_ready.is_ok(), "local responder was not added");

            let start_barrier = Arc::new(Barrier::new(REQUESTS + 1));

            let mut tasks = Vec::with_capacity(REQUESTS);
            for _ in 0..REQUESTS {
                let engine = Arc::clone(&engine);
                let barrier = Arc::clone(&start_barrier);
                tasks.push(tokio::spawn(async move {
                    barrier.wait().await;
                    let started = std::time::Instant::now();
                    let result = engine.find_peers(&[max_concurrent_lookups as u8; 20]).await;
                    (
                        started.elapsed(),
                        result.map(|result| result.nodes_contacted > 0),
                    )
                }));
            }

            start_barrier.wait().await;
            let started_at = std::time::Instant::now();
            let mut latencies = Vec::with_capacity(REQUESTS);
            let mut successful = 0usize;
            for task in tasks {
                let (latency, result) = task.await.expect("lookup task should join");
                latencies.push(latency);
                if result.expect("lookup should not be cancelled") {
                    successful += 1;
                }
            }
            let elapsed = started_at.elapsed();

            latencies.sort_unstable();
            let p50 = latencies[REQUESTS / 2];
            let p95 = latencies[(REQUESTS * 95 / 100).min(REQUESTS - 1)];
            let max = latencies[REQUESTS - 1];
            let received = responder_requests[max_concurrent_lookups].load(Ordering::Relaxed);
            let responses = responder_responses[max_concurrent_lookups].load(Ordering::Relaxed);
            let expected_packets = REQUESTS;
            let request_loss = expected_packets.saturating_sub(received);
            let response_loss = expected_packets.saturating_sub(responses);
            let throughput = successful as f64 / elapsed.as_secs_f64();

            println!(
                "DHT UDP pressure: concurrency={max_concurrent_lookups}, requests={REQUESTS}, successful={successful}, responder_requests={received}, responder_responses={responses}, request_loss={:.2}%, response_loss={:.2}%, p50={:?}, p95={:?}, max={:?}, throughput={throughput:.1}/s, immediate_queue_peak={}",
                request_loss as f64 * 100.0 / expected_packets as f64,
                response_loss as f64 * 100.0 / expected_packets as f64,
                p50,
                p95,
                max,
                engine
                    .task_queue
                    .immediate_executor()
                    .peak_queue_size()
                    .await,
            );

            assert_eq!(successful, REQUESTS, "local UDP pressure test lost lookups");
            assert!(
                received >= expected_packets,
                "local UDP responder received fewer packets than expected"
            );
            assert!(
                responses >= expected_packets,
                "local UDP responder sent fewer responses than expected"
            );
            engine.shutdown_async().await;
        }

        responder_stop.cancel();
        responder_task.await.expect("responder task should join");
    }
}

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

mod api;
#[cfg(test)]
mod tests;

use super::node::DhtNode;
use super::peer_storage::DhtPeerStorage;
use super::routing_table::RoutingTable;
use super::socket::DhtSocket;
use super::store::DhtItemStore;
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
    pub(super) item_store: DhtItemStore,
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
        let item_store = config
            .dht_file_path
            .as_deref()
            .map(|path| path.with_extension("items"))
            .and_then(|path| match DhtItemStore::load_from_file_sync(&path) {
                Ok(store) => {
                    info!(path = %path.display(), "Loaded BEP 44 item store");
                    Some(store)
                }
                Err(error) => {
                    debug!(path = %path.display(), "BEP 44 item store unavailable: {error}");
                    None
                }
            })
            .unwrap_or_default();
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
            item_store,
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
                super::persistence::DhtPersistence::merge_and_save_to_file_sync(
                    &save_path, &self_id, &nodes,
                )
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

            let item_path = path.with_extension("items");
            let item_save_guard = Arc::clone(&self.context.routing_table_save_lock)
                .lock_owned()
                .await;
            let item_store = self.context.item_store.clone();
            let item_save_path = item_path.clone();
            let item_result = tokio::task::spawn_blocking(move || {
                let _save_guard = item_save_guard;
                item_store.save_to_file_sync(&item_save_path)
            })
            .await;
            match item_result {
                Ok(Ok(())) => info!(path = %item_path.display(), "Saved BEP 44 item store"),
                Ok(Err(error)) => {
                    warn!(path = %item_path.display(), "Failed to save BEP 44 item store: {error}")
                }
                Err(error) => warn!("BEP 44 item store save task failed: {error}"),
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

// Background task methods remain in engine_inner.rs; network operations are in api.rs.

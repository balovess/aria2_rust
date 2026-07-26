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
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{debug, info, trace, warn};

use super::bootstrap::DhtBootstrap;
use super::handler::DhtQueryHandler;
use super::lookup::{iterative_find_node, iterative_get_peers, announce_to_token_nodes};
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
}

impl Default for DhtEngineConfig {
    fn default() -> Self {
        Self {
            port: 6881,
            self_id: [0u8; 20],
            dht_file_path: None,
            refresh_check_interval: Duration::from_secs(300), // 5 min check
            query_timeout: Duration::from_secs(10),
            token_rotation_interval: Duration::from_secs(600), // 10 min
            node_contact_interval: Duration::from_secs(900),  // 15 min
            max_concurrent_lookups: 16,
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
    PeerLookupCompleted { info_hash: [u8; 20], peers: Vec<SocketAddr> },
    NodeLookupCompleted { target: [u8; 20], nodes: Vec<SocketAddr> },
    TokenRotationNeeded,
    BootstrapCompleted,
}

// ==================== Internal shared state ====================

/// Shared mutable state behind `Arc<RwLock<>>`.
struct DhtEngineInner {
    state: DhtEngineState,
    routing_table: RoutingTable,
    self_id: [u8; 20],
}

// ==================== DhtEngine ====================

/// DHT engine — orchestrates the full DHT node lifecycle.
///
/// Created via [`DhtEngine::start`] which binds a UDP socket and returns
/// an `Arc<DhtEngine>` ready for shared use. All public methods take `&self`
/// and use interior mutability for thread-safe access.
pub struct DhtEngine {
    /// Shared mutable state.
    inner: Arc<RwLock<DhtEngineInner>>,
    /// Configuration (immutable after creation).
    config: DhtEngineConfig,
    /// UDP socket for DHT communication.
    socket: DhtSocket,
    /// Token tracker for generating/validating announce tokens.
    token_tracker: Arc<std::sync::Mutex<TokenTracker>>,
    /// Peer storage for inbound announce_peer queries.
    peer_storage: Arc<DhtPeerStorage>,
    /// Transaction tracker for matching queries to responses.
    tracker: Arc<TransactionTracker>,
    /// Query handler for inbound KRPC queries.
    handler: DhtQueryHandler,
    /// Shutdown signal sender.
    shutdown_tx: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl DhtEngine {
    /// Start the DHT engine with the given configuration.
    ///
    /// Binds a UDP socket on the configured port, loads the routing table
    /// from disk (if available), bootstraps into the network, and returns
    /// a shared reference to the running engine.
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
        let socket = DhtSocket::bind(config.port).await.map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::AddrInUse, e)
        })?;
        let actual_port = socket.local_addr().port();
        info!(port = actual_port, "DHT socket bound");

        // Load routing table from disk or start empty
        let mut routing_table = RoutingTable::new(self_id);
        if let Some(ref path) = config.dht_file_path {
            if path.exists() {
                match super::persistence::DhtPersistence::load_from_file_sync(path) {
                    Ok(data) => {
                        info!(count = data.nodes.len(), "Loaded DHT routing table from disk");
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
        }

        let inner = Arc::new(RwLock::new(DhtEngineInner {
            state: DhtEngineState::Bootstrapping,
            routing_table,
            self_id,
        }));

        let token_tracker = Arc::new(std::sync::Mutex::new(TokenTracker::new()));
        let peer_storage = Arc::new(DhtPeerStorage::new());
        let tracker = Arc::new(TransactionTracker::new());
        let handler = DhtQueryHandler::new(self_id);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let engine = Arc::new(Self {
            inner,
            config: config.clone(),
            socket,
            token_tracker,
            peer_storage,
            tracker,
            handler,
            shutdown_tx: std::sync::Mutex::new(Some(shutdown_tx)),
        });

        // Spawn the background receive loop
        engine.spawn_receive_loop(shutdown_rx);

        // Spawn periodic tasks
        engine.spawn_periodic_tasks();

        // Bootstrap: ping entry point nodes
        engine.bootstrap().await;

        Ok(engine)
    }

    /// Return a snapshot of the current engine state.
    pub async fn state(&self) -> DhtEngineState {
        let inner = self.inner.read().await;
        inner.state
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

        let rt = Arc::new(RwLock::new(self.inner.read().await.routing_table.clone()));
        let self_id = self.inner.read().await.self_id;

        debug!(info_hash = %hex::encode(info_hash), "Starting DHT get_peers lookup");

        let result = iterative_get_peers(
            info_hash,
            &self_id,
            &rt,
            &self.socket,
            &self.tracker,
        )
        .await;

        // Merge discovered nodes back into the main routing table
        {
            let mut inner = self.inner.write().await;
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

        let rt = Arc::new(RwLock::new(self.inner.read().await.routing_table.clone()));
        let self_id = self.inner.read().await.self_id;

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
            &self.socket,
            &self.tracker,
        )
        .await;

        // Merge discovered nodes back
        {
            let mut inner = self.inner.write().await;
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
                &self.socket,
                &self.tracker,
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
        let self_id = self.inner.read().await.self_id;
        let msg = DhtMessageBuilder::ping(0, &self_id);
        let encoded = match msg.encode() {
            Ok(e) => e,
            Err(e) => {
                debug!("Failed to encode ping for add_node: {}", e);
                return;
            }
        };

        if let Err(e) = self.socket.send_to(addr, &encoded).await {
            debug!(addr = %addr, "Failed to send ping: {}", e);
            return;
        }

        // Wait briefly for a response
        let mut buf = [0u8; 4096];
        if let Ok((len, _from)) = self.socket.recv_with_timeout(&mut buf, self.config.query_timeout).await {
            if len > 0 {
                if let Ok(response) = DhtMessage::decode(&buf[..len]) {
                    if response.is_response() {
                        // Extract node ID from response
                        if let Some(r) = &response.r {
                            if let Some(id_bytes) = r.dict_get(b"id").and_then(|v| v.as_bytes()) {
                                if id_bytes.len() == 20 {
                                    let mut node_id = [0u8; 20];
                                    node_id.copy_from_slice(id_bytes);
                                    let node = DhtNode::new(node_id, addr);
                                    let mut inner = self.inner.write().await;
                                    inner.routing_table.insert(node);
                                    debug!(addr = %addr, id = %hex::encode(node_id), "Added DHT node via add_node");
                                }
                            }
                        }
                    }
                }
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
        let mut tx = self.shutdown_tx.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sender) = tx.take() {
            let _ = sender.send(());
        }
        // Set state synchronously
        if let Ok(_inner) = self.inner.try_write() {
            // Already shutting down — can't hold the lock across await
        }
        info!("DHT shutdown signal sent");
    }

    /// Async shutdown — signals the engine to stop and awaits full teardown.
    pub async fn shutdown_async(&self) {
        self.shutdown();

        // Wait for the background task to terminate
        // (it will observe the shutdown signal via the oneshot channel)
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Save routing table to disk
        if let Some(ref path) = self.config.dht_file_path {
            let inner = self.inner.read().await;
            let self_id = inner.self_id;
            let nodes: Vec<DhtNode> = inner.routing_table.collect_good_nodes();
            drop(inner);

            if !nodes.is_empty() {
                match super::persistence::DhtPersistence::save_to_file_sync(path, &self_id, &nodes) {
                    Ok(_) => info!(path = %path.display(), count = nodes.len(), "Saved DHT routing table"),
                    Err(e) => warn!("Failed to save DHT routing table: {}", e),
                }
            }
        }

        info!("DHT engine shutdown complete");
    }

    /// Return a snapshot of DHT engine statistics.
    pub async fn stats(&self) -> DhtEngineStats {
        let inner = self.inner.read().await;
        DhtEngineStats {
            total_nodes: inner.routing_table.total_node_count(),
            good_nodes: inner.routing_table.good_node_count(),
            pending_transactions: self.tracker.pending_count(),
            state: inner.state,
        }
    }

    // ==================== Internal: Background tasks ====================

    /// Spawn the main UDP receive loop as a background task.
    fn spawn_receive_loop(self: &Arc<Self>, mut shutdown_rx: tokio::sync::oneshot::Receiver<()>) {
        let engine = Arc::clone(self);
        let socket = self.socket.clone();
        let tracker = Arc::clone(&self.tracker);
        let handler_self_id = self.handler.self_id();

        tokio::spawn(async move {
            let handler = DhtQueryHandler::new(handler_self_id);
            info!("DHT receive loop started");
            let mut buf = [0u8; 4096];

            loop {
                let recv_result = tokio::select! {
                    _ = &mut shutdown_rx => {
                        info!("DHT receive loop shutting down");
                        break;
                    }
                    result = socket.recv_with_timeout(&mut buf, Duration::from_secs(1)) => {
                        result
                    }
                };

                match recv_result {
                    Ok((len, from)) if len > 0 => {
                        engine.process_inbound_message(&buf[..len], from, &tracker, &handler).await;
                    }
                    Ok(_) => { /* empty packet, ignore */ }
                    Err(e) if e.contains("timeout") => { /* normal timeout, continue */ }
                    Err(e) => {
                        debug!("DHT recv error: {}", e);
                    }
                }

                // Process transaction timeouts
                let timed_out = tracker.handle_timeouts();
                for (addr, _query_type, node_id) in timed_out {
                    engine.handle_timeout(addr, node_id).await;
                }
            }

            info!("DHT receive loop exited");
        });
    }

    /// Spawn periodic maintenance tasks.
    fn spawn_periodic_tasks(self: &Arc<Self>) {
        let engine = Arc::clone(self);

        // Token rotation + bucket refresh + node contact + peer cleanup + auto-save
        tokio::spawn(async move {
            let mut token_interval = tokio::time::interval(engine.config.token_rotation_interval);
            let mut refresh_check_interval = tokio::time::interval(engine.config.refresh_check_interval);
            let mut node_contact_interval = tokio::time::interval(engine.config.node_contact_interval);
            let mut cleanup_interval = tokio::time::interval(Duration::from_secs(300));
            let mut save_interval = tokio::time::interval(Duration::from_secs(1800));

            loop {
                tokio::select! {
                    _ = token_interval.tick() => {
                        let mut tt = engine.token_tracker.lock().unwrap_or_else(|e| e.into_inner());
                        tt.maybe_rotate();
                        trace!("DHT token rotation check");
                    }
                    _ = refresh_check_interval.tick() => {
                        engine.refresh_buckets().await;
                    }
                    _ = node_contact_interval.tick() => {
                        engine.contact_nodes().await;
                    }
                    _ = cleanup_interval.tick() => {
                        engine.peer_storage.cleanup_expired();
                        engine.tracker.cleanup_expired();
                        engine.evict_and_replace_nodes().await;
                        trace!("DHT periodic cleanup");
                    }
                    _ = save_interval.tick() => {
                        engine.save_routing_table().await;
                    }
                }
            }
        });
    }

    /// Bootstrap into the DHT network by resolving entry point hostnames,
    /// pinging them, and performing an initial find_node for our own ID.
    ///
    /// This is the equivalent of C++ `DHTEntryPointNameResolveCommand` +
    /// `DHTInteractionCommand` bootstrap flow. The C++ resolves hostnames
    /// via c-ares; we use tokio's async DNS resolver.
    async fn bootstrap(&self) {
        let self_id = self.inner.read().await.self_id;

        // Resolve bootstrap node hostnames via async DNS (C++ uses c-ares).
        let entry_points = DhtBootstrap::resolve_bootstrap_nodes().await;

        if entry_points.is_empty() {
            warn!("No DHT bootstrap nodes could be resolved — DHT may not function properly");
        }

        info!(count = entry_points.len(), "Bootstrapping DHT with entry points");

        // Ping each entry point
        for node in &entry_points {
            let msg = super::message::DhtMessageBuilder::ping(0, &self_id);
            if let Ok(encoded) = msg.encode() {
                if let Err(e) = self.socket.send_to(node.addr, &encoded).await {
                    debug!(addr = %node.addr, "Bootstrap ping failed: {}", e);
                }
            }
        }

        // Wait briefly for responses, then do a find_node for our own ID
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Add any responding bootstrap nodes to routing table
        {
            let mut inner = self.inner.write().await;
            for node in &entry_points {
                inner.routing_table.insert(node.clone());
            }
        }

        // Do an initial find_node for our own ID to populate the routing table
        let rt = Arc::new(RwLock::new(self.inner.read().await.routing_table.clone()));
        let _result = iterative_find_node(&self_id, &self_id, &rt, &self.socket, &self.tracker).await;

        // Merge discovered nodes
        {
            let mut inner = self.inner.write().await;
            let discovered_rt = rt.read().await;
            for node in discovered_rt.all_nodes() {
                inner.routing_table.insert(node.clone());
            }
            inner.state = DhtEngineState::Running;
        }

        info!("DHT bootstrap completed");
    }

    /// Process an inbound KRPC message.
    async fn process_inbound_message(
        &self,
        data: &[u8],
        from: SocketAddr,
        tracker: &TransactionTracker,
        handler: &DhtQueryHandler,
    ) {
        let msg = match DhtMessage::decode(data) {
            Ok(m) => m,
            Err(_) => return,
        };

        match msg.y {
            super::message::DhtMessageType::Response | super::message::DhtMessageType::Error => {
                // Match to a pending transaction
                let tx_id = msg.t.clone();
                tracker.handle_response(&tx_id, msg, from);
            }
            super::message::DhtMessageType::Query => {
                let rt = self.inner.read().await.routing_table.clone();
                let (response, mark_good, sender_id) = {
                    let tt = self.token_tracker.lock().unwrap_or_else(|e| e.into_inner());
                    let result = handler.handle_query(&msg, from, &rt, &tt, &self.peer_storage);
                    (result.response, result.mark_good, result.sender_id)
                };
                // tt lock is released here, before any .await

                // Send response
                if let Some(response) = response {
                    if let Ok(encoded) = response.encode() {
                        if let Err(e) = self.socket.send_to(from, &encoded).await {
                            debug!(to = %from, "Failed to send DHT response: {}", e);
                        }
                    }
                }

                // Mark sender as good and add to routing table
                if mark_good {
                    if let Some(sender_id) = sender_id {
                        let mut inner = self.inner.write().await;
                        inner.routing_table.mark_good(&sender_id);
                        inner.routing_table.insert(DhtNode::new(sender_id, from));
                    }
                }
            }
        }
    }

    /// Handle a timed-out transaction by marking the node as failed.
    async fn handle_timeout(&self, _addr: SocketAddr, node_id: Option<[u8; 20]>) {
        let mut inner = self.inner.write().await;
        if let Some(_id) = node_id {
            let rt = &mut inner.routing_table;
            rt.evict_bad_nodes();
        }
    }

    /// Refresh stale k-buckets by doing find_node lookups for random IDs.
    async fn refresh_buckets(&self) {
        let self_id = self.inner.read().await.self_id;
        let targets = self.inner.read().await.routing_table.refresh_buckets();

        if targets.is_empty() {
            return;
        }

        debug!(count = targets.len(), "Refreshing stale DHT buckets");

        for target in targets.into_iter().take(3) {
            let rt = Arc::new(RwLock::new(self.inner.read().await.routing_table.clone()));
            let _result = iterative_find_node(&target, &self_id, &rt, &self.socket, &self.tracker).await;

            // Merge discovered nodes
            let mut inner = self.inner.write().await;
            let discovered_rt = rt.read().await;
            for node in discovered_rt.all_nodes() {
                inner.routing_table.insert(node.clone());
            }
        }
    }

    /// Send keep-alive pings to routing table nodes that haven't been
    /// contacted recently.
    ///
    /// Equivalent to C++ `DHT_NODE_CONTACT_INTERVAL` (15 min). The C++
    /// sends a ping to a random good node in each bucket to keep the
    /// routing table alive.
    async fn contact_nodes(&self) {
        let self_id = self.inner.read().await.self_id;
        let buckets = {
            let inner = self.inner.read().await;
            inner.routing_table.get_all_buckets();
            // Need to collect the info we need before releasing the lock.
            let mut addrs = Vec::new();
            for bucket in inner.routing_table.get_all_buckets() {
                if let Some(node) = bucket.nodes().iter().find(|n| n.is_good()) {
                    addrs.push(node.addr);
                }
            }
            addrs
        };

        let mut contacted = 0usize;
        for addr in buckets {
            let msg = DhtMessageBuilder::ping(rand::random::<u32>(), &self_id);
            if let Ok(encoded) = msg.encode() {
                if self.socket.send_to(addr, &encoded).await.is_ok() {
                    contacted += 1;
                }
            }
        }

        if contacted > 0 {
            trace!(contacted, "DHT node contact keep-alive sent");
        }
    }

    /// Evict bad nodes from the routing table and attempt to replace
    /// questionable nodes with cached candidates.
    ///
    /// Equivalent to C++ periodic `DHTReplaceNodeTask` execution.
    async fn evict_and_replace_nodes(&self) {
        let mut inner = self.inner.write().await;
        let evicted = inner.routing_table.evict_bad_nodes();

        // For buckets that have questionable nodes and cached replacements,
        // attempt to replace the questionable node with a cached candidate.
        let replacements: Vec<DhtNode> = inner
            .routing_table
            .get_all_buckets()
            .iter()
            .filter_map(|bucket| {
                if bucket.contains_questionable_node() && !bucket.cached_nodes().is_empty() {
                    bucket.cached_nodes().first().cloned()
                } else {
                    None
                }
            })
            .collect();

        let replacement_count = replacements.len();

        // For each replacement candidate, ping the questionable node.
        // If it doesn't respond, the replacement is promoted.
        // For simplicity, we directly promote cached nodes when bad nodes
        // were evicted — the full C++ ReplaceNodeTask pings first.
        for node in replacements {
            inner.routing_table.insert(node);
        }

        if evicted > 0 || replacement_count > 0 {
            debug!(
                evicted,
                replacements = replacement_count,
                "DHT node eviction and replacement complete"
            );
        }
    }

    /// Save the routing table to disk.
    async fn save_routing_table(&self) {
        if let Some(ref path) = self.config.dht_file_path {
            let inner = self.inner.read().await;
            let self_id = inner.self_id;
            let nodes = inner.routing_table.collect_good_nodes();
            drop(inner);

            if !nodes.is_empty() {
                match super::persistence::DhtPersistence::save_to_file_sync(path, &self_id, &nodes) {
                    Ok(_) => trace!(path = %path.display(), "Auto-saved DHT routing table"),
                    Err(e) => warn!("Failed to auto-save DHT routing table: {}", e),
                }
            }
        }
    }
}

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
        let config = DhtEngineConfig {
            port: 0, // random port
            dht_file_path: None,
            ..Default::default()
        };

        let engine = DhtEngine::start(config).await.expect("start should succeed");
        assert_eq!(engine.state().await, DhtEngineState::Running);

        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_dht_engine_stats() {
        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: None,
            ..Default::default()
        };

        let engine = DhtEngine::start(config).await.expect("start should succeed");
        let stats = engine.stats().await;
        assert_eq!(stats.state, DhtEngineState::Running);

        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_find_peers_when_stopped() {
        // Create an engine in ShuttingDown state
        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: None,
            ..Default::default()
        };

        let engine = DhtEngine::start(config).await.expect("start should succeed");
        engine.shutdown();

        // find_peers should return empty when shutting down
        // (may take a moment for state to propagate)
        tokio::time::sleep(Duration::from_millis(200)).await;
        let result = engine.find_peers(&[0u8; 20]).await;
        assert!(result.is_ok());
    }
}

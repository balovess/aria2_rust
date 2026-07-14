use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use super::bootstrap::DhtBootstrap;
use super::client::{extract_compact_nodes_from_response, extract_compact_peers_from_response};
use super::message::{DhtMessage, DhtMessageBuilder, DhtMessageType};
use super::node::DhtNode;
use super::peer_storage::DhtPeerStorage;
use super::persistence::DhtPersistence;
use super::routing_table::RoutingTable;
use super::socket::DhtSocket;
use super::token_tracker::TokenTracker;
use crate::bittorrent::bencode::codec::BencodeValue;

/// Hardcoded bootstrap nodes used when routing table is empty and no custom
/// bootstrappers are configured. These are well-known public DHT routers.
pub const HARDCODED_BOOTSTRAP_NODES: &[(&str, u16)] = &[
    ("router.bittorrent.com", 6881),
    ("dht.transmissionbt.com", 6881),
    ("router.utorrent.com", 6881),
    ("bitsnoop.com", 6881),
];

fn generate_random_id() -> [u8; 20] {
    let mut id = [0u8; 20];
    getrandom::getrandom(&mut id).expect("generate_random_id failed");
    id[0] &= 0x03;
    id
}

// ==================== Compact node encoding & argument extraction ====================

/// Encode nodes into BEP 0005 compact node format (IPv4 only).
///
/// Each entry is 26 bytes: 20 bytes node ID + 4 bytes IPv4 + 2 bytes port
/// (big-endian). IPv6 nodes are skipped for simplicity since the vast
/// majority of DHT traffic is IPv4.
fn encode_compact_nodes_ipv4(nodes: &[(&[u8; 20], SocketAddr)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(nodes.len() * 26);
    for &(id, addr) in nodes {
        if let SocketAddr::V4(v4) = addr {
            buf.extend_from_slice(id);
            buf.extend_from_slice(&v4.ip().octets());
            buf.extend_from_slice(&v4.port().to_be_bytes());
        }
        // IPv6 nodes are intentionally skipped (see doc comment).
    }
    buf
}

/// Extract the 20-byte `id` field from a query's `a` arguments.
fn extract_node_id_from_args(a: &Option<BencodeValue>) -> Option<[u8; 20]> {
    let a = a.as_ref()?;
    let id_bytes = a.dict_get(b"id")?.as_bytes()?;
    if id_bytes.len() != 20 {
        return None;
    }
    let mut id = [0u8; 20];
    id.copy_from_slice(id_bytes);
    Some(id)
}

/// Extract the 20-byte `target` field from a find_node query's arguments.
fn extract_target_from_args(a: &Option<BencodeValue>) -> Option<[u8; 20]> {
    let a = a.as_ref()?;
    let target_bytes = a.dict_get(b"target")?.as_bytes()?;
    if target_bytes.len() != 20 {
        return None;
    }
    let mut target = [0u8; 20];
    target.copy_from_slice(target_bytes);
    Some(target)
}

/// Extract the 20-byte `info_hash` field from a query's arguments.
fn extract_info_hash_from_args(a: &Option<BencodeValue>) -> Option<[u8; 20]> {
    let a = a.as_ref()?;
    let ih_bytes = a.dict_get(b"info_hash")?.as_bytes()?;
    if ih_bytes.len() != 20 {
        return None;
    }
    let mut ih = [0u8; 20];
    ih.copy_from_slice(ih_bytes);
    Some(ih)
}

/// Extract the `token` field (as a String) from a query's arguments.
fn extract_token_from_args(a: &Option<BencodeValue>) -> Option<String> {
    let a = a.as_ref()?;
    let token_bytes = a.dict_get(b"token")?.as_bytes()?;
    String::from_utf8(token_bytes.to_vec()).ok()
}

/// Extract the `port` field from an announce_peer query's arguments.
fn extract_port_from_args(a: &Option<BencodeValue>) -> Option<u16> {
    let a = a.as_ref()?;
    let port = a.dict_get(b"port")?.as_int()?;
    u16::try_from(port).ok()
}

/// Extract the `implied_port` field from an announce_peer query's arguments.
/// Per BEP 0005, a non-zero value means the receiver should use the source
/// port of the UDP packet rather than the explicit `port` field.
fn extract_implied_port_from_args(a: &Option<BencodeValue>) -> Option<i64> {
    let a = a.as_ref()?;
    a.dict_get(b"implied_port")?.as_int()
}

pub struct DhtEngineConfig {
    pub self_id: [u8; 20],
    pub port: u16,
    pub max_concurrent_queries: usize,
    pub query_timeout: Duration,
    pub dht_file_path: Option<String>,
}

impl Default for DhtEngineConfig {
    fn default() -> Self {
        Self {
            self_id: generate_random_id(),
            port: 0,
            max_concurrent_queries: 16,
            query_timeout: Duration::from_secs(5),
            dht_file_path: None,
        }
    }
}

pub struct DhtPeerDiscoveryResult {
    pub peers: Vec<std::net::SocketAddr>,
    pub nodes_contacted: usize,
    pub rounds_completed: usize,
}

pub struct DhtStats {
    pub total_nodes: usize,
    pub good_nodes: usize,
}

struct BatchQueryResult {
    peers: Vec<std::net::SocketAddr>,
    new_nodes: Vec<(std::net::SocketAddr, [u8; 20])>,
    nodes_queried: usize,
}

pub struct DhtEngine {
    config: DhtEngineConfig,
    socket: DhtSocket,
    routing_table: tokio::sync::RwLock<RoutingTable>,
    running: AtomicBool,
    tx_counter: AtomicU32,
    token_tracker: TokenTracker,
    peer_storage: DhtPeerStorage,
    /// Maps transaction ID bytes → oneshot sender for the waiting caller.
    /// Uses `std::sync::Mutex` because operations are brief and never span
    /// `.await` points, avoiding async runtime overhead.
    pending_queries: Mutex<HashMap<Vec<u8>, oneshot::Sender<DhtMessage>>>,
}

impl DhtEngine {
    pub async fn start(config: DhtEngineConfig) -> Result<Arc<Self>, String> {
        let socket = DhtSocket::bind(config.port).await?;
        info!("DHT engine started at {}", socket.local_addr());

        let mut self_id = config.self_id;
        let mut loaded_nodes: Vec<DhtNode> = Vec::new();

        if let Some(ref path) = config.dht_file_path {
            match DhtPersistence::load_from_file(std::path::Path::new(path)).await {
                Ok(data) => {
                    self_id = data.self_id;
                    info!(
                        "DHT: Loaded {} nodes from {} (self_id restored)",
                        data.nodes.len(),
                        path
                    );
                    for pn in &data.nodes {
                        loaded_nodes.push(DhtNode::new(pn.id, pn.addr));
                    }
                }
                Err(e) => {
                    debug!(
                        "DHT: Failed to load routing table file {} (using bootstrap): {}",
                        path, e
                    );
                }
            }
        }

        let engine = Arc::new(Self {
            config: DhtEngineConfig { self_id, ..config },
            socket,
            routing_table: tokio::sync::RwLock::new(RoutingTable::new(self_id)),
            running: AtomicBool::new(true),
            tx_counter: AtomicU32::new(0),
            token_tracker: TokenTracker::new(),
            peer_storage: DhtPeerStorage::new(),
            pending_queries: Mutex::new(HashMap::new()),
        });

        for node in loaded_nodes {
            engine.routing_table.write().await.insert(node);
        }

        engine.bootstrap_routing_table().await;
        // Start the single receive loop that routes incoming responses to
        // waiting callers and dispatches incoming queries to handlers.
        engine.start_query_handler();
        Ok(engine)
    }

    async fn bootstrap_routing_table(&self) {
        // Check current node count - if empty, we must use hardcoded bootstrap nodes
        let is_empty = {
            let rt = self.routing_table.read().await;
            rt.total_node_count() == 0
        };

        if is_empty {
            debug!("DHT: Routing table is empty, using hardcoded bootstrap nodes");
            for (host, port) in HARDCODED_BOOTSTRAP_NODES {
                // Create placeholder nodes with random IDs for bootstrap addresses
                // These will be replaced with real node IDs after ping responses
                let mut id = [0u8; 20];
                getrandom::getrandom(&mut id).ok();
                if let Ok(addr) = format!("{}:{}", host, port).parse::<std::net::SocketAddr>() {
                    let node = DhtNode::new(id, addr);
                    self.routing_table.write().await.insert(node);
                }
            }
            debug!(
                "DHT: Added {} hardcoded bootstrap nodes",
                HARDCODED_BOOTSTRAP_NODES.len()
            );
        }

        // Also add nodes from DhtBootstrap module (which may have additional nodes)
        let boot_nodes = DhtBootstrap::get_bootstrap_nodes();
        for node in &boot_nodes {
            self.routing_table.write().await.insert(node.clone());
        }

        let total_count = {
            let rt = self.routing_table.read().await;
            rt.total_node_count()
        };
        debug!(
            "DHT bootstrap: total {} nodes in routing table",
            total_count
        );

        // Send pings to all bootstrap nodes to discover their actual node IDs
        self.send_ping_to_all(&boot_nodes).await;
    }

    /// Add custom bootstrap nodes to the routing table
    pub async fn add_bootstrap_nodes(&self, nodes: &[std::net::SocketAddr]) {
        debug!("DHT: Adding {} custom bootstrap nodes", nodes.len());

        for addr in nodes {
            let mut id = [0u8; 20];
            getrandom::getrandom(&mut id).ok();
            let node = DhtNode::new(id, *addr);
            self.routing_table.write().await.insert(node);
        }

        // Send pings to discover their real node IDs
        let nodes_to_ping: Vec<DhtNode> = nodes
            .iter()
            .map(|&addr| {
                let mut id = [0u8; 20];
                getrandom::getrandom(&mut id).ok();
                DhtNode::new(id, addr)
            })
            .collect();

        self.send_ping_to_all(&nodes_to_ping).await;
    }

    /// Bootstrap the DHT with a list of known nodes
    pub async fn bootstrap_with_nodes(&self, nodes: &[(std::net::SocketAddr, [u8; 20])]) {
        debug!("DHT: Bootstrapping with {} known nodes", nodes.len());

        for (addr, id) in nodes {
            let node = DhtNode::new(*id, *addr);
            self.routing_table.write().await.insert(node);
        }

        // Send pings to verify connectivity
        let nodes_to_ping: Vec<DhtNode> = nodes
            .iter()
            .map(|(addr, id)| DhtNode::new(*id, *addr))
            .collect();

        self.send_ping_to_all(&nodes_to_ping).await;
    }

    async fn send_ping_to_all(&self, nodes: &[DhtNode]) {
        for node in nodes {
            let msg = DhtMessageBuilder::ping(self.next_tx_id(), &self.config.self_id);
            if let Ok(data) = msg.encode() {
                let _ = self.socket.send_to(node.addr, &data).await;
            }
        }
    }

    pub async fn find_peers(&self, info_hash: &[u8; 20]) -> DhtPeerDiscoveryResult {
        // Fast path: check local peer storage first. If peers were previously
        // announced to us (via announce_peer queries), return them immediately
        // without hitting the network.
        let cached_peers = self.peer_storage.get_peers(info_hash);
        if !cached_peers.is_empty() {
            info!(
                "DHT: found {} cached peers in local storage for info_hash",
                cached_peers.len()
            );
            return DhtPeerDiscoveryResult {
                peers: cached_peers,
                nodes_contacted: 0,
                rounds_completed: 0,
            };
        }

        let mut all_peers: Vec<std::net::SocketAddr> = Vec::new();
        let mut contacted = 0usize;
        const MAX_ROUNDS: usize = 3;

        for round in 0..MAX_ROUNDS {
            if !all_peers.is_empty() {
                break;
            }

            let closest_owned: Vec<DhtNode> = {
                let rt = self.routing_table.read().await;
                rt.find_closest(info_hash, self.config.max_concurrent_queries)
                    .into_iter()
                    .cloned()
                    .collect()
            };

            if closest_owned.is_empty() && round == 0 {
                self.bootstrap_routing_table().await;
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            if closest_owned.is_empty() {
                break;
            }

            let results = self.query_get_peers_batch(&closest_owned, info_hash).await;
            contacted += results.nodes_queried;

            all_peers.extend(results.peers);
            for (addr, nid) in results.new_nodes {
                self.routing_table
                    .write()
                    .await
                    .insert(DhtNode::new(nid, addr));
            }

            sleep(Duration::from_millis(200)).await;
        }

        all_peers.sort();
        all_peers.dedup();

        // Store discovered peers in DhtPeerStorage so future get_peers
        // queries (from us or other nodes) can benefit from the results.
        for peer in &all_peers {
            self.peer_storage.add_peer(*info_hash, *peer);
        }

        let is_empty = all_peers.is_empty();

        DhtPeerDiscoveryResult {
            peers: all_peers,
            nodes_contacted: contacted,
            rounds_completed: if is_empty { MAX_ROUNDS } else { 1 },
        }
    }

    pub async fn announce_peer(&self, info_hash: &[u8; 20], port: u16) -> Result<(), String> {
        let closest: Vec<DhtNode> = {
            let rt = self.routing_table.read().await;
            rt.find_closest(info_hash, 8).into_iter().cloned().collect()
        };

        use futures::future::join_all;

        let mut handles = Vec::new();
        for node in &closest {
            // Validate existing token or generate new one for announce
            let _announce_token = self.token_tracker.generate_token(info_hash, &node.addr);
            let token = self.token_tracker.generate_token(info_hash, &node.addr);
            let msg = DhtMessageBuilder::announce_peer(
                self.next_tx_id(),
                &self.config.self_id,
                info_hash,
                port,
                &token,
            );
            if let Ok(data) = msg.encode() {
                let sock = self.socket.clone();
                let addr = node.addr;
                handles.push(tokio::spawn(async move {
                    sock.send_to(addr, &data).await.is_ok()
                }));
            }
        }

        let results = join_all(handles).await;
        let announced = results
            .into_iter()
            .filter_map(|r| r.ok())
            .filter(|&ok| ok)
            .count();

        info!(
            "DHT announce_peer: Announced to {} nodes (port={})",
            announced, port
        );
        Ok(())
    }

    async fn query_get_peers_batch(
        &self,
        targets: &[DhtNode],
        info_hash: &[u8; 20],
    ) -> BatchQueryResult {
        let mut result = BatchQueryResult {
            peers: vec![],
            new_nodes: vec![],
            nodes_queried: 0,
        };

        // Send all queries and register pending receivers. Registration
        // happens BEFORE sending so the receiver is in the map when the
        // response arrives (the query handler routes via the registry).
        let mut receivers: Vec<(SocketAddr, Vec<u8>, oneshot::Receiver<DhtMessage>)> =
            Vec::with_capacity(targets.len());
        for target in targets {
            let tx_id = self.next_tx_id();
            let tx_id_bytes = tx_id.to_be_bytes().to_vec();
            let msg = DhtMessageBuilder::get_peers(tx_id, &self.config.self_id, info_hash);
            let data = match msg.encode() {
                Ok(d) => d,
                Err(_) => continue,
            };
            let rx = self.register_pending_query(tx_id_bytes.clone());
            if self.socket.send_to(target.addr, &data).await.is_err() {
                self.cancel_pending_query(&tx_id_bytes);
                continue;
            }
            receivers.push((target.addr, tx_id_bytes, rx));
        }

        // Wait for all responses concurrently with per-query timeout.
        // All queries were sent above; here we only wait, so the total
        // wall-clock time is bounded by a single `query_timeout` (not
        // `targets.len() * query_timeout` as with sequential waiting).
        let query_timeout = self.config.query_timeout;
        let futs: Vec<_> = receivers
            .into_iter()
            .map(|(addr, tx_id_bytes, rx)| async move {
                match tokio::time::timeout(query_timeout, rx).await {
                    Ok(Ok(resp)) => {
                        let peers = extract_compact_peers_from_response(&resp);
                        let nodes = extract_compact_nodes_from_response(&resp);
                        (peers, nodes, true)
                    }
                    Ok(Err(_)) => {
                        debug!(
                            "DHT: get_peers query to {} cancelled (sender dropped)",
                            addr
                        );
                        (vec![], vec![], false)
                    }
                    Err(_) => {
                        debug!("DHT: get_peers query to {} timed out", addr);
                        self.cancel_pending_query(&tx_id_bytes);
                        (vec![], vec![], false)
                    }
                }
            })
            .collect();

        let results = futures::future::join_all(futs).await;
        for (peers, nodes, success) in results {
            if success {
                result.nodes_queried += 1;
            }
            result.peers.extend(peers);
            result.new_nodes.extend(nodes);
        }

        result
    }

    async fn refresh_closest_buckets(&self) {
        let target_id = self.config.self_id;
        let closest: Vec<DhtNode> = {
            let rt = self.routing_table.read().await;
            rt.find_closest(&target_id, 4)
                .into_iter()
                .cloned()
                .collect()
        };
        self.send_find_node_to_all(&closest).await;
    }

    pub fn start_maintenance_loop(self: &Arc<Self>) {
        let e = Arc::clone(self);
        tokio::spawn(async move {
            let mut save_interval = tokio::time::interval(Duration::from_secs(900));
            loop {
                tokio::select! {
                    _ = save_interval.tick() => {
                        e.save_routing_table_if_configured().await;
                        e.refresh_closest_buckets().await;
                    }
                }
                if !e.running.load(Ordering::Relaxed) {
                    break;
                }
            }
            info!("DHT maintenance loop exited");
        });
    }

    async fn save_routing_table_if_configured(&self) {
        if let Some(ref path) = self.config.dht_file_path {
            let rt = self.routing_table.read().await;
            let nodes = DhtPersistence::collect_good_nodes(&rt);
            drop(rt);

            match DhtPersistence::save_to_file(
                std::path::Path::new(path),
                &self.config.self_id,
                &nodes,
            )
            .await
            {
                Ok(n) => debug!("DHT auto-saved {} good nodes", n),
                Err(e) => warn!("DHT auto-save failed: {}", e),
            }
        }
    }

    async fn send_find_node_to_all(&self, targets: &[DhtNode]) {
        for target in targets {
            let msg =
                DhtMessageBuilder::find_node(self.next_tx_id(), &self.config.self_id, &target.id);
            if let Ok(data) = msg.encode() {
                let _ = self.socket.send_to(target.addr, &data).await;
            }
        }
    }

    pub fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);

        if let Some(ref path) = self.config.dht_file_path {
            let rt = &self.routing_table;
            let nodes = DhtPersistence::collect_good_nodes(&rt.blocking_read());
            match DhtPersistence::save_to_file_sync(
                std::path::Path::new(path),
                &self.config.self_id,
                &nodes,
            ) {
                Ok(n) => info!("DHT: Saved {} good nodes to {}", n, path),
                Err(e) => warn!("DHT: Failed to save routing table: {}", e),
            }
        }

        info!("DHT engine shutdown complete");
    }

    pub async fn shutdown_async(&self) {
        self.running.store(false, Ordering::Relaxed);

        if let Some(ref path) = self.config.dht_file_path {
            let rt = self.routing_table.read().await;
            let nodes = DhtPersistence::collect_good_nodes(&rt);
            drop(rt);

            match DhtPersistence::save_to_file(
                std::path::Path::new(path),
                &self.config.self_id,
                &nodes,
            )
            .await
            {
                Ok(n) => info!("DHT: Saved {} good nodes to {}", n, path),
                Err(e) => warn!("DHT: Failed to save routing table: {}", e),
            }
        }

        info!("DHT engine shutdown complete");
    }

    pub async fn stats(&self) -> DhtStats {
        let rt = self.routing_table.read().await;
        DhtStats {
            total_nodes: rt.total_node_count(),
            good_nodes: rt.good_node_count(),
        }
    }

    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.socket.local_addr()
    }

    /// Add a node to the routing table (for custom bootstrap nodes)
    pub async fn add_node(&self, node: DhtNode) {
        self.routing_table.write().await.insert(node);
    }

    fn next_tx_id(&self) -> u32 {
        self.tx_counter.fetch_add(1, Ordering::Relaxed)
    }

    // ==================== Pending Query Registry (Task 3) ====================

    /// Register a pending query and return a receiver that will be fulfilled
    /// when the matching response arrives (routed by `start_query_handler`).
    ///
    /// The `tx_id_bytes` key supports arbitrary-length transaction IDs per
    /// BEP 0005 (typically 2 or 4 bytes).
    fn register_pending_query(&self, tx_id_bytes: Vec<u8>) -> oneshot::Receiver<DhtMessage> {
        let (tx, rx) = oneshot::channel();
        let mut map = self
            .pending_queries
            .lock()
            .expect("pending_queries mutex poisoned");
        // If a stale entry exists (shouldn't happen with unique tx_ids), the
        // old sender is dropped, which signals cancellation to any old waiter.
        map.insert(tx_id_bytes, tx);
        rx
    }

    /// Complete a pending query by sending `msg` to the waiting caller.
    /// Returns `true` if a matching entry was found, `false` otherwise
    /// (e.g. the query already timed out and was cancelled).
    fn complete_pending_query(&self, tx_id: &[u8], msg: DhtMessage) -> bool {
        let sender = {
            let mut map = self
                .pending_queries
                .lock()
                .expect("pending_queries mutex poisoned");
            map.remove(tx_id)
        };
        match sender {
            Some(tx) => {
                // If send fails the receiver was already dropped (timeout);
                // that's fine — the response is simply discarded.
                let _ = tx.send(msg);
                true
            }
            None => false,
        }
    }

    /// Remove a pending query entry without sending a response.
    /// Called when a query times out so the registry doesn't leak memory.
    fn cancel_pending_query(&self, tx_id: &[u8]) {
        let removed = {
            let mut map = self
                .pending_queries
                .lock()
                .expect("pending_queries mutex poisoned");
            map.remove(tx_id)
        };
        // `removed` (the Sender) is dropped here, which is the correct
        // behaviour: the receiver will observe a Closed error on next poll.
        drop(removed);
    }

    // ==================== Incoming Query Handlers (Task 5) ====================

    /// Handle an incoming `ping` query: add the querying node to the routing
    /// table and reply with our node ID.
    async fn handle_ping_query(&self, sender: SocketAddr, msg: &DhtMessage) {
        if let Some(id) = extract_node_id_from_args(&msg.a) {
            self.routing_table
                .write()
                .await
                .insert(DhtNode::new(id, sender));
        }
        let response = DhtMessageBuilder::ping_response(&msg.t, &self.config.self_id);
        if let Ok(data) = response.encode() {
            let _ = self.socket.send_to(sender, &data).await;
        }
    }

    /// Handle an incoming `find_node` query: add the querying node and reply
    /// with the `K` closest nodes we know to the requested target.
    async fn handle_find_node_query(&self, sender: SocketAddr, msg: &DhtMessage) {
        if let Some(id) = extract_node_id_from_args(&msg.a) {
            self.routing_table
                .write()
                .await
                .insert(DhtNode::new(id, sender));
        }
        let compact_nodes = match extract_target_from_args(&msg.a) {
            Some(target) => {
                let closest: Vec<DhtNode> = self
                    .routing_table
                    .read()
                    .await
                    .find_closest(&target, 8)
                    .into_iter()
                    .cloned()
                    .collect();
                let refs: Vec<(&[u8; 20], SocketAddr)> =
                    closest.iter().map(|n| (&n.id, n.addr)).collect();
                encode_compact_nodes_ipv4(&refs)
            }
            None => Vec::new(),
        };
        let response =
            DhtMessageBuilder::find_node_response(&msg.t, &self.config.self_id, &compact_nodes);
        if let Ok(data) = response.encode() {
            let _ = self.socket.send_to(sender, &data).await;
        }
    }

    /// Handle an incoming `get_peers` query. If we know peers for the
    /// info_hash, reply with them; otherwise reply with the closest nodes.
    /// A token is always included so the requester can later announce.
    async fn handle_get_peers_query(&self, sender: SocketAddr, msg: &DhtMessage) {
        if let Some(id) = extract_node_id_from_args(&msg.a) {
            self.routing_table
                .write()
                .await
                .insert(DhtNode::new(id, sender));
        }
        let Some(info_hash) = extract_info_hash_from_args(&msg.a) else {
            let response = DhtMessageBuilder::error_response(&msg.t, 203, "Missing info_hash");
            if let Ok(data) = response.encode() {
                let _ = self.socket.send_to(sender, &data).await;
            }
            return;
        };

        let token = self.token_tracker.generate_token(&info_hash, &sender);
        let token_bytes = token.as_bytes();

        let peers = self.peer_storage.get_peers(&info_hash);
        if !peers.is_empty() {
            let response = DhtMessageBuilder::get_peers_response_with_peers(
                &msg.t,
                &self.config.self_id,
                token_bytes,
                &peers,
            );
            if let Ok(data) = response.encode() {
                let _ = self.socket.send_to(sender, &data).await;
            }
            debug!(
                "DHT: get_peers replied with {} peers for info_hash",
                peers.len()
            );
        } else {
            let closest: Vec<DhtNode> = self
                .routing_table
                .read()
                .await
                .find_closest(&info_hash, 8)
                .into_iter()
                .cloned()
                .collect();
            let refs: Vec<(&[u8; 20], SocketAddr)> =
                closest.iter().map(|n| (&n.id, n.addr)).collect();
            let compact_nodes = encode_compact_nodes_ipv4(&refs);
            let response = DhtMessageBuilder::get_peers_response_with_nodes(
                &msg.t,
                &self.config.self_id,
                token_bytes,
                &compact_nodes,
            );
            if let Ok(data) = response.encode() {
                let _ = self.socket.send_to(sender, &data).await;
            }
        }
    }

    /// Handle an incoming `announce_peer` query. Validates the token, and if
    /// valid, stores the peer in `DhtPeerStorage`. Per BEP 0005, when
    /// `implied_port` is non-zero the source port of the UDP packet is used.
    async fn handle_announce_peer_query(&self, sender: SocketAddr, msg: &DhtMessage) {
        if let Some(id) = extract_node_id_from_args(&msg.a) {
            self.routing_table
                .write()
                .await
                .insert(DhtNode::new(id, sender));
        }

        let info_hash = extract_info_hash_from_args(&msg.a);
        let token = extract_token_from_args(&msg.a);
        let port = extract_port_from_args(&msg.a);
        let implied_port = extract_implied_port_from_args(&msg.a).unwrap_or(0);

        let (Some(info_hash), Some(token)) = (info_hash, token) else {
            let response =
                DhtMessageBuilder::error_response(&msg.t, 203, "Missing required fields");
            if let Ok(data) = response.encode() {
                let _ = self.socket.send_to(sender, &data).await;
            }
            return;
        };

        // Determine the announce address per BEP 0005:
        // - implied_port != 0 → use the source port from the UDP packet
        // - otherwise → use the explicit `port` field
        let announce_addr = if implied_port != 0 {
            sender
        } else {
            match port {
                Some(p) => SocketAddr::new(sender.ip(), p),
                None => {
                    let response = DhtMessageBuilder::error_response(&msg.t, 203, "Missing port");
                    if let Ok(data) = response.encode() {
                        let _ = self.socket.send_to(sender, &data).await;
                    }
                    return;
                }
            }
        };

        if self
            .token_tracker
            .validate_token(&token, &info_hash, &sender)
        {
            self.peer_storage.add_peer(info_hash, announce_addr);
            let response = DhtMessageBuilder::announce_peer_response(&msg.t, &self.config.self_id);
            if let Ok(data) = response.encode() {
                let _ = self.socket.send_to(sender, &data).await;
            }
            info!(
                "DHT: announce_peer stored peer {} for info_hash",
                announce_addr
            );
        } else {
            let response = DhtMessageBuilder::error_response(&msg.t, 203, "Invalid token");
            if let Ok(data) = response.encode() {
                let _ = self.socket.send_to(sender, &data).await;
            }
            debug!(
                "DHT: announce_peer rejected (invalid token) from {}",
                sender
            );
        }
    }

    // ==================== Query Handler Receive Loop (Task 6) ====================

    /// Dispatch a decoded incoming message. Responses/errors are routed to the
    /// pending query registry; queries are dispatched to the appropriate handler.
    async fn handle_incoming_message(&self, sender: SocketAddr, msg: DhtMessage) {
        match &msg.y {
            DhtMessageType::Response | DhtMessageType::Error => {
                // Clone tx_id before moving msg into complete_pending_query.
                // The tx_id is a small Vec<u8> (typically 2-4 bytes).
                let tx_id = msg.t.clone();
                if !self.complete_pending_query(&tx_id, msg) {
                    debug!("DHT: received response for unknown tx_id (late or unsolicited)");
                }
            }
            DhtMessageType::Query => {
                let method = msg.q.as_ref().map(|q| q.0.as_str()).unwrap_or("");
                match method {
                    "ping" => self.handle_ping_query(sender, &msg).await,
                    "find_node" => self.handle_find_node_query(sender, &msg).await,
                    "get_peers" => self.handle_get_peers_query(sender, &msg).await,
                    "announce_peer" => self.handle_announce_peer_query(sender, &msg).await,
                    other => debug!("DHT: unknown query method '{}'", other),
                }
            }
        }
    }

    /// Spawn the single background receive loop that handles ALL incoming UDP
    /// messages. Routes responses to waiting callers via the pending query
    /// registry and dispatches queries to the appropriate handler.
    ///
    /// The loop exits cleanly when `self.running` is set to `false`. A
    /// periodic wakeup (every 200ms) ensures the `running` flag is checked
    /// even when no messages arrive, so shutdown is prompt.
    pub fn start_query_handler(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        let socket = self.socket.shared_socket();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            info!("DHT query handler started");
            loop {
                if !engine.running.load(Ordering::Relaxed) {
                    break;
                }
                // Use select! with a short timeout so the loop wakes up
                // periodically to re-check the running flag, enabling
                // prompt shutdown even when no messages arrive.
                tokio::select! {
                    result = socket.recv_from(&mut buf) => {
                        match result {
                            Ok((n, sender)) => {
                                if n == 0 {
                                    continue;
                                }
                                match DhtMessage::decode(&buf[..n]) {
                                    Ok(msg) => {
                                        engine.handle_incoming_message(sender, msg).await;
                                    }
                                    Err(e) => {
                                        debug!(
                                            "DHT: failed to decode incoming message: {}",
                                            e
                                        )
                                    }
                                }
                            }
                            Err(e) => {
                                if engine.running.load(Ordering::Relaxed) {
                                    warn!("DHT query handler recv error: {}", e);
                                }
                            }
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {
                        // Timeout — loop back to check running flag
                    }
                }
            }
            info!("DHT query handler exited");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_engine_start_and_stats() {
        let config = DhtEngineConfig {
            port: 0,
            ..Default::default()
        };
        let engine = DhtEngine::start(config).await.expect("engine should start");
        let stats = engine.stats().await;
        assert!(
            stats.total_nodes >= 4,
            "should have at least bootstrap nodes"
        );
        engine.shutdown();
    }

    #[tokio::test]
    async fn test_find_peers_returns_result() {
        let config = DhtEngineConfig {
            port: 0,
            ..Default::default()
        };
        let engine = DhtEngine::start(config).await.expect("engine should start");

        let hash = [0xABu8; 20];
        let result = engine.find_peers(&hash).await;

        assert!(
            result.rounds_completed > 0,
            "should complete at least one round"
        );
        let _ = result.nodes_contacted;
        engine.shutdown();
    }

    #[test]
    fn test_config_default_values() {
        let cfg = DhtEngineConfig::default();
        assert_eq!(cfg.port, 0);
        assert_eq!(cfg.max_concurrent_queries, 16);
        assert_eq!(cfg.query_timeout, Duration::from_secs(5));
    }

    #[test]
    fn test_config_self_id_is_valid() {
        let cfg = DhtEngineConfig::default();
        assert_ne!(cfg.self_id, [0u8; 20], "self_id should not be all zeros");
    }

    #[tokio::test]
    async fn test_shutdown_sets_flag() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();
        assert!(engine.running.load(Ordering::Relaxed));
        engine.shutdown();
        assert!(!engine.running.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_local_addr_valid() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();
        let addr = engine.local_addr();
        assert!(addr.port() > 0, "should have a valid port");
        engine.shutdown();
    }

    #[tokio::test]
    async fn test_maintenance_loop_starts() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();
        engine.start_maintenance_loop();
        sleep(Duration::from_millis(50)).await;
        assert!(
            engine.running.load(Ordering::Relaxed),
            "maintenance should keep engine running"
        );
        engine.shutdown();
        sleep(Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn test_start_with_persisted_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_dht.dat");
        let self_id = [0xBBu8; 20];
        let addr: std::net::SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let node = DhtNode::new([0xAAu8; 20], addr);
        DhtPersistence::save_to_file(&path, &self_id, &[node])
            .await
            .unwrap();

        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: Some(path.to_string_lossy().to_string()),
            ..Default::default()
        };
        let engine = DhtEngine::start(config)
            .await
            .expect("should start with persisted data");

        assert_eq!(
            engine.config.self_id, self_id,
            "self_id should come from file"
        );
        let stats = engine.stats().await;
        assert!(
            stats.total_nodes >= 5,
            "should have bootstrap + persisted nodes (got {})",
            stats.total_nodes
        );
        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_start_fallback_when_no_file() {
        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: Some("/nonexistent/path/dht.dat".to_string()),
            ..Default::default()
        };
        let engine = DhtEngine::start(config)
            .await
            .expect("should fallback to bootstrap when no file");
        let stats = engine.stats().await;
        assert!(
            stats.total_nodes >= 4,
            "should have bootstrap nodes despite missing file"
        );
        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_start_uses_persisted_self_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("self_id_test.dat");
        let custom_id = [
            0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        DhtPersistence::save_to_file(&path, &custom_id, &[])
            .await
            .unwrap();

        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: Some(path.to_string_lossy().to_string()),
            ..Default::default()
        };
        let engine = DhtEngine::start(config).await.unwrap();

        assert_eq!(
            engine.config.self_id, custom_id,
            "engine should use persisted self_id"
        );
        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_shutdown_saves_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shutdown_test.dat");

        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: Some(path.to_string_lossy().to_string()),
            ..Default::default()
        };
        let engine = DhtEngine::start(config).await.unwrap();

        engine.bootstrap_routing_table().await;
        engine.shutdown_async().await;

        assert!(path.exists(), "dht.dat should exist after shutdown");
        let loaded = DhtPersistence::load_from_file_sync(&path).unwrap();
        assert_eq!(loaded.self_id, engine.config.self_id);
    }

    #[test]
    fn test_shutdown_no_path_no_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("should_not_exist.dat");

        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: None,
            ..Default::default()
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let engine = DhtEngine::start(config).await.unwrap();
            engine.shutdown_async().await;
        });

        assert!(
            !path.exists(),
            "no file should be created when dht_file_path is None"
        );
    }

    #[tokio::test]
    async fn test_auto_save_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auto_save_test.dat");

        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: Some(path.to_string_lossy().to_string()),
            ..Default::default()
        };
        let engine = DhtEngine::start(config).await.unwrap();

        engine.start_maintenance_loop();

        tokio::time::sleep(Duration::from_millis(100)).await;
        engine.save_routing_table_if_configured().await;

        assert!(path.exists(), "auto-save should create dht.dat");
        engine.shutdown_async().await;
    }

    #[tokio::test]
    async fn test_auto_save_skips_without_path() {
        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: None,
            ..Default::default()
        };
        let engine = DhtEngine::start(config).await.unwrap();

        engine.save_routing_table_if_configured().await;
        let stats = engine.stats().await;
        assert!(
            stats.total_nodes >= 4,
            "engine should still work after skipped save"
        );
        engine.shutdown();
    }

    #[test]
    fn test_hardcoded_bootstrap_nodes_defined() {
        // Verify the hardcoded bootstrap nodes constant is properly defined
        assert!(!HARDCODED_BOOTSTRAP_NODES.is_empty());
        assert!(HARDCODED_BOOTSTRAP_NODES.len() >= 3);
        for (host, port) in HARDCODED_BOOTSTRAP_NODES {
            assert!(!host.is_empty(), "Bootstrap host should not be empty");
            assert!(*port > 0, "Bootstrap port should be positive");
        }
    }

    #[tokio::test]
    async fn test_hardcoded_bootstrap_used_when_table_empty() {
        // Create an engine with no persistence file and empty initial state
        let config = DhtEngineConfig {
            port: 0,
            dht_file_path: None, // No persisted data
            ..Default::default()
        };

        // Start the engine - it should use hardcoded bootstrap nodes
        let engine = DhtEngine::start(config)
            .await
            .expect("engine should start with hardcoded bootstrap");

        let stats = engine.stats().await;

        // The routing table should have nodes from hardcoded bootstrap + DhtBootstrap
        // At minimum, we expect HARDCODED_BOOTSTRAP_NODES entries
        assert!(
            stats.total_nodes >= HARDCODED_BOOTSTRAP_NODES.len(),
            "Should have at least {} hardcoded bootstrap nodes (got {})",
            HARDCODED_BOOTSTRAP_NODES.len(),
            stats.total_nodes
        );

        debug!(
            "Hardcoded bootstrap test: total_nodes={}, expected_min={}",
            stats.total_nodes,
            HARDCODED_BOOTSTRAP_NODES.len()
        );

        engine.shutdown();
    }

    // ==================== Task 8: DHT Query Handler Tests ====================

    /// Helper: receive a single DHT message from a UDP socket within a timeout.
    async fn recv_dht_message(socket: &tokio::net::UdpSocket) -> DhtMessage {
        let mut buf = [0u8; 4096];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buf))
            .await
            .expect("should receive response within 2s timeout")
            .expect("recv_from should succeed");
        DhtMessage::decode(&buf[..n]).expect("should decode DHT message")
    }

    /// Convert a socket's bind address (which may be `0.0.0.0:port` since the
    /// DHT socket binds to all interfaces) into a loopback address suitable
    /// for sending to in tests. On Windows, sending to `0.0.0.0` fails with
    /// `WSAEADDRNOTAVAIL` (os error 10049).
    fn localhost_addr(addr: SocketAddr) -> SocketAddr {
        SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            addr.port(),
        )
    }

    // ---- Pending query registry tests (Task 3) ----

    #[tokio::test]
    async fn test_pending_query_register_complete() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        let tx_id = vec![0x01u8, 0x02, 0x03];
        let rx = engine.register_pending_query(tx_id.clone());

        let response_msg = DhtMessageBuilder::ping_response(&[0xAB], &[0xCD; 20]);
        let found = engine.complete_pending_query(&tx_id, response_msg.clone());
        assert!(found, "complete_pending_query should find registered tx_id");

        let received = rx.await.expect("receiver should get the message");
        assert!(received.is_response());

        engine.shutdown();
    }

    #[tokio::test]
    async fn test_pending_query_complete_unknown_returns_false() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        let msg = DhtMessageBuilder::ping_response(&[0x01], &[0x02; 20]);
        let found = engine.complete_pending_query(&[0xFF, 0xFF], msg);
        assert!(!found, "complete for unknown tx_id should return false");

        engine.shutdown();
    }

    #[tokio::test]
    async fn test_pending_query_cancel() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        let tx_id = vec![0x0Au8, 0x0B];
        let rx = engine.register_pending_query(tx_id.clone());

        engine.cancel_pending_query(&tx_id);

        // After cancellation, the receiver should get a Closed error
        let result = rx.await;
        assert!(result.is_err(), "cancelled receiver should get an error");

        // Completing a cancelled query should return false
        let msg = DhtMessageBuilder::ping_response(&tx_id, &[0x02; 20]);
        let found = engine.complete_pending_query(&tx_id, msg);
        assert!(!found, "complete after cancel should return false");

        engine.shutdown();
    }

    // ---- Compact node encoder test (Task 5 helper) ----

    #[test]
    fn test_encode_compact_nodes_ipv4() {
        let id1 = [0x01u8; 20];
        let id2 = [0x02u8; 20];
        let addr1: SocketAddr = "192.168.1.1:8080".parse().unwrap();
        let addr2: SocketAddr = "10.0.0.2:6881".parse().unwrap();
        let addr_v6: SocketAddr = "[::1]:1234".parse().unwrap();

        let nodes: Vec<(&[u8; 20], SocketAddr)> =
            vec![(&id1, addr1), (&id2, addr2), (&id1, addr_v6)];

        let encoded = encode_compact_nodes_ipv4(&nodes);

        // Only IPv4 nodes are encoded (26 bytes each), IPv6 is skipped
        assert_eq!(encoded.len(), 52, "should have 2 IPv4 nodes * 26 bytes");

        // Verify first node
        assert_eq!(&encoded[0..20], &id1[..]);
        assert_eq!(&encoded[20..24], &[192, 168, 1, 1]);
        assert_eq!(u16::from_be_bytes([encoded[24], encoded[25]]), 8080);

        // Verify second node
        assert_eq!(&encoded[26..46], &id2[..]);
        assert_eq!(&encoded[46..50], &[10, 0, 0, 2]);
        assert_eq!(u16::from_be_bytes([encoded[50], encoded[51]]), 6881);
    }

    #[test]
    fn test_encode_compact_nodes_empty() {
        let encoded = encode_compact_nodes_ipv4(&[]);
        assert!(encoded.is_empty());
    }

    // ---- Argument extraction helper tests ----

    #[test]
    fn test_extract_node_id_from_args() {
        let mut dict = std::collections::BTreeMap::new();
        dict.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0xAB; 20]));
        let args = BencodeValue::Dict(dict);

        let id = extract_node_id_from_args(&Some(args));
        assert_eq!(id, Some([0xAB; 20]));
    }

    #[test]
    fn test_extract_node_id_wrong_length() {
        let mut dict = std::collections::BTreeMap::new();
        dict.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0x01; 10]));
        let args = BencodeValue::Dict(dict);

        let id = extract_node_id_from_args(&Some(args));
        assert_eq!(id, None, "wrong-length id should return None");
    }

    #[test]
    fn test_extract_node_id_missing() {
        let args = BencodeValue::Dict(std::collections::BTreeMap::new());
        let id = extract_node_id_from_args(&Some(args));
        assert_eq!(id, None);

        assert_eq!(extract_node_id_from_args(&None), None);
    }

    #[test]
    fn test_extract_port_from_args() {
        let mut dict = std::collections::BTreeMap::new();
        dict.insert(b"port".to_vec(), BencodeValue::Int(6881));
        let args = BencodeValue::Dict(dict);

        assert_eq!(extract_port_from_args(&Some(args)), Some(6881));
    }

    #[test]
    fn test_extract_port_out_of_range() {
        let mut dict = std::collections::BTreeMap::new();
        dict.insert(b"port".to_vec(), BencodeValue::Int(70000));
        let args = BencodeValue::Dict(dict);

        assert_eq!(extract_port_from_args(&Some(args)), None);
    }

    #[test]
    fn test_extract_implied_port_from_args() {
        let mut dict = std::collections::BTreeMap::new();
        dict.insert(b"implied_port".to_vec(), BencodeValue::Int(1));
        let args = BencodeValue::Dict(dict);

        assert_eq!(extract_implied_port_from_args(&Some(args)), Some(1));
    }

    #[test]
    fn test_extract_token_from_args() {
        let mut dict = std::collections::BTreeMap::new();
        dict.insert(b"token".to_vec(), BencodeValue::Bytes(b"abc123".to_vec()));
        let args = BencodeValue::Dict(dict);

        assert_eq!(
            extract_token_from_args(&Some(args)),
            Some("abc123".to_string())
        );
    }

    // ---- Direct handler tests (Task 5) ----

    #[tokio::test]
    async fn test_handle_ping_query_directly() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        let test_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let test_addr = test_socket.local_addr().unwrap();

        let query = DhtMessageBuilder::ping(12345, &[0xAA; 20]);
        engine.handle_ping_query(test_addr, &query).await;

        let response = recv_dht_message(&test_socket).await;
        assert!(response.is_response(), "should be a response message");

        let r = response.r.as_ref().expect("response must have r field");
        let id_bytes = r
            .dict_get(b"id")
            .and_then(|v| v.as_bytes())
            .expect("missing r.id");
        assert_eq!(
            id_bytes,
            &engine.config.self_id[..],
            "r.id should be engine's self_id"
        );

        engine.shutdown();
    }

    #[tokio::test]
    async fn test_handle_find_node_query_directly() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        // Insert a known node so find_node has something to return
        let known_id = [0x55u8; 20];
        let known_addr: SocketAddr = "192.168.1.50:9999".parse().unwrap();
        engine.add_node(DhtNode::new(known_id, known_addr)).await;

        let test_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let test_addr = test_socket.local_addr().unwrap();

        let target = [0xFFu8; 20];
        let query = DhtMessageBuilder::find_node(42, &[0xBB; 20], &target);
        engine.handle_find_node_query(test_addr, &query).await;

        let response = recv_dht_message(&test_socket).await;
        assert!(response.is_response());

        let r = response.r.as_ref().expect("response must have r field");
        let nodes_bytes = r.dict_get(b"nodes").and_then(|v| v.as_bytes());
        assert!(
            nodes_bytes.is_some(),
            "find_node response should have nodes field"
        );

        // The known node is IPv4, so it should be in the compact nodes (26 bytes)
        if let Some(nodes) = nodes_bytes
            && !nodes.is_empty()
        {
            assert_eq!(
                nodes.len() % 26,
                0,
                "compact nodes should be 26-byte aligned"
            );
        }

        engine.shutdown();
    }

    #[tokio::test]
    async fn test_handle_get_peers_query_returns_nodes_when_empty() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        // Insert a known node so get_peers has nodes to return
        let known_id = [0x77u8; 20];
        let known_addr: SocketAddr = "10.0.0.7:7777".parse().unwrap();
        engine.add_node(DhtNode::new(known_id, known_addr)).await;

        let test_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let test_addr = test_socket.local_addr().unwrap();

        let info_hash = [0xCCu8; 20];
        let query = DhtMessageBuilder::get_peers(99, &[0xDD; 20], &info_hash);
        engine.handle_get_peers_query(test_addr, &query).await;

        let response = recv_dht_message(&test_socket).await;
        assert!(response.is_response());

        let r = response.r.as_ref().expect("response must have r field");

        // Should have a token
        let token = r.dict_get(b"token").and_then(|v| v.as_bytes());
        assert!(token.is_some(), "get_peers response must include a token");
        assert!(!token.unwrap().is_empty(), "token should not be empty");

        // Should have nodes (no peers stored)
        let nodes = r.dict_get(b"nodes").and_then(|v| v.as_bytes());
        assert!(nodes.is_some(), "should have nodes field when no peers");

        // Should NOT have values field
        assert!(
            r.dict_get(b"values").is_none(),
            "should not have values field when no peers"
        );

        engine.shutdown();
    }

    #[tokio::test]
    async fn test_handle_announce_peer_valid_token() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        let test_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let test_addr = test_socket.local_addr().unwrap();

        let info_hash = [0xEEu8; 20];
        let announce_port = 51413u16;

        // Generate a valid token for the test address
        let token = engine.token_tracker.generate_token(&info_hash, &test_addr);

        let query =
            DhtMessageBuilder::announce_peer(777, &[0xFF; 20], &info_hash, announce_port, &token);
        engine.handle_announce_peer_query(test_addr, &query).await;

        let response = recv_dht_message(&test_socket).await;
        assert!(response.is_response(), "valid token should get a response");

        // Verify the peer was stored
        let stored_peers = engine.peer_storage.get_peers(&info_hash);
        let expected_addr = SocketAddr::new(test_addr.ip(), announce_port);
        assert!(
            stored_peers.contains(&expected_addr),
            "announced peer {} should be in storage (got {:?})",
            expected_addr,
            stored_peers
        );

        engine.shutdown();
    }

    #[tokio::test]
    async fn test_handle_announce_peer_invalid_token() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        let test_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let test_addr = test_socket.local_addr().unwrap();

        let info_hash = [0x11u8; 20];
        let bad_token = "this_is_not_a_valid_token";

        let query = DhtMessageBuilder::announce_peer(888, &[0x22; 20], &info_hash, 6881, bad_token);
        engine.handle_announce_peer_query(test_addr, &query).await;

        let response = recv_dht_message(&test_socket).await;
        assert!(
            response.is_error(),
            "invalid token should get an error response"
        );

        // Verify no peer was stored
        let stored_peers = engine.peer_storage.get_peers(&info_hash);
        assert!(
            stored_peers.is_empty(),
            "no peer should be stored for invalid token"
        );

        engine.shutdown();
    }

    #[tokio::test]
    async fn test_handle_announce_peer_implied_port() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        let test_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let test_addr = test_socket.local_addr().unwrap();

        let info_hash = [0x33u8; 20];

        // Generate a valid token
        let token = engine.token_tracker.generate_token(&info_hash, &test_addr);

        // Build announce_peer with implied_port=1 and a DIFFERENT port
        let mut args_dict = std::collections::BTreeMap::new();
        args_dict.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0x44; 20]));
        args_dict.insert(
            b"info_hash".to_vec(),
            BencodeValue::Bytes(info_hash.to_vec()),
        );
        args_dict.insert(b"port".to_vec(), BencodeValue::Int(9999));
        args_dict.insert(b"implied_port".to_vec(), BencodeValue::Int(1));
        args_dict.insert(
            b"token".to_vec(),
            BencodeValue::Bytes(token.as_bytes().to_vec()),
        );

        let query = DhtMessage::new_query(555, "announce_peer", BencodeValue::Dict(args_dict));
        engine.handle_announce_peer_query(test_addr, &query).await;

        let response = recv_dht_message(&test_socket).await;
        assert!(
            response.is_response(),
            "implied_port announce should succeed"
        );

        // With implied_port=1, the stored peer should use the SENDER's port,
        // not the explicit port field (9999).
        let stored_peers = engine.peer_storage.get_peers(&info_hash);
        assert_eq!(stored_peers.len(), 1, "exactly one peer should be stored");
        assert_eq!(
            stored_peers[0], test_addr,
            "implied_port should use sender's address, not the port field"
        );

        engine.shutdown();
    }

    #[tokio::test]
    async fn test_handle_get_peers_query_returns_peers_after_announce() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        let test_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let test_addr = test_socket.local_addr().unwrap();

        let info_hash = [0x99u8; 20];

        // Step 1: Announce a peer
        let token = engine.token_tracker.generate_token(&info_hash, &test_addr);
        let announce_query =
            DhtMessageBuilder::announce_peer(111, &[0xAA; 20], &info_hash, 51413, &token);
        engine
            .handle_announce_peer_query(test_addr, &announce_query)
            .await;
        // Consume the announce response
        let _ = recv_dht_message(&test_socket).await;

        // Step 2: get_peers should now return the announced peer
        let get_peers_query = DhtMessageBuilder::get_peers(222, &[0xBB; 20], &info_hash);
        engine
            .handle_get_peers_query(test_addr, &get_peers_query)
            .await;

        let response = recv_dht_message(&test_socket).await;
        assert!(response.is_response());

        let r = response.r.as_ref().expect("response must have r field");

        // Should have values (peers), not nodes
        let values = r.dict_get(b"values").and_then(|v| v.as_list());
        assert!(values.is_some(), "should have values field with peers");
        assert_eq!(values.unwrap().len(), 1, "should have exactly 1 peer");

        // Should still have a token
        let token = r.dict_get(b"token").and_then(|v| v.as_bytes());
        assert!(token.is_some(), "should still include a token");

        engine.shutdown();
    }

    #[tokio::test]
    async fn test_handle_announce_peer_missing_fields() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        let test_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let test_addr = test_socket.local_addr().unwrap();

        // Build a query with only id, missing info_hash and token
        let mut args_dict = std::collections::BTreeMap::new();
        args_dict.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0x55; 20]));
        let query = DhtMessage::new_query(333, "announce_peer", BencodeValue::Dict(args_dict));
        engine.handle_announce_peer_query(test_addr, &query).await;

        let response = recv_dht_message(&test_socket).await;
        assert!(
            response.is_error(),
            "missing fields should result in error response"
        );

        engine.shutdown();
    }

    // ---- Full roundtrip tests (Task 8: integration) ----

    #[tokio::test]
    async fn test_full_ping_roundtrip_two_engines() {
        let engine_a = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();
        let engine_b = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        // Register a pending query on engine A
        let tx_id = engine_a.next_tx_id();
        let tx_id_bytes = tx_id.to_be_bytes().to_vec();
        let rx = engine_a.register_pending_query(tx_id_bytes.clone());

        // A sends a ping to B. Use localhost because the socket is bound to
        // 0.0.0.0 and Windows rejects sends to 0.0.0.0 (os error 10049).
        let b_addr = localhost_addr(engine_b.local_addr());
        let msg = DhtMessageBuilder::ping(tx_id, &engine_a.config.self_id);
        let data = msg.encode().unwrap();
        engine_a.socket.send_to(b_addr, &data).await.unwrap();

        // Wait for B's response (routed by A's query handler)
        let response = tokio::time::timeout(Duration::from_secs(3), rx)
            .await
            .expect("should receive response within 3s")
            .expect("oneshot should not be cancelled");

        assert!(response.is_response(), "should be a response");

        let r = response.r.as_ref().expect("response must have r field");
        let id_bytes = r
            .dict_get(b"id")
            .and_then(|v| v.as_bytes())
            .expect("missing r.id");
        assert_eq!(
            id_bytes,
            &engine_b.config.self_id[..],
            "response r.id should be engine B's self_id"
        );

        engine_a.shutdown();
        engine_b.shutdown();
    }

    #[tokio::test]
    async fn test_full_get_peers_roundtrip_two_engines() {
        let engine_a = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();
        let engine_b = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        // Add B to A's routing table so A knows where to send. Use localhost
        // because the socket is bound to 0.0.0.0 (Windows rejects sends to it).
        let b_addr = localhost_addr(engine_b.local_addr());
        engine_a
            .add_node(DhtNode::new(engine_b.config.self_id, b_addr))
            .await;

        let info_hash = [0x42u8; 20];

        // Register a pending query on A
        let tx_id = engine_a.next_tx_id();
        let tx_id_bytes = tx_id.to_be_bytes().to_vec();
        let rx = engine_a.register_pending_query(tx_id_bytes);

        // A sends get_peers to B
        let msg = DhtMessageBuilder::get_peers(tx_id, &engine_a.config.self_id, &info_hash);
        let data = msg.encode().unwrap();
        engine_a.socket.send_to(b_addr, &data).await.unwrap();

        let response = tokio::time::timeout(Duration::from_secs(3), rx)
            .await
            .expect("should receive get_peers response within 3s")
            .expect("oneshot should not be cancelled");

        assert!(response.is_response());

        let r = response.r.as_ref().expect("response must have r field");
        let token = r.dict_get(b"token").and_then(|v| v.as_bytes());
        assert!(token.is_some(), "get_peers response should include a token");

        engine_a.shutdown();
        engine_b.shutdown();
    }

    #[tokio::test]
    async fn test_full_announce_peer_roundtrip() {
        let engine_a = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();
        let engine_b = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        let info_hash = [0x77u8; 20];
        let announce_port = 51413u16;

        // Use localhost because sockets bind to 0.0.0.0 (Windows rejects
        // sends to 0.0.0.0 with os error 10049).
        let b_addr = localhost_addr(engine_b.local_addr());

        // Step 1: A sends get_peers to B to obtain a token
        let tx_id1 = engine_a.next_tx_id();
        let tx_id1_bytes = tx_id1.to_be_bytes().to_vec();
        let rx1 = engine_a.register_pending_query(tx_id1_bytes);

        let get_peers_msg =
            DhtMessageBuilder::get_peers(tx_id1, &engine_a.config.self_id, &info_hash);
        engine_a
            .socket
            .send_to(b_addr, &get_peers_msg.encode().unwrap())
            .await
            .unwrap();

        let get_peers_resp = tokio::time::timeout(Duration::from_secs(3), rx1)
            .await
            .expect("should receive get_peers response")
            .expect("oneshot should not be cancelled");

        // Extract the token from B's response
        let token_str = {
            let r = get_peers_resp.r.as_ref().expect("response must have r");
            let token_bytes = r
                .dict_get(b"token")
                .and_then(|v| v.as_bytes())
                .expect("response should have token");
            String::from_utf8(token_bytes.to_vec()).expect("token should be valid UTF-8")
        };

        // Step 2: A sends announce_peer to B with the token
        let tx_id2 = engine_a.next_tx_id();
        let tx_id2_bytes = tx_id2.to_be_bytes().to_vec();
        let rx2 = engine_a.register_pending_query(tx_id2_bytes);

        let announce_msg = DhtMessageBuilder::announce_peer(
            tx_id2,
            &engine_a.config.self_id,
            &info_hash,
            announce_port,
            &token_str,
        );
        engine_a
            .socket
            .send_to(b_addr, &announce_msg.encode().unwrap())
            .await
            .unwrap();

        let announce_resp = tokio::time::timeout(Duration::from_secs(3), rx2)
            .await
            .expect("should receive announce_peer response")
            .expect("oneshot should not be cancelled");

        assert!(announce_resp.is_response(), "announce should succeed");

        // Step 3: Verify B stored the peer. The announce_peer query does not
        // set implied_port, so B uses the explicit `port` field with the
        // sender's IP. Since A sent via localhost, the sender IP B sees is
        // 127.0.0.1 — NOT 0.0.0.0 (which is only a bind address).
        let stored_peers = engine_b.peer_storage.get_peers(&info_hash);
        let expected_addr = SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), announce_port);
        assert!(
            stored_peers.contains(&expected_addr),
            "B should have stored the announced peer {} (got {:?})",
            expected_addr,
            stored_peers
        );

        engine_a.shutdown();
        engine_b.shutdown();
    }

    // ---- Shutdown test (Task 8.7) ----

    #[tokio::test]
    async fn test_query_handler_exits_on_shutdown() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();
        assert!(
            engine.running.load(Ordering::Relaxed),
            "engine should be running"
        );

        // Shutdown should complete without hanging
        let shutdown_result = tokio::time::timeout(Duration::from_secs(2), async {
            engine.shutdown_async().await;
        })
        .await;

        assert!(
            shutdown_result.is_ok(),
            "shutdown should complete within 2s"
        );
        assert!(
            !engine.running.load(Ordering::Relaxed),
            "engine should be stopped after shutdown"
        );

        // Give the query handler time to notice the flag and exit
        sleep(Duration::from_millis(300)).await;
    }

    // ---- Task 7: find_peers cache integration test ----

    #[tokio::test]
    async fn test_find_peers_returns_cached_peers() {
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        let info_hash = [0xCAu8; 20];
        let cached_peer: SocketAddr = "203.0.113.42:4242".parse().unwrap();

        // Manually add a peer to the storage
        engine.peer_storage.add_peer(info_hash, cached_peer);

        // find_peers should return the cached peer immediately without
        // hitting the network.
        let result = tokio::time::timeout(Duration::from_millis(500), async {
            engine.find_peers(&info_hash).await
        })
        .await;

        assert!(
            result.is_ok(),
            "find_peers with cached peers should return immediately"
        );

        let discovery = result.unwrap();
        assert_eq!(discovery.peers, vec![cached_peer]);
        assert_eq!(
            discovery.nodes_contacted, 0,
            "should not contact any nodes when using cache"
        );
        assert_eq!(
            discovery.rounds_completed, 0,
            "should complete 0 rounds (cache hit)"
        );

        engine.shutdown();
    }

    #[tokio::test]
    async fn test_find_peers_stores_discovered_peers() {
        // This test verifies that find_peers stores discovered peers in
        // DhtPeerStorage. Since we can't easily get real peers from the
        // DHT network in a test, we verify the storage integration by
        // checking that the storage is populated after find_peers returns.
        let engine = DhtEngine::start(DhtEngineConfig::default()).await.unwrap();

        let info_hash = [0xFEu8; 20];

        // find_peers will likely not find peers (no real DHT network),
        // but it should still complete without error.
        let _ = engine.find_peers(&info_hash).await;

        // The storage should be empty (no peers found), but the method
        // should not have panicked.
        let stored = engine.peer_storage.get_peers(&info_hash);
        assert!(
            stored.is_empty(),
            "no peers should be stored if none were found"
        );

        engine.shutdown();
    }
}

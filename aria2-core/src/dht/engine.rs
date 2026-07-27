//! DHT engine: top-level orchestrator for the DHT subsystem.
//!
//! Owns all DHT components and runs the main event loop:
//! 1. Receive inbound UDP datagrams
//! 2. Decode and process messages (update routing table, generate responses)
//! 3. Send outbound messages (responses + task-initiated queries)
//! 4. Execute periodic tasks (bucket refresh, token rotation, auto-save)
//!
//! Unlike the C++ version which uses a Command pattern with global
//! registries (`DHTRegistry`), this Rust implementation uses an owned
//! `DhtEngine` struct with async methods. This is more idiomatic for
//! the tokio runtime and avoids global mutable state.
//!
//! C++ reference: `DHTSetup.cc` + `DHTInteractionCommand.cc` + `DHTRegistry.h`

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::time::{self, MissedTickBehavior};
use tracing::{debug, info, trace, warn};

use super::constants::{
    BUCKET_REFRESH_CHECK_INTERVAL_SECS, DHT_MAX_MESSAGE_SIZE,
    PEER_ANNOUNCE_CHECK_INTERVAL_SECS, TOKEN_UPDATE_INTERVAL_SECS,
};
use super::dispatcher::DhtDispatcher;
use super::node::DhtNode;
use super::node_id::NodeId;
use super::peer_announce::DhtPeerAnnounceStorage;
use super::receiver::{DhtReceiver, ReceiveAction};
use super::routing_table::RoutingTable;
use super::routing_table_ser;
use super::task::{
    DhtBucketRefreshTask, DhtLookupTask, DhtTask, DhtTaskQueue,
    LookupKind, TaskExecutor,
};
use super::token_tracker::TokenTracker;
use super::transport::{AddressFamily, DhtTransport};

// ── DHT entry point ───────────────────────────────────────────────────────

/// A DHT bootstrap entry point (host:port pair).
#[derive(Clone, Debug)]
pub struct DhtEntryPoint {
    pub host: String,
    pub port: u16,
}

// ── DhtEngineConfig ───────────────────────────────────────────────────────

/// Configuration for the DHT engine.
#[derive(Clone, Debug)]
pub struct DhtEngineConfig {
    /// Local address to bind to (empty = wildcard).
    pub listen_addr: String,
    /// Local port (0 = OS-assigned).
    pub listen_port: u16,
    /// Address family (IPv4 or IPv6).
    pub family: AddressFamily,
    /// Path to the DHT routing table file (dht.dat).
    pub dht_file_path: PathBuf,
    /// DHT bootstrap entry points.
    pub entry_points: Vec<DhtEntryPoint>,
    /// Message timeout in seconds.
    pub message_timeout_secs: u64,
}

impl Default for DhtEngineConfig {
    fn default() -> Self {
        Self {
            listen_addr: String::new(),
            listen_port: 0,
            family: AddressFamily::Ipv4,
            dht_file_path: PathBuf::from("dht.dat"),
            entry_points: Vec::new(),
            message_timeout_secs: 10,
        }
    }
}

// ── DhtEngine ─────────────────────────────────────────────────────────────

/// The DHT engine: owns all DHT components and runs the main event loop.
///
/// This struct replaces the C++ combination of:
/// - `DHTRegistry` (global state)
/// - `DHTSetup` (initialization)
/// - `DHTInteractionCommand` (main loop)
/// - Periodic commands (`DHTTokenUpdateCommand`, `DHTBucketRefreshCommand`,
///   `DHTPeerAnnounceCommand`, `DHTAutoSaveCommand`)
///
/// The engine is started via [`run`] which blocks until shutdown.
pub struct DhtEngine {
    /// The local node's ID.
    local_id: NodeId,
    /// The UDP transport.
    transport: DhtTransport,
    /// The routing table.
    routing_table: RoutingTable,
    /// The message dispatcher (outbound).
    dispatcher: DhtDispatcher,
    /// The message receiver (inbound).
    receiver: DhtReceiver,
    /// The peer announce storage.
    peer_announce_storage: DhtPeerAnnounceStorage,
    /// The token tracker.
    token_tracker: TokenTracker,
    /// The task queue.
    task_queue: DhtTaskQueue,
    /// The task executor.
    task_executor: TaskExecutor,
    /// Engine configuration.
    config: DhtEngineConfig,
    /// Whether the engine has been bootstrapped.
    bootstrapped: bool,
}

impl DhtEngine {
    /// Create and initialize a new DHT engine.
    ///
    /// This binds the UDP socket, loads the routing table from disk,
    /// and wires up all components.
    pub async fn new(config: DhtEngineConfig) -> std::io::Result<Self> {
        // Bind the UDP transport
        let transport = DhtTransport::bind(&config.listen_addr, config.listen_port).await?;
        let bound_addr = transport.local_addr()?;
        info!(
            addr = %bound_addr,
            family = ?config.family,
            "DHT engine initialized"
        );

        // Load or generate local node ID
        let local_id = Self::load_or_create_local_id(&config.dht_file_path);

        // Load routing table from disk
        let mut routing_table = RoutingTable::new(local_id);
        Self::load_routing_table(&config.dht_file_path, &mut routing_table, config.family);

        // Create components
        let timeout = Duration::from_secs(config.message_timeout_secs);
        let dispatcher = DhtDispatcher::with_timeout(timeout);
        let receiver = DhtReceiver::new(local_id);
        let peer_announce_storage = DhtPeerAnnounceStorage::new();
        let token_tracker = TokenTracker::new();
        let task_queue = DhtTaskQueue::new();
        let task_executor = TaskExecutor::new(8); // max concurrent tasks

        let bootstrapped = false;

        Ok(Self {
            local_id,
            transport,
            routing_table,
            dispatcher,
            receiver,
            peer_announce_storage,
            token_tracker,
            task_queue,
            task_executor,
            config,
            bootstrapped,
        })
    }

    /// Run the DHT engine main loop.
    ///
    /// This method blocks until a shutdown signal is received.
    /// It performs:
    /// - Receiving and processing inbound messages
    /// - Sending outbound messages
    /// - Executing periodic tasks (bucket refresh, token rotation, auto-save)
    /// - Bootstrapping from entry points
    pub async fn run(&mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        // Bootstrap from entry points if needed
        if !self.bootstrapped && !self.config.entry_points.is_empty() {
            self.bootstrap().await;
        }

        // Periodic task intervals
        let mut token_update_interval = time::interval(Duration::from_secs(TOKEN_UPDATE_INTERVAL_SECS));
        token_update_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut bucket_refresh_interval = time::interval(Duration::from_secs(BUCKET_REFRESH_CHECK_INTERVAL_SECS));
        bucket_refresh_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut peer_announce_interval = time::interval(Duration::from_secs(PEER_ANNOUNCE_CHECK_INTERVAL_SECS));
        peer_announce_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut auto_save_interval = time::interval(Duration::from_secs(30 * 60));
        auto_save_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // Receive buffer
        let mut buf = [0u8; DHT_MAX_MESSAGE_SIZE];

        info!("DHT engine main loop started");

        loop {
            tokio::select! {
                // Check for shutdown signal
                _ = shutdown.changed() => {
                    info!("DHT engine shutting down");
                    break;
                }

                // Receive inbound UDP message
                result = self.transport.recv_message(&mut buf) => {
                    match result {
                        Ok((len, sender)) => {
                            self.handle_inbound_message(&buf[..len], sender).await;
                        }
                        Err(e) => {
                            warn!(error = %e, "Error receiving DHT message");
                        }
                    }
                }

                // Periodic: token rotation
                _ = token_update_interval.tick() => {
                    trace!("Rotating DHT token secrets");
                    self.token_tracker.update_secret();
                }

                // Periodic: bucket refresh check
                _ = bucket_refresh_interval.tick() => {
                    self.bucket_refresh_check();
                }

                // Periodic: peer announce check
                _ = peer_announce_interval.tick() => {
                    self.peer_announce_check().await;
                }

                // Periodic: auto-save routing table
                _ = auto_save_interval.tick() => {
                    self.auto_save();
                }
            }
        }

        // Final save on shutdown
        self.auto_save();
        info!("DHT engine stopped");
    }

    /// Get the local node ID.
    pub fn local_id(&self) -> &NodeId {
        &self.local_id
    }

    /// Get the bound socket address.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.transport.local_addr()
    }

    /// Get a reference to the routing table.
    pub fn routing_table(&self) -> &RoutingTable {
        &self.routing_table
    }

    /// Get a reference to the peer announce storage.
    pub fn peer_announce_storage(&self) -> &DhtPeerAnnounceStorage {
        &self.peer_announce_storage
    }

    /// Register a local info hash for DHT announcement.
    pub fn announce_local_info_hash(&mut self, info_hash: NodeId) {
        self.peer_announce_storage.add_local_info_hash(info_hash);
    }

    /// Remove a local info hash from DHT announcement.
    pub fn remove_local_info_hash(&mut self, info_hash: &NodeId) {
        self.peer_announce_storage.remove_local_info_hash(info_hash);
    }

    /// Initiate a peer lookup for the given info hash.
    pub fn lookup_peers(&mut self, info_hash: NodeId) {
        let mut task = DhtLookupTask::new(info_hash, LookupKind::Peer);
        task.state_mut().startup(&self.routing_table, &self.local_id);
        task.startup();
        self.task_queue.add_immediate(Box::new(task));
    }

    // ── Private methods ────────────────────────────────────────────────────

    /// Handle an inbound UDP message.
    async fn handle_inbound_message(&mut self, data: &[u8], sender: SocketAddr) {
        // Step 1: Process through receiver
        let actions = self.receiver.receive_message(
            data,
            sender,
            &mut self.dispatcher,
            &mut self.routing_table,
            &mut self.peer_announce_storage,
            &mut self.token_tracker,
        );

        // Step 2: Execute actions (send responses, etc.)
        for action in actions {
            match action {
                ReceiveAction::Respond(reply) => {
                    self.dispatcher.add_message(reply);
                }
                ReceiveAction::ResponseReceived { method, sender_addr } => {
                    trace!(
                        method = %method,
                        addr = %sender_addr,
                        "DHT response processed"
                    );
                }
                ReceiveAction::NoAction => {}
            }
        }

        // Step 3: Send any queued messages
        self.dispatcher.send_messages(&self.transport).await;

        // Step 4: Execute tasks (may queue more messages)
        self.execute_tasks().await;
    }

    /// Execute pending tasks from the task queue.
    async fn execute_tasks(&mut self) {
        // Execute tasks from the task queue
        self.task_queue.execute();

        // Execute tasks through the task executor
        self.task_executor.update();

        // Send any messages queued by tasks
        if self.dispatcher.queue_length() > 0 {
            self.dispatcher.send_messages(&self.transport).await;
        }

        // Handle timeouts
        let timed_out = self.receiver.handle_timeouts(
            &mut self.dispatcher,
            &mut self.routing_table,
        );
        if !timed_out.is_empty() {
            trace!(count = timed_out.len(), "DHT queries timed out");
        }
    }

    /// Bootstrap from configured entry points.
    async fn bootstrap(&mut self) {
        info!("Bootstrapping DHT from entry points");

        for entry in &self.config.entry_points {
            // Resolve the entry point hostname
            match tokio::net::lookup_host(format!("{}:{}", entry.host, entry.port)).await {
                Ok(addrs) => {
                    for addr in addrs {
                        info!(addr = %addr, "Adding DHT entry point node");
                        // Create a node from the entry point and add it
                        let node = DhtNode::new(NodeId::random(), addr);
                        self.routing_table.add_node(node);

                        // Start a find_node lookup to discover more nodes
                        let target = self.local_id;
                        let mut task = DhtLookupTask::new(target, LookupKind::Node);
                        task.state_mut().startup(&self.routing_table, &self.local_id);
                        task.startup();
                        self.task_queue.add_immediate(Box::new(task));
                    }
                }
                Err(e) => {
                    warn!(
                        host = %entry.host,
                        port = entry.port,
                        error = %e,
                        "Failed to resolve DHT entry point"
                    );
                }
            }
        }

        // Also trigger a bucket refresh to populate the routing table
        let bucket_refresh = DhtBucketRefreshTask::new(true);
        self.task_queue.add_periodic1(Box::new(bucket_refresh));

        self.bootstrapped = true;
    }

    /// Check if any buckets need refreshing and start refresh tasks.
    fn bucket_refresh_check(&self) {
        for bucket in self.routing_table.get_buckets() {
            if bucket.needs_refresh() {
                let prefix_len = bucket.prefix_length();
                trace!(prefix_len, "Bucket needs refresh");
                // A bucket refresh task would be created here via the task factory
            }
        }
    }

    /// Check if any local info hashes need re-announcing.
    async fn peer_announce_check(&mut self) {
        // Purge stale peer entries
        self.peer_announce_storage.handle_timeout();

        // Get info hashes needing re-announcement
        let info_hashes = self.peer_announce_storage.local_info_hashes();
        for info_hash in info_hashes {
            debug!(info_hash = %info_hash, "Re-announcing local info hash via DHT");
            let mut task = DhtLookupTask::new(*info_hash, LookupKind::Peer);
            task.state_mut().startup(&self.routing_table, &self.local_id);
            task.startup();
            self.task_queue.add_immediate(Box::new(task));
        }
    }

    /// Save the routing table to disk.
    fn auto_save(&self) {
        let path = &self.config.dht_file_path;
        match routing_table_ser::serialize_to_file(&self.routing_table, path) {
            Ok(()) => {
                trace!(path = %path.display(), "Saved DHT routing table");
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Failed to save DHT routing table");
            }
        }
    }

    /// Load or create a local node ID.
    fn load_or_create_local_id(dht_file: &PathBuf) -> NodeId {
        // Try to load from the routing table file
        match routing_table_ser::deserialize_from_file(dht_file) {
            Ok(result) => {
                debug!(id = %result.local_node_id, "Loaded local node ID from dht.dat");
                result.local_node_id
            }
            Err(e) => {
                debug!(error = %e, "No existing DHT data file, generating new node ID");
                let id = NodeId::random();
                debug!(id = %id, "Generated new DHT node ID");
                id
            }
        }
    }

    /// Load the routing table from disk.
    fn load_routing_table(
        dht_file: &PathBuf,
        routing_table: &mut RoutingTable,
        _family: AddressFamily,
    ) {
        match routing_table_ser::deserialize_from_file(dht_file) {
            Ok(result) => {
                for node in result.nodes {
                    routing_table.add_node(node);
                }
                debug!(
                    "Loaded DHT nodes from routing table file"
                );
            }
            Err(e) => {
                debug!(error = %e, "No DHT routing table file found, starting fresh");
            }
        }
    }
}

impl DhtPeerAnnounceStorage {
    /// Get info hashes that need re-announcement.
    ///
    /// Returns all local info hashes (a more sophisticated implementation
    /// would track last_announce time per entry and only return those
    /// past the PEER_ANNOUNCE_INTERVAL threshold).
    #[allow(dead_code)]
    fn info_hashes_needing_announce(&self) -> Vec<NodeId> {
        self.local_info_hashes().iter().copied().collect()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_values() {
        let config = DhtEngineConfig::default();
        assert!(config.listen_addr.is_empty());
        assert_eq!(config.listen_port, 0);
        assert_eq!(config.family, AddressFamily::Ipv4);
        assert_eq!(config.dht_file_path, PathBuf::from("dht.dat"));
        assert!(config.entry_points.is_empty());
        assert_eq!(config.message_timeout_secs, 10);
    }

    #[test]
    fn entry_point_clone_debug() {
        let ep = DhtEntryPoint {
            host: "router.bittorrent.com".to_owned(),
            port: 6881,
        };
        let ep2 = ep.clone();
        assert_eq!(ep.host, ep2.host);
        assert_eq!(ep.port, ep2.port);
        let _ = format!("{:?}", ep);
    }
}

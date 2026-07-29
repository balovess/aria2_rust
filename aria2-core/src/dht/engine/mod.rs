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
//!
//! # Module layout
//!
//! - [`mod`] (this file) — core types, struct definition, public API
//! - [`message_handling`] — inbound message processing, lookup result handling
//! - [`task_execution`] — task execution, lookup driving, ping/replace handlers
//! - [`bootstrap`] — bootstrap, periodic tasks, persistence

pub mod bootstrap;
pub mod message_handling;
pub mod task_execution;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::time::{self, MissedTickBehavior};
use tracing::{info, trace, warn};

use super::constants::{
    BUCKET_REFRESH_CHECK_INTERVAL_SECS, DHT_MAX_MESSAGE_SIZE,
    PEER_ANNOUNCE_CHECK_INTERVAL_SECS, TOKEN_UPDATE_INTERVAL_SECS,
};
use super::dispatcher::DhtDispatcher;
use super::node_id::NodeId;
use super::peer_announce::DhtPeerAnnounceStorage;
use super::receiver::DhtReceiver;
use super::routing_table::RoutingTable;
use super::task::{
    self, DhtPingTask, DhtReplaceNodeTask, DhtTaskQueue, LookupKind, LookupResult, LookupState,
    TaskExecutor,
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

// ── ActiveLookup ────────────────────────────────────────────────────────

/// An active Kademlia lookup being driven by the DHT engine.
///
/// Unlike the C++ version where `DHTAbstractNodeLookupTask` sends queries
/// directly via `DHTMessageDispatcher`, this Rust version separates the
/// lookup state machine from the message dispatching. The engine polls
/// each active lookup for the next batch of query targets, constructs
/// the appropriate DHT messages, and queues them via the dispatcher.
///
/// C++: `DHTAbstractNodeLookupTask<Resp>` + `DHTNodeLookupTask` / `DHTPeerLookupTask`
pub struct ActiveLookup {
    /// The lookup state machine.
    state: LookupState,
}

impl ActiveLookup {
    /// Create a new active lookup from an initial state.
    pub fn new(state: LookupState) -> Self {
        Self { state }
    }

    /// Get a reference to the lookup state.
    pub fn state(&self) -> &LookupState {
        &self.state
    }

    /// Get a mutable reference to the lookup state.
    pub fn state_mut(&mut self) -> &mut LookupState {
        &mut self.state
    }

    /// Whether this lookup has finished.
    pub fn is_done(&self) -> bool {
        self.state.is_done()
    }

    /// Consume the state and produce a result.
    pub fn into_result(self) -> LookupResult {
        self.state.into_result()
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
    /// The task queue (for non-lookup tasks: ping, replace node, bucket refresh).
    task_queue: DhtTaskQueue,
    /// The task executor (for non-lookup tasks).
    task_executor: TaskExecutor,
    /// Active lookups being driven by the engine.
    /// Unlike the C++ version where lookups are opaque tasks, here the engine
    /// directly manages the lookup state machine to dispatch queries.
    active_lookups: Vec<ActiveLookup>,
    /// Active ping tasks for bootstrap entry points and replace-node probes.
    ///
    /// C++: `DHTEntryPointNameResolveCommand::addPingTask()` creates a
    /// `DHTPingTask` with 10 retries. When the ping succeeds, the response
    /// handler adds the node to the routing table. When it fails after all
    /// retries, the node is discarded.
    active_pings: Vec<DhtPingTask>,
    /// Active replace-node tasks.
    ///
    /// C++: `DHTReplaceNodeTask` is created when a good node is cached in a
    /// full bucket. It pings the LRU questionable node; if it doesn't respond,
    /// it's replaced with the cached candidate.
    active_replace_tasks: Vec<DhtReplaceNodeTask>,
    /// Channel for completed lookup results from non-engine-driven sources.
    lookup_result_rx: task::LookupResultReceiver,
    /// Channel sender for lookup results (cloned for external consumers).
    /// TODO: Wire into BT engine for announce_peer dispatch after peer lookup.
    #[allow(dead_code)]
    lookup_result_tx: task::LookupResultSender,
    /// TCP port for announce_peer (0 = use default).
    tcp_port: u16,
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
        let (lookup_result_tx, lookup_result_rx) = task::lookup_result_channel();

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
            active_lookups: Vec::new(),
            active_pings: Vec::new(),
            active_replace_tasks: Vec::new(),
            lookup_result_rx,
            lookup_result_tx,
            tcp_port: bound_addr.port(),
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
        let mut token_update_interval =
            time::interval(Duration::from_secs(TOKEN_UPDATE_INTERVAL_SECS));
        token_update_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut bucket_refresh_interval =
            time::interval(Duration::from_secs(BUCKET_REFRESH_CHECK_INTERVAL_SECS));
        bucket_refresh_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut peer_announce_interval =
            time::interval(Duration::from_secs(PEER_ANNOUNCE_CHECK_INTERVAL_SECS));
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

                // Receive completed lookup results from task system
                result = self.lookup_result_rx.recv() => {
                    match result {
                        Some(lookup_result) => {
                            self.handle_lookup_result(lookup_result).await;
                        }
                        None => {
                            warn!("Lookup result channel closed unexpectedly");
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
    ///
    /// After the lookup completes, the engine will:
    /// 1. Feed discovered peers to the peer announce storage
    /// 2. Send `announce_peer` messages to K closest nodes that provided tokens
    ///
    /// C++: `DHTGetPeersCommand::execute()` -> `taskFactory->createPeerLookupTask()`
    pub fn lookup_peers(&mut self, info_hash: NodeId) {
        let mut state = LookupState::new(info_hash, LookupKind::Peer);
        state.startup(&self.routing_table, &self.local_id);
        let lookup = ActiveLookup::new(state);
        self.active_lookups.push(lookup);
    }

    /// Initiate a node lookup for the given target ID.
    ///
    /// C++: `DHTBucketRefreshTask` -> `taskFactory->createNodeLookupTask()`
    pub fn lookup_nodes(&mut self, target: NodeId) {
        let mut state = LookupState::new(target, LookupKind::Node);
        state.startup(&self.routing_table, &self.local_id);
        let lookup = ActiveLookup::new(state);
        self.active_lookups.push(lookup);
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

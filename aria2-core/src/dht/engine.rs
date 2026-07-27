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
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::{self, MissedTickBehavior};
use tracing::{debug, info, trace, warn};

use super::constants::{
    BUCKET_REFRESH_CHECK_INTERVAL_SECS, DHT_MAX_MESSAGE_SIZE, K,
    PEER_ANNOUNCE_CHECK_INTERVAL_SECS, TOKEN_UPDATE_INTERVAL_SECS,
};
use super::dispatcher::DhtDispatcher;
use super::message::{AnnouncePeerQueryPayload, DhtMessage};
use super::node::DhtNode;
use super::node_id::NodeId;
use super::peer_announce::DhtPeerAnnounceStorage;
use super::receiver::{DhtReceiver, ReceiveAction};
use super::routing_table::RoutingTable;
use super::routing_table_ser;
use super::message_decode;
use super::task::{
    self, DhtBucketRefreshTask, DhtPingTask, DhtReplaceNodeTask, DhtTask, DhtTaskQueue,
    LookupKind, LookupResult, LookupState, TaskExecutor,
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
    /// C++: `DHTGetPeersCommand::execute()` → `taskFactory->createPeerLookupTask()`
    pub fn lookup_peers(&mut self, info_hash: NodeId) {
        let mut state = LookupState::new(info_hash, LookupKind::Peer);
        state.startup(&self.routing_table, &self.local_id);
        let lookup = ActiveLookup::new(state);
        self.active_lookups.push(lookup);
    }

    /// Initiate a node lookup for the given target ID.
    ///
    /// C++: `DHTBucketRefreshTask` → `taskFactory->createNodeLookupTask()`
    pub fn lookup_nodes(&mut self, target: NodeId) {
        let mut state = LookupState::new(target, LookupKind::Node);
        state.startup(&self.routing_table, &self.local_id);
        let lookup = ActiveLookup::new(state);
        self.active_lookups.push(lookup);
    }

    // ── Private methods ────────────────────────────────────────────────────

    /// Generate a random transaction ID for DHT messages.
    ///
    /// C++: `DHTMessageFactoryImpl::generateTransactionId()` uses random bytes.
    fn generate_transaction_id() -> Vec<u8> {
        use super::constants::TRANSACTION_ID_LENGTH;
        let mut tid = vec![0u8; TRANSACTION_ID_LENGTH];
        // Use a simple counter + random prefix for uniqueness
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let counter: u32 = (now.as_nanos() as u32) ^ (now.as_secs() as u32);
        tid[..4].copy_from_slice(&counter.to_ne_bytes());
        for b in tid.iter_mut().skip(4) {
            *b = rand::random::<u8>();
        }
        tid
    }

    /// Handle a completed lookup result.
    ///
    /// C++: `DHTPeerLookupTask::onFinish()` + `DHTPeerLookupTask::onReceivedInternal()`
    ///
    /// When a peer lookup finishes:
    /// 1. Add discovered peers to the peer announce storage
    /// 2. Send `announce_peer` messages to the K closest nodes that returned tokens
    ///
    /// When a node lookup finishes:
    /// 1. Add discovered nodes to the routing table
    async fn handle_lookup_result(&mut self, result: LookupResult) {
        match result.kind {
            LookupKind::Peer => {
                debug!(
                    info_hash = %result.target,
                    peers = result.peers.len(),
                    tokens = result.tokens.len(),
                    "Peer lookup completed"
                );

                // Feed discovered peers to the peer announce storage
                for peer_addr in &result.peers {
                    self.peer_announce_storage.add_peer_announce(&result.target, *peer_addr);
                }

                // Send announce_peer messages to K closest nodes that provided tokens.
                // C++: DHTPeerLookupTask::onFinish() iterates entries and calls
                // createAnnouncePeerMessage() for each node with a stored token.
                let mut token_count = 0;
                for (node_addr, token) in result.tokens.iter().take(K) {
                    let announce_msg = DhtMessage::AnnouncePeerQuery {
                        transaction_id: Self::generate_transaction_id(),
                        sender_id: self.local_id,
                        sender_addr: *node_addr,
                        payload: AnnouncePeerQueryPayload {
                            info_hash: result.target,
                            port: self.tcp_port,
                            token: token.clone(),
                        },
                    };
                    self.dispatcher.add_message(announce_msg);
                    token_count += 1;
                }
                debug!(
                    info_hash = %result.target,
                    announce_count = token_count,
                    "Queued announce_peer messages"
                );

                // Send the queued messages
                if self.dispatcher.queue_length() > 0 {
                    self.dispatcher.send_messages(&self.transport).await;
                }
            }
            LookupKind::Node => {
                debug!(
                    target = %result.target,
                    nodes = result.nodes.len(),
                    "Node lookup completed"
                );

                // Add discovered nodes to the routing table
                for node in &result.nodes {
                    self.routing_table.add_node(node.clone());
                }
            }
        }
    }

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
                ReceiveAction::ResponseReceived {
                    method,
                    sender_addr,
                    target_node_id: _,
                    elapsed,
                } => {
                    trace!(
                        method = %method,
                        addr = %sender_addr,
                        elapsed_ms = elapsed.as_millis(),
                        "DHT response processed"
                    );

                    // Feed the response to active lookups.
                    // C++: DHTPeerLookupTask::onReceivedInternal() and
                    // DHTNodeLookupTask handle responses via callbacks.
                    self.feed_response_to_lookups(method.clone(), sender_addr, data);

                    // Feed the response to active ping tasks.
                    // C++: DHTPingTask::onReceived() marks the node as
                    // successfully pinged. For bootstrap pings, this is when
                    // the real node ID becomes known (via the response).
                    self.handle_ping_response(sender_addr, elapsed);

                    // Feed the response to active replace-node tasks.
                    // C++: DHTReplaceNodeTask::onReceived() marks the
                    // questionable node as alive (no replacement needed).
                    self.handle_replace_node_response(sender_addr);
                }
                ReceiveAction::NoAction => {}
            }
        }

        // Step 3: Send any queued messages
        self.dispatcher.send_messages(&self.transport).await;

        // Step 4: Execute tasks (may queue more messages)
        self.execute_tasks().await;
    }

    /// Feed an inbound DHT response to active lookups that are waiting for it.
    ///
    /// When a response arrives from a node that an active lookup queried,
    /// the lookup state is updated with the response data (nodes, peers, tokens).
    ///
    /// C++: `DHTAbstractNodeLookupTask` uses a callback mechanism where each
    /// response triggers `onReceived()` → `onReceivedInternal()`. In Rust,
    /// we match the response to the lookup by the sender address and method.
    fn feed_response_to_lookups(&mut self, method: String, sender_addr: SocketAddr, data: &[u8]) {
        // Decode the response to extract nodes, peers, and tokens
        let (nodes, peers, token) = match method.as_str() {
            "find_node" => {
                match message_decode::decode_response_with_method(data, sender_addr, "find_node") {
                    Ok(DhtMessage::FindNodeResponse { payload, .. }) => {
                        let nodes: Vec<DhtNode> = payload
                            .nodes
                            .iter()
                            .map(|cni| DhtNode::new(cni.node_id, cni.addr))
                            .collect();
                        (nodes, Vec::new(), None)
                    }
                    _ => (Vec::new(), Vec::new(), None),
                }
            }
            "get_peers" => {
                match message_decode::decode_response_with_method(data, sender_addr, "get_peers") {
                    Ok(DhtMessage::GetPeersResponse { payload, .. }) => {
                        let nodes: Vec<DhtNode> = payload
                            .nodes
                            .iter()
                            .map(|cni| DhtNode::new(cni.node_id, cni.addr))
                            .collect();
                        let peers: Vec<SocketAddr> =
                            payload.values.iter().map(|pi| pi.addr).collect();
                        (nodes, peers, Some(payload.token))
                    }
                    _ => (Vec::new(), Vec::new(), None),
                }
            }
            _ => return,
        };

        // Update the matching active lookup(s)
        for lookup in &mut self.active_lookups {
            let matches_method = match lookup.state().kind() {
                LookupKind::Node => method == "find_node",
                LookupKind::Peer => method == "get_peers",
            };

            if !matches_method {
                continue;
            }

            // Check if the sender is one of the nodes we queried
            let is_queried = lookup
                .state()
                .entries()
                .iter()
                .any(|e| e.node.addr() == sender_addr && e.used);

            if is_queried {
                lookup.state_mut().on_response(
                    sender_addr,
                    nodes.clone(),
                    peers.clone(),
                    token.clone(),
                    &self.local_id,
                );
                trace!(
                    method = %method,
                    addr = %sender_addr,
                    nodes = nodes.len(),
                    peers = peers.len(),
                    "Fed response to active lookup"
                );
            }
        }
    }

    /// Execute pending tasks from the task queue.
    ///
    /// After executing, checks for finished lookup tasks and sends their
    /// results through the lookup result channel. Also drives active
    /// lookup tasks by sending the next batch of queries.
    async fn execute_tasks(&mut self) {
        // Execute tasks from the task queue
        self.task_queue.execute();

        // Execute tasks through the task executor
        self.task_executor.update();

        // Drive active lookup tasks: send the next batch of queries
        self.drive_active_lookups().await;

        // Send any messages queued by tasks
        if self.dispatcher.queue_length() > 0 {
            self.dispatcher.send_messages(&self.transport).await;
        }

        // Handle timeouts — updates node state (RTT, condition counter)
        let timed_out = self
            .receiver
            .handle_timeouts(&mut self.dispatcher, &mut self.routing_table);
        if !timed_out.is_empty() {
            // Process timeout entries for active ping and replace-node tasks
            for entry in &timed_out {
                self.handle_ping_timeout(entry.target_addr);
                self.handle_replace_node_timeout(entry.target_addr);
            }
            trace!(count = timed_out.len(), "DHT queries timed out");
        }

        // Clean up completed ping tasks
        let bootstrap_done = self
            .active_pings
            .iter()
            .all(|p| p.finished())
            && !self.active_pings.is_empty();
        if bootstrap_done {
            // All bootstrap pings completed (success or failure).
            // C++: After all entry point pings succeed, the bootstrap
            // triggers createNodeLookupTask(localNode_->getID()).
            let all_success = self.active_pings.iter().all(|p| p.is_success());
            if all_success {
                info!("All bootstrap entry point pings succeeded, starting initial node lookup");
                self.lookup_nodes(self.local_id);
            } else {
                debug!("Some bootstrap entry point pings failed, proceeding with partial routing table");
                // Still try a node lookup even if some pings failed
                self.lookup_nodes(self.local_id);
            }
            self.active_pings.clear();
        }

        // Clean up completed replace-node tasks
        let mut replacements_to_apply = Vec::new();
        self.active_replace_tasks.retain(|task| {
            if task.finished() && !task.is_target_alive() {
                // Target node is unresponsive — replace it with the candidate
                replacements_to_apply.push((
                    task.bucket_prefix_len(),
                    task.target_node().id().clone(),
                    task.replacement_node().clone(),
                ));
                false // remove from active list
            } else {
                !task.finished() // keep if not finished
            }
        });

        // Apply replacements: drop the bad node, add the replacement
        for (_prefix_len, target_id, replacement) in replacements_to_apply {
            debug!(
                target_id = %target_id,
                replacement_id = %replacement.id(),
                "Replacing unresponsive DHT node with cached candidate"
            );
            self.routing_table.drop_node(&target_id);
            self.routing_table.add_good_node(replacement);
        }
    }

    /// Handle a successful ping response from a node.
    ///
    /// C++: `DHTPingReplyMessage::receivedAction()` calls
    /// `node->markGood()` and `node->updateLastContact()`. For bootstrap
    /// pings, this is where the real node ID (from the response) replaces
    /// the random placeholder ID.
    fn handle_ping_response(&mut self, sender_addr: SocketAddr, elapsed: std::time::Duration) {
        for ping in &mut self.active_pings {
            if ping.remote_node().addr() == sender_addr && !ping.finished() {
                ping.on_response();
                debug!(
                    addr = %sender_addr,
                    elapsed_ms = elapsed.as_millis(),
                    "Bootstrap ping succeeded"
                );
            }
        }
    }

    /// Handle a ping timeout for active ping tasks.
    ///
    /// C++: `DHTPingTask::onTimeout()` retries up to max_retry times.
    fn handle_ping_timeout(&mut self, timed_out_addr: SocketAddr) {
        // First pass: update ping task state and collect retry addresses
        let mut retry_addrs = Vec::new();
        for ping in &mut self.active_pings {
            if ping.remote_node().addr() == timed_out_addr && !ping.finished() {
                let should_retry = ping.on_timeout();
                if should_retry {
                    retry_addrs.push(timed_out_addr);
                }
            }
        }
        // Second pass: send retries (avoids borrow conflict)
        for addr in retry_addrs {
            self.send_ping(addr);
            trace!(addr = %addr, "Retrying bootstrap ping");
        }
    }

    /// Handle a successful response for an active replace-node task.
    ///
    /// C++: `DHTReplaceNodeTask::onReceived()` marks the target as alive
    /// and finishes the task without replacing.
    fn handle_replace_node_response(&mut self, sender_addr: SocketAddr) {
        for task in &mut self.active_replace_tasks {
            if task.target_node().addr() == sender_addr && !task.finished() {
                task.on_response();
                debug!(
                    addr = %sender_addr,
                    "Replace-node task: questionable node is alive, keeping it"
                );
            }
        }
    }

    /// Handle a timeout for an active replace-node task.
    ///
    /// C++: `DHTReplaceNodeTask::onTimeout()` increments retry count.
    /// After MAX_RETRY timeouts, the target is replaced with the candidate.
    fn handle_replace_node_timeout(&mut self, timed_out_addr: SocketAddr) {
        // First pass: update task state and collect retry addresses
        let mut retry_addrs = Vec::new();
        for task in &mut self.active_replace_tasks {
            if task.target_node().addr() == timed_out_addr && !task.finished() {
                task.on_timeout();
                if !task.finished() {
                    retry_addrs.push(timed_out_addr);
                }
            }
        }
        // Second pass: send retries (avoids borrow conflict)
        for addr in retry_addrs {
            self.send_ping(addr);
            trace!(addr = %addr, "Retrying replace-node ping");
        }
    }

    /// Drive active lookup tasks by sending the next batch of queries.
    ///
    /// For each active lookup, this method:
    /// 1. Gets the next batch of nodes to query from `LookupState::next_query_batch()`
    /// 2. Constructs the appropriate DHT query message (find_node or get_peers)
    /// 3. Queues the message via the dispatcher
    /// 4. Marks the queried entries as "used" in the lookup state
    ///
    /// C++: `DHTAbstractNodeLookupTask::sendMessage()` dispatches queries
    /// via `DHTMessageDispatcher::addMessageToQueue()`.
    async fn drive_active_lookups(&mut self) {
        // Collect completed lookups by removing them from active_lookups.
        // We cannot use drain_filter directly, so we swap the vec and filter.
        let mut remaining = Vec::new();
        let mut completed_results = Vec::new();

        for lookup in std::mem::take(&mut self.active_lookups) {
            if lookup.is_done() {
                completed_results.push(lookup.into_result());
            } else {
                remaining.push(lookup);
            }
        }
        self.active_lookups = remaining;

        // Handle completed lookup results
        for result in completed_results {
            self.handle_lookup_result(result).await;
        }

        // Drive remaining active lookups: send the next batch of queries
        for lookup in &mut self.active_lookups {
            let batch = lookup.state().next_query_batch();
            if batch.is_empty() {
                continue;
            }

            let indices: Vec<usize> = batch.iter().map(|(i, _)| *i).collect();
            let method = match lookup.state().kind() {
                LookupKind::Node => "find_node",
                LookupKind::Peer => "get_peers",
            };

            for (_, node) in &batch {
                let transaction_id = Self::generate_transaction_id();
                let target_addr = node.addr();

                let msg = match lookup.state().kind() {
                    LookupKind::Node => {
                        let target = *lookup.state().target();
                        DhtMessage::FindNodeQuery {
                            transaction_id,
                            sender_id: self.local_id,
                            sender_addr: target_addr,
                            payload: super::message::FindNodeQueryPayload { target },
                        }
                    }
                    LookupKind::Peer => {
                        let info_hash = *lookup.state().target();
                        DhtMessage::GetPeersQuery {
                            transaction_id,
                            sender_id: self.local_id,
                            sender_addr: target_addr,
                            payload: super::message::GetPeersQueryPayload { info_hash },
                        }
                    }
                };

                trace!(
                    method = method,
                    addr = %target_addr,
                    tid = ?msg.transaction_id(),
                    "Dispatching lookup query"
                );

                self.dispatcher.add_message(msg);
            }

            // Mark the queried entries as "used" in the lookup state
            lookup.state_mut().mark_sent(&indices);
        }

        // Send any queued messages
        if self.dispatcher.queue_length() > 0 {
            self.dispatcher.send_messages(&self.transport).await;
        }
    }

    /// Bootstrap from configured entry points.
    ///
    /// C++: `DHTEntryPointNameResolveCommand::execute()` resolves the entry
    /// point hostnames, then calls `addPingTask()` for each resolved address.
    /// The ping task verifies the node is alive and updates its real node ID
    /// from the ping response. Only after successful pings does the bootstrap
    /// proceed to `createNodeLookupTask(localNode_->getID())` and
    /// `createBucketRefreshTask()`.
    ///
    /// In the Rust implementation, we create `DhtPingTask` entries with high
    /// retry counts (matching C++'s 10 retries) and send actual ping messages
    /// via the dispatcher. When the response arrives, `handle_inbound_message()`
    /// will update the node in the routing table with the real ID and RTT.
    async fn bootstrap(&mut self) {
        info!("Bootstrapping DHT from entry points");

        let entry_points = self.config.entry_points.clone();

        for entry in &entry_points {
            // Resolve the entry point hostname
            match tokio::net::lookup_host(format!("{}:{}", entry.host, entry.port)).await {
                Ok(addrs) => {
                    for addr in addrs {
                        info!(addr = %addr, "Pinging DHT entry point before adding");

                        // C++: DHTEntryPointNameResolveCommand::addPingTask()
                        // creates a DHTPingTask with 10 retries. The entry node
                        // starts with a random ID; the real ID comes from the
                        // ping response.
                        let node = DhtNode::with_random_id(addr);
                        let ping_task = DhtPingTask::new(node, 10);
                        self.active_pings.push(ping_task);

                        // Send the ping message immediately
                        self.send_ping(addr);
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

        // Also trigger a bucket refresh to populate the routing table.
        // C++: DHTBucketRefreshCommand creates bucket refresh tasks which
        // schedule node lookups for stale buckets.
        let mut bucket_refresh = DhtBucketRefreshTask::new(true);
        bucket_refresh.compute_targets(&self.routing_table);
        let targets = bucket_refresh.take_targets();
        for target in targets {
            self.lookup_nodes(target);
        }

        self.bootstrapped = true;
    }

    /// Send a DHT ping message to the given address.
    ///
    /// C++: `DHTPingTask::sendMessage()` dispatches a ping via
    /// `DHTMessageDispatcher::addMessageToQueue()`.
    fn send_ping(&mut self, target_addr: SocketAddr) {
        let transaction_id = Self::generate_transaction_id();
        let msg = DhtMessage::PingQuery {
            transaction_id,
            sender_id: self.local_id,
            sender_addr: target_addr,
            payload: super::message::PingQueryPayload,
        };
        self.dispatcher.add_message(msg);
    }

    /// Check if any buckets need refreshing and start node lookups for them.
    ///
    /// C++: `DHTBucketRefreshCommand::execute()` calls
    /// `taskFactory_->createBucketRefreshTask()` which creates a
    /// `DHTBucketRefreshTask`. That task's `startup()` iterates all buckets,
    /// checks `b->needsRefresh()`, calls `b->notifyUpdate()` to reset the
    /// bucket's timer, generates a random target ID within the bucket's range
    /// via `b->getRandomNodeID(targetID)`, then creates a `DHTNodeLookupTask`
    /// for each stale bucket.
    ///
    /// In Rust, we directly create node lookups for stale buckets.
    fn bucket_refresh_check(&mut self) {
        // Collect refresh targets from stale buckets
        let mut targets = Vec::new();
        for bucket in self.routing_table.get_buckets() {
            if bucket.needs_refresh() {
                let target = bucket.random_id_in_range();
                debug!(
                    prefix_len = bucket.common_prefix_len(),
                    nodes = bucket.count(),
                    "Bucket needs refresh, scheduling node lookup"
                );
                targets.push(target);
            }
        }

        // Create node lookups for each stale bucket
        for target in targets {
            self.lookup_nodes(target);
        }

        // Check for buckets with questionable nodes and cached replacements.
        // C++: DHTReplaceNodeTask is triggered when a good node is cached in
        // a full bucket. We also proactively check on bucket refresh.
        self.schedule_replace_node_tasks();
    }

    /// Schedule replace-node tasks for buckets with questionable nodes
    /// and cached replacement candidates.
    ///
    /// C++: When `DHTBucket::cacheNode()` is called, the replace-node task
    /// is created to ping the LRU questionable node. If it doesn't respond,
    /// it's replaced with the cached candidate.
    fn schedule_replace_node_tasks(&mut self) {
        let buckets = self.routing_table.get_buckets();
        let mut tasks_to_create = Vec::new();

        for bucket in &buckets {
            // Only consider buckets with cached candidates
            if bucket.cached_nodes().is_empty() {
                continue;
            }

            // Find the LRU questionable node
            if let Some(target_node) = bucket.lru_questionable_node() {
                // Get the first cached replacement candidate
                if let Some(replacement) = bucket.cached_nodes().front() {
                    tasks_to_create.push((
                        bucket.common_prefix_len(),
                        target_node.clone(),
                        replacement.as_ref().clone(),
                    ));
                }
            }
        }

        // Create replace-node tasks
        for (prefix_len, target, replacement) in tasks_to_create {
            debug!(
                prefix_len = prefix_len,
                target = %target.addr(),
                replacement = %replacement.addr(),
                "Scheduling replace-node task for questionable node"
            );
            let task = DhtReplaceNodeTask::new(prefix_len, target.clone(), replacement);
            self.active_replace_tasks.push(task);

            // Send a ping to the questionable node
            self.send_ping(target.addr());
        }
    }

    /// Check if any local info hashes need re-announcing.
    async fn peer_announce_check(&mut self) {
        // Purge stale peer entries
        self.peer_announce_storage.handle_timeout();

        // Collect info hashes first to avoid immutable borrow conflict
        // when calling self.lookup_peers() (mutable) inside the loop.
        let info_hashes: Vec<NodeId> = self.peer_announce_storage.local_info_hashes().iter().copied().collect();
        for info_hash in info_hashes {
            debug!(info_hash = %info_hash, "Re-announcing local info hash via DHT");
            self.lookup_peers(info_hash);
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
    fn load_or_create_local_id(dht_file: &Path) -> NodeId {
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
        dht_file: &Path,
        routing_table: &mut RoutingTable,
        _family: AddressFamily,
    ) {
        match routing_table_ser::deserialize_from_file(dht_file) {
            Ok(result) => {
                for node in result.nodes {
                    routing_table.add_node(node);
                }
                debug!("Loaded DHT nodes from routing table file");
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

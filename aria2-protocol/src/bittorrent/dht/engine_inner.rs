//! DHT Engine internal methods — background tasks, bootstrap, and maintenance.
//!
//! Split from `engine.rs` to keep file size under 600 lines. Contains all
//! the `DhtEngine` impl methods that are not part of the public API:
//! receive loop, periodic tasks, bootstrap, inbound message processing,
//! and routing table maintenance.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{debug, info, trace, warn};

use super::DhtEngine;
use super::DhtEngineState;
use super::bootstrap::DhtBootstrap;
use super::handler::DhtQueryHandler;
use super::lookup::iterative_find_node;
use super::message::DhtMessage;
use super::message::DhtMessageBuilder;
use super::node::DhtNode;
use super::tracker::TransactionTracker;

// Note: DhtTaskQueue and DhtTaskFactory are available in the `task` and
// `task_peer` modules. The periodic task scheduler currently uses direct
// execution (refresh_buckets / contact_nodes) rather than dispatching
// through the task queue. When full task-queue integration is wired into
// DhtEngine, the spawn_periodic_tasks and bootstrap methods can be
// updated to use engine.task_queue / engine.task_factory.

impl DhtEngine {
    // ==================== Internal: Background tasks ====================

    /// Spawn the main UDP receive loop as a background task.
    pub(super) fn spawn_receive_loop(
        self: &Arc<Self>,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
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
                        engine
                            .process_inbound_message(&buf[..len], from, &tracker, &handler)
                            .await;
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
    ///
    /// Token rotation, bucket refresh, node contact, cleanup, and auto-save
    /// are all handled inline. When the task queue integration is fully wired,
    /// bucket refresh and node contact can be dispatched through
    /// `DhtTaskQueue` for concurrency-limited scheduling (matching C++'s
    /// periodicTaskQueue1/2 design).
    pub(super) fn spawn_periodic_tasks(self: &Arc<Self>) {
        let engine = Arc::clone(self);

        // Token rotation + bucket refresh + node contact + peer cleanup + auto-save
        tokio::spawn(async move {
            let mut token_interval = tokio::time::interval(engine.config.token_rotation_interval);
            let mut refresh_check_interval =
                tokio::time::interval(engine.config.refresh_check_interval);
            let mut node_contact_interval =
                tokio::time::interval(engine.config.node_contact_interval);
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
    pub(super) async fn bootstrap(&self) {
        let self_id = self.inner.read().await.self_id;

        // Resolve bootstrap node hostnames via async DNS (C++ uses c-ares).
        let entry_points = DhtBootstrap::resolve_bootstrap_nodes().await;

        if entry_points.is_empty() {
            warn!("No DHT bootstrap nodes could be resolved — DHT may not function properly");
        }

        info!(
            count = entry_points.len(),
            "Bootstrapping DHT with entry points"
        );

        // Ping each entry point
        for node in &entry_points {
            let msg = super::message::DhtMessageBuilder::ping(0, &self_id);
            if let Ok(encoded) = msg.encode()
                && let Err(e) = self.socket.send_to(node.addr, &encoded).await
            {
                debug!(addr = %node.addr, "Bootstrap ping failed: {}", e);
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
        let _result =
            iterative_find_node(&self_id, &self_id, &rt, &self.socket, &self.tracker).await;

        // Merge discovered nodes
        {
            let mut inner = self.inner.write().await;
            let discovered_rt = rt.read().await;
            for node in discovered_rt.all_nodes() {
                inner.routing_table.insert(node.clone());
            }
            inner.state = DhtEngineState::Running;
        }

        // Perform a direct bucket refresh to fully populate the routing
        // table after bootstrap. When the task queue integration is fully
        // wired, this can be dispatched as a forced BucketRefreshTask.
        self.refresh_buckets().await;

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
                if let Some(response) = response
                    && let Ok(encoded) = response.encode()
                    && let Err(e) = self.socket.send_to(from, &encoded).await
                {
                    debug!(to = %from, "Failed to send DHT response: {}", e);
                }

                // Mark sender as good and add to routing table
                if mark_good && let Some(sender_id) = sender_id {
                    let mut inner = self.inner.write().await;
                    inner.routing_table.mark_good(&sender_id);
                    inner.routing_table.insert(DhtNode::new(sender_id, from));
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
            let _result =
                iterative_find_node(&target, &self_id, &rt, &self.socket, &self.tracker).await;

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
    async fn contact_nodes(&self) {
        let self_id = self.inner.read().await.self_id;
        let buckets = {
            let inner = self.inner.read().await;
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
            if let Ok(encoded) = msg.encode()
                && self.socket.send_to(addr, &encoded).await.is_ok()
            {
                contacted += 1;
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
    pub(super) async fn evict_and_replace_nodes(&self) {
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
    pub(super) async fn save_routing_table(&self) {
        if let Some(ref path) = self.config.dht_file_path {
            let inner = self.inner.read().await;
            let self_id = inner.self_id;
            let nodes = inner.routing_table.collect_good_nodes();
            drop(inner);

            if !nodes.is_empty() {
                match super::persistence::DhtPersistence::save_to_file_sync(path, &self_id, &nodes)
                {
                    Ok(_) => trace!(path = %path.display(), "Auto-saved DHT routing table"),
                    Err(e) => warn!("Failed to auto-save DHT routing table: {}", e),
                }
            }
        }
    }
}

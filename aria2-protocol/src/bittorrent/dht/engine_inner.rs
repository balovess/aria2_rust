//! DHT Engine internal methods — background tasks, bootstrap, and maintenance.
//!
//! Split from `engine.rs` to keep file size under 600 lines. Contains all
//! the `DhtEngine` impl methods that are not part of the public API:
//! receive loop, periodic tasks, bootstrap, inbound message processing,
//! and routing table maintenance.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{debug, info, trace, warn};

use super::DhtEngine;
use super::DhtEngineState;
use super::bootstrap::DhtBootstrap;
use super::engine::DhtEngineContext;
use super::handler::DhtQueryHandler;
use super::lookup::iterative_find_node;
use super::message::DhtMessage;
use super::message::DhtMessageBuilder;
use super::node::DhtNode;
use super::tracker::TransactionTracker;

const INBOUND_QUEUE_CAPACITY: usize = 1024;
const INBOUND_WORKERS: usize = 4;

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
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        let context = Arc::clone(&self.context);
        let socket = context.socket.shared_socket();
        let tracker = Arc::clone(&context.tracker);
        let tracker_notify = tracker.change_notifier();
        let handler_self_id = context.handler_self_id;

        let handle = tokio::spawn(async move {
            let (inbound_tx, inbound_rx) =
                mpsc::channel::<(Vec<u8>, SocketAddr)>(INBOUND_QUEUE_CAPACITY);
            let shared_rx = Arc::new(Mutex::new(inbound_rx));
            let mut workers = tokio::task::JoinSet::new();

            for _ in 0..INBOUND_WORKERS {
                let worker_context = Arc::clone(&context);
                let worker_tracker = Arc::clone(&tracker);
                let worker_rx = Arc::clone(&shared_rx);
                let worker_handler = DhtQueryHandler::new(handler_self_id);
                workers.spawn(async move {
                    loop {
                        let packet = {
                            let mut rx = worker_rx.lock().await;
                            rx.recv().await
                        };
                        let Some((data, from)) = packet else { break };
                        worker_context
                            .process_inbound_message(&data, from, &worker_tracker, &worker_handler)
                            .await;
                    }
                });
            }

            info!("DHT receive loop started");
            let mut buf = [0u8; 4096];

            loop {
                if *shutdown_rx.borrow() {
                    break;
                }

                let timeout_wait = async {
                    match tracker.next_timeout() {
                        Some(timeout) => tokio::time::sleep(timeout).await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::pin!(timeout_wait);
                let transaction_changed = tracker_notify.notified();
                tokio::pin!(transaction_changed);
                transaction_changed.as_mut().enable();

                tokio::select! {
                    result = shutdown_rx.changed() => {
                        if result.is_ok() {
                            info!("DHT receive loop shutting down");
                        }
                        break;
                    }
                    result = socket.recv_from(&mut buf) => {
                        match result {
                            Ok((len, from)) if len > 0 => {
                                match inbound_tx.try_send((buf[..len].to_vec(), from)) {
                                    Ok(()) => {}
                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                        debug!("DHT inbound queue full; dropping packet from {}", from);
                                    }
                                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                                }
                            }
                            Ok(_) => { /* empty packet, ignore */ }
                            Err(e) => {
                                debug!("DHT recv error: {}", e);
                                break;
                            }
                        }
                    }
                    _ = &mut timeout_wait => {}
                    _ = &mut transaction_changed => {
                        continue;
                    }
                }

                let timed_out = tracker.handle_timeouts();
                for (addr, _query_type, node_id) in timed_out {
                    context.handle_timeout(addr, node_id).await;
                }
            }

            drop(inbound_tx);
            let wait_for_workers = async { while workers.join_next().await.is_some() {} };
            if tokio::time::timeout(Duration::from_millis(100), wait_for_workers)
                .await
                .is_err()
            {
                workers.abort_all();
                while workers.join_next().await.is_some() {}
            }
            info!("DHT receive loop exited");
        });
        self.register_background_task(handle);
    }

    /// Spawn periodic maintenance tasks.
    ///
    /// Token rotation, bucket refresh, node contact, cleanup, and auto-save
    /// are all handled inline. When the task queue integration is fully wired,
    /// bucket refresh and node contact can be dispatched through
    /// `DhtTaskQueue` for concurrency-limited scheduling (matching C++'s
    /// periodicTaskQueue1/2 design).
    pub(super) fn spawn_periodic_tasks(self: &Arc<Self>) {
        let context = Arc::clone(&self.context);
        let config = context.config.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        // Token rotation + bucket refresh + node contact + peer cleanup + auto-save
        let handle = tokio::spawn(async move {
            let mut token_interval = tokio::time::interval(config.token_rotation_interval);
            let mut refresh_check_interval = tokio::time::interval(config.refresh_check_interval);
            let mut node_contact_interval = tokio::time::interval(config.node_contact_interval);
            let mut cleanup_interval = tokio::time::interval(Duration::from_secs(300));
            let mut save_interval = tokio::time::interval(Duration::from_secs(1800));

            loop {
                tokio::select! {
                    result = shutdown_rx.changed() => {
                        if result.is_ok() {
                            info!("DHT periodic tasks shutting down");
                        }
                        break;
                    }
                    _ = token_interval.tick() => {
                        let mut tt = context.token_tracker.lock().unwrap_or_else(|e| e.into_inner());
                        tt.maybe_rotate();
                        trace!("DHT token rotation check");
                    }
                    _ = refresh_check_interval.tick() => {
                        context.refresh_buckets().await;
                    }
                    _ = node_contact_interval.tick() => {
                        context.contact_nodes().await;
                    }
                    _ = cleanup_interval.tick() => {
                        context.peer_storage.cleanup_expired();
                        context.tracker.cleanup_expired();
                        context.evict_and_replace_nodes().await;
                        trace!("DHT periodic cleanup");
                    }
                    _ = save_interval.tick() => {
                        context.save_routing_table().await;
                    }
                }
            }
        });
        self.register_background_task(handle);
    }
}

impl DhtEngineContext {
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
            if !self
                .shutdown_requested
                .load(std::sync::atomic::Ordering::Acquire)
            {
                inner.state = DhtEngineState::Running;
            }
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
                let (response, mark_good, sender_id) = {
                    // The handler only borrows the table during synchronous
                    // bencode/routing work. Keep the read guard scoped to this
                    // block instead of cloning every bucket and node for each
                    // inbound packet.
                    let inner = self.inner.read().await;
                    let tt = self.token_tracker.lock().unwrap_or_else(|e| e.into_inner());
                    let result = handler.handle_query(
                        &msg,
                        from,
                        &inner.routing_table,
                        &tt,
                        &self.peer_storage,
                    );
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

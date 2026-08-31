//! DHT Engine internal methods — background tasks, bootstrap, and maintenance.
//!
//! Split from `engine.rs` to keep file size under 600 lines. Contains all
//! the `DhtEngine` impl methods that are not part of the public API:
//! receive loop, periodic tasks, bootstrap, inbound message processing,
//! and routing table maintenance.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use super::DhtEngine;
use super::DhtEngineState;
use super::bootstrap::DhtBootstrap;
use super::engine::DhtEngineContext;
use super::handler::DhtQueryHandler;
use super::message::DhtMessage;
use super::node::DhtNode;
use super::task::DhtTask;
use super::task::DhtTaskQueue;
use super::tracker::{QueryType, TransactionTracker};

const INBOUND_QUEUE_CAPACITY: usize = 1024;
const INBOUND_WORKERS: usize = 4;

#[derive(Clone, Copy, Debug)]
enum MaintenanceKind {
    NodeContact,
    Cleanup,
    SaveRoutingTable,
}

struct MaintenanceTask {
    context: Arc<DhtEngineContext>,
    kind: MaintenanceKind,
}

impl std::fmt::Debug for MaintenanceTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaintenanceTask")
            .field("kind", &self.kind)
            .finish()
    }
}

#[async_trait::async_trait]
impl DhtTask for MaintenanceTask {
    async fn run(self: Box<Self>) {
        match self.kind {
            MaintenanceKind::NodeContact => {
                self.context.contact_nodes().await;
            }
            MaintenanceKind::Cleanup => {
                self.context.peer_storage.cleanup_expired();
                self.context.tracker.cleanup_expired();
                self.context.evict_and_replace_nodes().await;
            }
            MaintenanceKind::SaveRoutingTable => {
                self.context.save_routing_table().await;
            }
        }
    }

    fn name(&self) -> &'static str {
        match self.kind {
            MaintenanceKind::NodeContact => "DhtNodeContactTask",
            MaintenanceKind::Cleanup => "DhtCleanupTask",
            MaintenanceKind::SaveRoutingTable => "DhtSaveRoutingTableTask",
        }
    }
}

fn maintenance_task(context: &Arc<DhtEngineContext>, kind: MaintenanceKind) -> Box<dyn DhtTask> {
    Box::new(MaintenanceTask {
        context: Arc::clone(context),
        kind,
    })
}

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
            // Give each worker its own receiver. Sharing one receiver behind a
            // mutex would keep `recv().await` serialized and make the worker
            // count look larger than the actual processing parallelism.
            let worker_capacity = INBOUND_QUEUE_CAPACITY.div_ceil(INBOUND_WORKERS);
            let mut worker_txs = Vec::with_capacity(INBOUND_WORKERS);
            let mut workers = tokio::task::JoinSet::new();

            for _ in 0..INBOUND_WORKERS {
                let (worker_tx, mut worker_rx) =
                    mpsc::channel::<(Vec<u8>, SocketAddr)>(worker_capacity);
                worker_txs.push(worker_tx);
                let worker_context = Arc::clone(&context);
                let worker_tracker = Arc::clone(&tracker);
                let worker_handler = DhtQueryHandler::new(handler_self_id);
                workers.spawn(async move {
                    while let Some((data, from)) = worker_rx.recv().await {
                        worker_context
                            .process_inbound_message(&data, from, &worker_tracker, &worker_handler)
                            .await;
                    }
                });
            }

            info!("DHT receive loop started");
            let mut buf = [0u8; 4096];
            let mut next_worker = 0usize;

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

                let timeout_elapsed = tokio::select! {
                    result = shutdown_rx.changed() => {
                        if result.is_ok() {
                            info!("DHT receive loop shutting down");
                        }
                        break;
                    }
                    result = socket.recv_from(&mut buf) => {
                        match result {
                            Ok((len, from)) if len > 0 => {
                                let mut packet = (buf[..len].to_vec(), from);
                                let mut dispatched = false;
                                for offset in 0..worker_txs.len() {
                                    let worker_index = (next_worker + offset) % worker_txs.len();
                                    match worker_txs[worker_index].try_send(packet) {
                                        Ok(()) => {
                                            next_worker = (worker_index + 1) % worker_txs.len();
                                            dispatched = true;
                                            break;
                                        }
                                        Err(mpsc::error::TrySendError::Full(returned)) => {
                                            packet = returned;
                                        }
                                        Err(mpsc::error::TrySendError::Closed(returned)) => {
                                            packet = returned;
                                        }
                                    }
                                }
                                if !dispatched {
                                    debug!(
                                        "DHT inbound workers busy; dropping packet from {}",
                                        from
                                    );
                                }
                            }
                            Ok(_) => { /* empty packet, ignore */ }
                            Err(e) => {
                                debug!("DHT recv error: {}", e);
                                break;
                            }
                        }
                        false
                    }
                    _ = &mut timeout_wait => true,
                    _ = &mut transaction_changed => {
                        continue;
                    }
                };

                if timeout_elapsed {
                    let timed_out = tracker.handle_timeouts();
                    if !timed_out.is_empty() {
                        context.handle_timeouts(&timed_out).await;
                    }
                }
            }

            // Closing all worker senders lets workers drain their bounded
            // queues before the shutdown timeout decides whether to abort.
            drop(worker_txs);
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
    /// Periodic maintenance is submitted to the DHT task queue. Timer ticks
    /// are coalesced while the corresponding lane is busy, so a slow lookup
    /// cannot create an unbounded backlog.
    pub(super) fn spawn_periodic_tasks(self: &Arc<Self>) {
        let context = Arc::clone(&self.context);
        let config = context.config.clone();
        let task_queue = Arc::clone(&self.task_queue);
        let task_factory = context.task_factory.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        // Timer ownership stays in this small coordinator; task execution is
        // owned by the two priority lanes in DhtTaskQueue.
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
                        let mut tokens = context
                            .token_tracker
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        tokens.maybe_rotate();
                        trace!("DHT token rotation check");
                    }
                    _ = refresh_check_interval.tick() => {
                        let _ = task_queue.try_add_periodic_task_1_if_idle(
                            task_factory.create_bucket_refresh_task(false),
                        ).await;
                    }
                    _ = node_contact_interval.tick() => {
                        let _ = task_queue.try_add_periodic_task_2_if_idle(
                            maintenance_task(&context, MaintenanceKind::NodeContact),
                        ).await;
                    }
                    _ = cleanup_interval.tick() => {
                        let _ = task_queue.try_add_periodic_task_2_if_idle(
                            maintenance_task(&context, MaintenanceKind::Cleanup),
                        ).await;
                    }
                    _ = save_interval.tick() => {
                        let _ = task_queue.try_add_periodic_task_2_if_idle(
                            maintenance_task(&context, MaintenanceKind::SaveRoutingTable),
                        ).await;
                    }
                }
            }
        });
        self.register_background_task(handle);
    }
}

impl DhtEngineContext {
    pub(super) async fn bootstrap(&self, task_queue: &DhtTaskQueue) {
        // Resolve the public defaults here. Task-specific bootstrap endpoints
        // are resolved by the core configuration seam before engine start.
        let entry_points = if self.config.bootstrap_nodes.is_empty() {
            DhtBootstrap::resolve_bootstrap_nodes().await
        } else {
            self.config
                .bootstrap_nodes
                .iter()
                .map(|addr| DhtNode::new([0u8; 20], *addr))
                .collect()
        };

        if entry_points.is_empty() {
            warn!("No DHT bootstrap nodes could be resolved — DHT may not function properly");
        }

        info!(
            count = entry_points.len(),
            "Bootstrapping DHT with entry points"
        );

        // Add bootstrap endpoints before the first tracked lookup. The lookup
        // itself sends the first ping/find_node requests through the shared
        // transaction tracker, so there is no untracked probe to race with
        // the engine receive loop.
        {
            let mut routing_table = self.routing_table.write().await;
            for node in &entry_points {
                routing_table.insert(node.clone());
            }
        }

        // Queue the first refresh instead of performing a private lookup here.
        // The refresh task uses the same tracker, sole UDP reader, and bounded
        // concurrency as every later maintenance cycle.
        let _ = task_queue
            .add_periodic_task_1(
                self.task_factory
                    .create_bootstrap_refresh_task(self.config.bootstrap_timeout),
            )
            .await;

        // Bootstrap is complete once entry points are installed and the first
        // refresh has been scheduled. The refresh itself continues in the
        // background, so an unreachable DHT cannot block engine startup.
        {
            let mut inner = self.inner.write().await;
            if !self
                .shutdown_requested
                .load(std::sync::atomic::Ordering::Acquire)
            {
                inner.state = DhtEngineState::Running;
            }
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
                let (response, mark_good, sender_id) = {
                    let routing_table = self.routing_table.read().await;
                    let tt = self.token_tracker.lock().unwrap_or_else(|e| e.into_inner());
                    let result =
                        handler.handle_query(&msg, from, &routing_table, &tt, &self.peer_storage);
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
                    let mut routing_table = self.routing_table.write().await;
                    routing_table.mark_good(&sender_id);
                    routing_table.insert(DhtNode::new(sender_id, from));
                }
            }
        }
    }

    /// Handle a batch of timed-out transactions with one routing-table lock.
    async fn handle_timeouts(&self, timed_out: &[(SocketAddr, QueryType, Option<[u8; 20]>)]) {
        let mut routing_table = self.routing_table.write().await;
        for (_, _, node_id) in timed_out {
            if let Some(id) = node_id {
                routing_table.mark_bad(id);
            }
        }
        routing_table.evict_bad_nodes();
    }

    /// Send keep-alive pings to routing table nodes that haven't been
    /// contacted recently.
    async fn contact_nodes(&self) {
        let buckets = {
            let routing_table = self.routing_table.read().await;
            // Need to collect the info we need before releasing the lock.
            let mut nodes = Vec::new();
            for bucket in routing_table.get_all_buckets() {
                if let Some(node) = bucket.nodes().iter().find(|n| n.is_good()) {
                    nodes.push(node.clone());
                }
            }
            nodes
        };

        let task_factory = self.task_factory.clone();
        let contacted = buckets.len();
        futures::stream::iter(buckets)
            .map(|node| {
                let task_factory = task_factory.clone();
                async move {
                    task_factory.create_ping_task(node, 0).run().await;
                }
            })
            .buffer_unordered(16)
            .for_each(|()| async {})
            .await;

        if contacted > 0 {
            trace!(contacted, "DHT node contact keep-alive completed");
        }
    }

    /// Evict bad nodes from the routing table and attempt to replace
    /// questionable nodes with cached candidates.
    ///
    /// Equivalent to C++ periodic `DHTReplaceNodeTask` execution.
    pub(super) async fn evict_and_replace_nodes(&self) {
        let (evicted, replacements) = {
            let mut routing_table = self.routing_table.write().await;
            let evicted = routing_table.evict_bad_nodes();
            let replacements = routing_table
                .get_all_buckets()
                .iter()
                .filter_map(|bucket| {
                    let questionable = bucket.get_lru_questionable_node()?;
                    let replacement = bucket.cached_nodes().first()?.clone();
                    Some((questionable.id, replacement))
                })
                .collect::<Vec<_>>();
            (evicted, replacements)
        };

        let replacement_count = replacements.len();
        let task_factory = self.task_factory.clone();
        futures::stream::iter(replacements)
            .map(|(questionable_node_id, new_node)| {
                let task_factory = task_factory.clone();
                async move {
                    task_factory
                        .create_replace_node_for_node(questionable_node_id, new_node)
                        .run()
                        .await;
                }
            })
            .buffer_unordered(16)
            .for_each(|()| async {})
            .await;

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
            let path = path.clone();
            // Acquire the save lock before taking the snapshot. Otherwise a
            // shutdown snapshot can be newer than an auto-save snapshot but
            // still be written first, allowing the older snapshot to win.
            let save_guard = Arc::clone(&self.routing_table_save_lock).lock_owned().await;
            let self_id = self.inner.read().await.self_id;
            let nodes = self.routing_table.read().await.collect_good_nodes();

            let save_path = path.clone();
            let result = tokio::task::spawn_blocking(move || {
                let _save_guard = save_guard;
                super::persistence::DhtPersistence::save_to_file_sync(&save_path, &self_id, &nodes)
            })
            .await;
            match result {
                Ok(Ok(_)) => trace!(path = %path.display(), "Auto-saved DHT routing table"),
                Ok(Err(e)) => {
                    warn!(path = %path.display(), "Failed to auto-save DHT routing table: {}", e)
                }
                Err(e) => {
                    warn!(path = %path.display(), "DHT routing table save task failed: {}", e)
                }
            }
        }
    }
}

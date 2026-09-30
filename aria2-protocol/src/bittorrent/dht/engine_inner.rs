//! DHT Engine internal methods — background tasks, bootstrap, and maintenance.
//!
//! Split from `engine.rs` to keep file size under 600 lines. Contains all
//! the `DhtEngine` impl methods that are not part of the public API:
//! periodic tasks, bootstrap, and routing table maintenance.

use std::sync::Arc;

use futures::StreamExt;
use tracing::{debug, info, trace, warn};

use super::DhtEngine;
use super::DhtEngineState;
use super::bootstrap::DhtBootstrap;
use super::engine::DhtEngineContext;
use super::task::DhtTask;
use super::task::DhtTaskQueue;
use super::task_impl::{BootstrapRefreshTask, BucketRefreshTask, PingTask};
use super::task_peer::ReplaceNodeTask;

#[derive(Clone, Copy, Debug)]
enum MaintenanceKind {
    NodeContact,
    Cleanup,
    SaveState,
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
                self.context.task_context.tracker.cleanup_expired();
                self.context.evict_and_replace_nodes().await;
            }
            MaintenanceKind::SaveState => {
                if let Err(error) = self.context.save_state().await
                    && self.context.config.dht_file_path.is_some()
                {
                    warn!("Automatic DHT state save failed: {error}");
                }
            }
        }
    }

    fn name(&self) -> &'static str {
        match self.kind {
            MaintenanceKind::NodeContact => "DhtNodeContactTask",
            MaintenanceKind::Cleanup => "DhtCleanupTask",
            MaintenanceKind::SaveState => "DhtSaveStateTask",
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

    /// Spawn periodic maintenance tasks.
    ///
    /// Periodic maintenance is submitted to the DHT task queue. Network
    /// maintenance ticks are coalesced while the lane is busy; persistence
    /// checkpoints are queued so a busy lane cannot silently lose a save.
    pub(super) fn spawn_periodic_tasks(self: &Arc<Self>) {
        let context = Arc::clone(&self.context);
        let config = context.config.clone();
        let task_queue = Arc::clone(&self.task_queue);
        let task_context = context.task_context.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        // Timer ownership stays in this small coordinator; task execution is
        // owned by the independent scheduling lanes in DhtTaskQueue.
        let handle = tokio::spawn(async move {
            let mut token_interval = tokio::time::interval(config.token_rotation_interval);
            let mut refresh_check_interval = tokio::time::interval(config.refresh_check_interval);
            let mut node_contact_interval = tokio::time::interval(config.node_contact_interval);
            let mut cleanup_interval = tokio::time::interval(config.cleanup_interval);
            let mut save_interval = tokio::time::interval(config.save_interval);

            // Bootstrap owns startup network discovery. Consume the
            // immediately-ready first ticks so periodic refresh/contact do
            // not race bootstrap or duplicate its initial queries.
            let _ = refresh_check_interval.tick().await;
            let _ = node_contact_interval.tick().await;

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
                            Box::new(BucketRefreshTask::new(task_context.clone(), false)),
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
                        let _ = task_queue.add_periodic_task_2(
                            maintenance_task(&context, MaintenanceKind::SaveState),
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
            DhtBootstrap::resolve_bootstrap_nodes_for_family(
                self.task_context.socket.local_addr().is_ipv6(),
            )
            .await
        } else {
            DhtBootstrap::nodes_from_addresses(self.config.bootstrap_nodes.iter().copied())
        };

        if entry_points.is_empty() {
            warn!("No DHT bootstrap nodes could be resolved — DHT may not function properly");
        }

        info!(
            count = entry_points.len(),
            "Bootstrapping DHT with entry points"
        );

        // Keep unresolved bootstrap endpoints available for the tracked ping
        // retries below, but do not count them as good or persist them.
        {
            let mut routing_table = self.task_context.routing_table.write().await;
            for node in &entry_points {
                routing_table.insert(node.clone());
            }
        }

        // Ping each entry point with bounded retries before the first bucket
        // refresh, matching aria2's bootstrap handshake. Both phases use the
        // shared transaction tracker and sole UDP reader.
        let _ = task_queue
            .add_periodic_task_1(Box::new(BootstrapRefreshTask::new(
                self.task_context.clone(),
                self.config.bootstrap_timeout,
                entry_points,
            )))
            .await;

        // Bootstrap is complete once entry points are installed and the first
        // refresh has been scheduled. The refresh itself continues in the
        // background, so an unreachable DHT cannot block engine startup.
        let became_ready = {
            let mut inner = self.inner.write().await;
            if !self
                .shutdown_requested
                .load(std::sync::atomic::Ordering::Acquire)
            {
                inner.state = DhtEngineState::Running;
                true
            } else {
                false
            }
        };
        if became_ready {
            let _ = self.state_updates.send(DhtEngineState::Running);
        }

        info!("DHT bootstrap completed");
    }

    /// Send keep-alive pings to routing table nodes that haven't been
    /// contacted recently.
    async fn contact_nodes(&self) {
        let buckets = {
            let routing_table = self.task_context.routing_table.read().await;
            // Need to collect the info we need before releasing the lock.
            let mut nodes = Vec::new();
            for bucket in routing_table.get_all_buckets() {
                if let Some(node) = bucket.nodes().iter().find(|n| n.is_good()) {
                    nodes.push(node.clone());
                }
            }
            nodes
        };

        let task_context = self.task_context.clone();
        let contacted = buckets.len();
        futures::stream::iter(buckets)
            .map(|node| {
                let task_context = task_context.clone();
                async move {
                    Box::new(PingTask::new(task_context, node, 0, None))
                        .run()
                        .await;
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
    pub(super) async fn evict_and_replace_nodes(&self) -> (usize, usize) {
        let (evicted, replacements) = {
            let mut routing_table = self.task_context.routing_table.write().await;
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
        let task_context = self.task_context.clone();
        futures::stream::iter(replacements)
            .map(|(questionable_node_id, new_node)| {
                let task_context = task_context.clone();
                async move {
                    Box::new(ReplaceNodeTask::new(
                        task_context,
                        questionable_node_id,
                        new_node,
                    ))
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

        (evicted, replacement_count)
    }

    /// Save the routing table and BEP 44 store to disk.
    pub(super) async fn save_state(&self) -> Result<(), String> {
        let Some(ref configured_path) = self.config.dht_file_path else {
            return Err("DHT persistence is disabled (dht-file-path is not set)".to_string());
        };
        let path = configured_path.clone();
        // Acquire the save lock before taking the snapshot. Otherwise a
        // shutdown snapshot can be newer than an auto-save snapshot but
        // still be written first, allowing the older snapshot to win.
        let save_guard = Arc::clone(&self.routing_table_save_lock).lock_owned().await;
        let self_id = self.task_context.self_id;
        let nodes = self
            .task_context
            .routing_table
            .read()
            .await
            .collect_good_nodes();

        let save_path = path.clone();
        let routing_result = tokio::task::spawn_blocking(move || {
            let _save_guard = save_guard;
            super::persistence::DhtPersistence::save_to_file_sync(&save_path, &self_id, &nodes)
        })
        .await
        .map_err(|error| format!("DHT routing table save task failed: {error}"))
        .and_then(|result| {
            result
                .map(|_| ())
                .map_err(|error| format!("Failed to save DHT routing table: {error}"))
        });
        if routing_result.is_ok() {
            trace!(path = %path.display(), "Saved DHT routing table");
        }

        // Attempt the BEP 44 save even if writing the routing snapshot failed.
        let item_path = path.with_extension("items");
        let item_save_guard = Arc::clone(&self.routing_table_save_lock).lock_owned().await;
        let save_path = item_path.clone();
        let item_store = self.item_store.clone();
        let item_result = tokio::task::spawn_blocking(move || {
            let _save_guard = item_save_guard;
            item_store.save_to_file_sync(&save_path)
        })
        .await
        .map_err(|error| format!("BEP 44 item store save task failed: {error}"))
        .and_then(|result| {
            result.map_err(|error| format!("Failed to save BEP 44 item store: {error}"))
        });
        if item_result.is_ok() {
            trace!(path = %item_path.display(), "Saved BEP 44 item store");
        }

        match (routing_result, item_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(routing_error), Err(item_error)) => Err(format!("{routing_error}; {item_error}")),
        }
    }
}

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};

use super::{DhtEngine, DhtEngineState, DhtEngineStats};

impl DhtEngine {
    /// Spawn the bootstrap procedure as a background task.
    ///
    /// The task is bounded by [`DhtEngineConfig::bootstrap_timeout`]; on
    /// timeout the engine still transitions to `Running` so that lookups are
    /// not blocked indefinitely by an unreachable network.
    pub(super) async fn spawn_bootstrap(self: &Arc<Self>) {
        if self.context.shutdown_requested.load(Ordering::Acquire) {
            return;
        }

        let context = Arc::clone(&self.context);
        let task_queue = Arc::clone(&self.task_queue);
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let limit = context.config.bootstrap_timeout;
        self.register_background_task(async move {
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
                    let _ = context.state_updates.send(DhtEngineState::Running);
                }
            };

            tokio::select! {
                _ = bootstrap => {}
                _ = shutdown_rx.changed() => {}
            }
        })
        .await;
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

    /// Subscribe to lifecycle state transitions without polling [`Self::state`].
    ///
    /// The receiver always contains the latest state. Consumers should read
    /// [`watch::Receiver::borrow`] for the current snapshot and await
    /// [`watch::Receiver::changed`] for the next transition.
    pub fn subscribe_state(&self) -> watch::Receiver<DhtEngineState> {
        self.context.state_updates.subscribe()
    }

    /// Wait until bootstrap has reached a usable state without polling.
    ///
    /// A running engine may still have an empty routing table when the public
    /// network is unreachable; this method only waits for the lifecycle
    /// transition and does not promise that a peer lookup will succeed.
    pub async fn wait_until_ready(&self, timeout: Duration) -> std::io::Result<()> {
        let mut updates = self.context.state_updates.subscribe();
        let wait = async {
            loop {
                match self.state().await {
                    DhtEngineState::Running => return Ok(()),
                    DhtEngineState::ShuttingDown | DhtEngineState::Stopped => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "DHT engine is not running",
                        ));
                    }
                    DhtEngineState::Bootstrapping => updates.changed().await.map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "DHT engine state updates are closed",
                        )
                    })?,
                }
            }
        };

        tokio::time::timeout(timeout, wait).await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "DHT bootstrap did not become ready before the timeout",
            )
        })?
    }

    /// Synchronous shutdown — sets engine state to `ShuttingDown`.
    pub fn shutdown(&self) {
        let first_shutdown = !self.context.shutdown_requested.swap(true, Ordering::AcqRel);

        if first_shutdown {
            let _ = self.shutdown_tx.send(true);

            if let Ok(mut inner) = self.context.inner.try_write() {
                inner.state = DhtEngineState::ShuttingDown;
                let _ = self
                    .context
                    .state_updates
                    .send(DhtEngineState::ShuttingDown);
            }

            info!("DHT shutdown signal sent");
        }
    }

    /// Async shutdown — signals the engine to stop and awaits full teardown.
    pub async fn shutdown_async(&self) {
        self.shutdown();

        self.task_queue.shutdown().await;

        // Give tasks a bounded opportunity to observe the shared signal before
        // aborting a maintenance operation that is currently awaiting network
        // I/O. Keep the JoinSet in the engine owner while awaiting it: a
        // cancelled shutdown future then releases the mutex without detaching
        // its tasks, and a later shutdown can resume draining the same set.
        let mut join_set = self.background_tasks.lock().await;
        let wait_for_tasks = async { while join_set.join_next().await.is_some() {} };
        if tokio::time::timeout(Duration::from_millis(100), wait_for_tasks)
            .await
            .is_err()
        {
            join_set.abort_all();
            while join_set.join_next().await.is_some() {}
        }

        self.context.inner.write().await.state = DhtEngineState::ShuttingDown;
        let _ = self
            .context
            .state_updates
            .send(DhtEngineState::ShuttingDown);

        if self.context.config.dht_file_path.is_some()
            && let Err(error) = self.context.save_state().await
        {
            warn!("DHT shutdown save failed: {error}");
        }

        info!("DHT engine shutdown complete");
    }

    /// Return a snapshot of DHT engine statistics.
    pub async fn stats(&self) -> DhtEngineStats {
        let inner = self.context.inner.read().await;
        let routing_table = self.context.task_context.routing_table.read().await;
        let state = if self.context.shutdown_requested.load(Ordering::Acquire) {
            DhtEngineState::ShuttingDown
        } else {
            inner.state
        };
        DhtEngineStats {
            total_nodes: routing_table.total_node_count(),
            good_nodes: routing_table.good_node_count(),
            pending_transactions: self.context.task_context.tracker.pending_count(),
            questionable_nodes: routing_table.questionable_node_count(),
            bad_nodes: routing_table.bad_node_count(),
            cached_nodes: routing_table
                .get_all_buckets()
                .iter()
                .map(|bucket| bucket.cached_nodes().len())
                .sum(),
            bucket_count: routing_table.num_buckets(),
            persistence_enabled: self.context.config.dht_file_path.is_some(),
            persistence_max_age_secs: self.context.config.persistence_max_age.as_secs(),
            cleanup_interval_secs: self.context.config.cleanup_interval.as_secs(),
            save_interval_secs: self.context.config.save_interval.as_secs(),
            state,
        }
    }

    /// Register a background task owned by this engine.
    pub(in crate::bittorrent::dht) async fn register_background_task<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut background_tasks = self.background_tasks.lock().await;
        if !self.context.shutdown_requested.load(Ordering::Acquire) {
            background_tasks.spawn(task);
        }
    }
}

impl Drop for DhtEngine {
    fn drop(&mut self) {
        self.task_queue.cancel();
        self.background_tasks.get_mut().abort_all();
    }
}

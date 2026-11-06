use std::fmt;

use super::executor::DhtTaskExecutor;
use super::{BoxedDhtTask, DEFAULT_NUM_CONCURRENT};
// DhtTaskQueue — three-lane task queue
// ---------------------------------------------------------------------------

/// Three independent scheduling lanes for DHT work.
///
/// - **Periodic 1**: Bucket refresh and bootstrap lookup tasks.
/// - **Periodic 2**: Keep-alive pings and maintenance tasks.
/// - **Immediate**: On-demand tasks triggered by user actions (announce,
///   peer lookup, etc.).
///
/// Each lane has its own concurrency limit, so periodic work cannot consume
/// the execution slots assigned to immediate tasks.
pub struct DhtTaskQueue {
    /// Periodic lane 1: bucket refresh and bootstrap lookup.
    periodic_executor_1: DhtTaskExecutor,

    /// Periodic lane 2: keep-alive pings and maintenance.
    periodic_executor_2: DhtTaskExecutor,

    /// Immediate lane: on-demand user tasks.
    immediate_executor: DhtTaskExecutor,
}

impl DhtTaskQueue {
    /// Create a new task queue with the default concurrency limit.
    pub fn new() -> Self {
        Self::with_concurrency(DEFAULT_NUM_CONCURRENT)
    }

    /// Create a new task queue with a custom concurrency limit.
    pub fn with_concurrency(num_concurrent: usize) -> Self {
        Self {
            periodic_executor_1: DhtTaskExecutor::new(num_concurrent),
            periodic_executor_2: DhtTaskExecutor::new(num_concurrent),
            immediate_executor: DhtTaskExecutor::new(num_concurrent),
        }
    }

    /// Add a task to periodic lane 1 (bucket refresh and bootstrap lookup).
    pub async fn add_periodic_task_1(&self, task: BoxedDhtTask) -> bool {
        self.periodic_executor_1.add_task(task).await
    }

    /// Enqueue periodic lane-one work only when that lane is idle.
    pub async fn try_add_periodic_task_1_if_idle(&self, task: BoxedDhtTask) -> bool {
        self.periodic_executor_1.try_add_task_if_idle(task).await
    }

    /// Add a task to periodic lane 2 (keep-alive and maintenance).
    pub async fn add_periodic_task_2(&self, task: BoxedDhtTask) -> bool {
        self.periodic_executor_2.add_task(task).await
    }

    /// Enqueue periodic lane-two work only when that lane is idle.
    pub async fn try_add_periodic_task_2_if_idle(&self, task: BoxedDhtTask) -> bool {
        self.periodic_executor_2.try_add_task_if_idle(task).await
    }

    /// Add an immediate (on-demand) task.
    pub async fn add_immediate_task(&self, task: BoxedDhtTask) -> bool {
        self.immediate_executor.add_task(task).await
    }

    /// Cancel all queued and running work in every lane.
    pub async fn shutdown(&self) {
        tokio::join!(
            self.periodic_executor_1.shutdown(),
            self.periodic_executor_2.shutdown(),
            self.immediate_executor.shutdown(),
        );
    }

    /// Signal cancellation for all lanes without waiting for task teardown.
    pub fn cancel(&self) {
        self.periodic_executor_1.cancel();
        self.periodic_executor_2.cancel();
        self.immediate_executor.cancel();
    }
}

impl Default for DhtTaskQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for DhtTaskQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DhtTaskQueue").finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

//! DHT Task scheduling — async task executor with concurrency-limited queues.
//!
//! This module provides the Rust equivalent of the C++ DHT task scheduling
//! system (`DHTTask`, `DHTTaskExecutor`, `DHTTaskQueue`, `DHTTaskFactory`).
//!
//! Key differences from the C++ design:
//! - C++ uses synchronous `startup()` + callback-based `finished()` checks.
//!   Rust uses `async fn run()` that returns when the task completes.
//! - C++ polls `update()` every event-loop iteration. Rust uses `tokio::spawn`
//!   with a semaphore for concurrency control, so tasks run independently.
//! - C++ has three fixed priority executors (periodicTaskQueue1/2 + immediate).
//!   Rust preserves the same three-lane design via `DhtTaskQueue`.
//!
//! Architecture:
//! ```text
//! DhtTaskQueue
//!   ├── periodic_executor_1: DhtTaskExecutor (bucket refresh, node lookup)
//!   ├── periodic_executor_2: DhtTaskExecutor (keep-alive pings)
//!   └── immediate_executor:  DhtTaskExecutor (announce, on-demand lookup)
//!
//! DhtTaskExecutor
//!   ├── queue: VecDeque<BoxedDhtTask>   (pending tasks)
//!   └── semaphore: Semaphore(num_concurrent) (limits in-flight tasks)
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

// ---------------------------------------------------------------------------
// DhtTask trait
// ---------------------------------------------------------------------------

/// Core trait for all DHT tasks.
///
/// Equivalent to C++ `DHTTask` with `startup()` and `finished()`, but
/// expressed as a single `async fn run()` that completes when the task
/// is done. The executor manages concurrency and scheduling externally.
#[async_trait::async_trait]
pub trait DhtTask: Send + fmt::Debug {
    /// Execute the task to completion.
    ///
    /// Implementations should perform their work and return when finished.
    /// The executor ensures that no more than `num_concurrent` tasks run
    /// simultaneously within each priority lane.
    async fn run(self: Box<Self>);

    /// Human-readable task name for logging.
    fn name(&self) -> &'static str;
}

/// Type-erased boxed DHT task.
pub type BoxedDhtTask = Box<dyn DhtTask>;

// ---------------------------------------------------------------------------
// DhtTaskExecutor
// ---------------------------------------------------------------------------

/// Default concurrency limit per executor lane (matches C++ `NUM_CONCURRENT_TASK = 15`).
pub const DEFAULT_NUM_CONCURRENT: usize = 15;

/// A concurrency-limited executor for DHT tasks.
///
/// Equivalent to C++ `DHTTaskExecutor`. Tasks are queued and dispatched
/// up to `num_concurrent` at a time. When a task finishes, the next
/// queued task is started.
///
/// Unlike the C++ version which is polled synchronously via `update()`,
/// this Rust version uses a `tokio::sync::Semaphore` and spawns tasks
/// via `tokio::spawn` so that they run concurrently without blocking
/// the executor's dispatch loop.
pub struct DhtTaskExecutor {
    /// Shared state protected by an async mutex.
    inner: Arc<Mutex<DhtTaskExecutorInner>>,
    /// Semaphore controlling maximum concurrency.
    semaphore: Arc<Semaphore>,
    /// Maximum concurrent tasks.
    num_concurrent: usize,
    /// Cancels queued and running work when the owning DHT engine shuts down.
    shutdown: CancellationToken,
    /// Wakes shutdown waiters after the last running task leaves the executor.
    idle_notify: Arc<Notify>,
}

struct DhtTaskExecutorInner {
    /// FIFO queue of pending tasks.
    queue: VecDeque<BoxedDhtTask>,
    /// Number of currently executing tasks.
    executing: usize,
    /// Maximum number of tasks observed waiting in the queue.
    peak_queue_size: usize,
}

impl DhtTaskExecutor {
    /// Create a new executor with the given concurrency limit.
    pub fn new(num_concurrent: usize) -> Self {
        let num_concurrent = num_concurrent.max(1);
        Self {
            inner: Arc::new(Mutex::new(DhtTaskExecutorInner {
                queue: VecDeque::new(),
                executing: 0,
                peak_queue_size: 0,
            })),
            semaphore: Arc::new(Semaphore::new(num_concurrent)),
            num_concurrent,
            shutdown: CancellationToken::new(),
            idle_notify: Arc::new(Notify::new()),
        }
    }

    /// Enqueue a task for execution.
    ///
    /// If there is capacity, the task is dispatched immediately.
    /// Otherwise it waits in the FIFO queue until a slot opens.
    pub async fn add_task(&self, task: BoxedDhtTask) -> bool {
        if self.shutdown.is_cancelled() {
            return false;
        }
        let task_name = task.name();
        let mut inner = self.inner.lock().await;
        if self.shutdown.is_cancelled() {
            return false;
        }
        inner.queue.push_back(task);
        inner.peak_queue_size = inner.peak_queue_size.max(inner.queue.len());
        trace!(
            task = task_name,
            queue_len = inner.queue.len(),
            executing = inner.executing,
            "DHT task enqueued"
        );
        drop(inner);

        // Try to dispatch pending tasks.
        self.dispatch_pending().await;
        true
    }

    /// Enqueue work only when this executor is idle.
    ///
    /// Periodic producers use this operation to coalesce a timer tick with
    /// work that is already running or waiting. This keeps maintenance work
    /// bounded when a network operation takes longer than its interval.
    pub async fn try_add_task_if_idle(&self, task: BoxedDhtTask) -> bool {
        if self.shutdown.is_cancelled() {
            return false;
        }

        let task_name = task.name();
        let mut inner = self.inner.lock().await;
        if self.shutdown.is_cancelled() {
            return false;
        }
        if inner.executing != 0 || !inner.queue.is_empty() {
            return false;
        }
        inner.queue.push_back(task);
        inner.peak_queue_size = inner.peak_queue_size.max(inner.queue.len());
        trace!(task = task_name, "DHT idle periodic task enqueued");
        drop(inner);

        self.dispatch_pending().await;
        true
    }

    /// Number of currently executing tasks.
    pub async fn executing_count(&self) -> usize {
        self.inner.lock().await.executing
    }

    /// Number of tasks waiting in the queue.
    pub async fn queue_size(&self) -> usize {
        self.inner.lock().await.queue.len()
    }

    /// Maximum number of tasks that have waited in this executor's queue.
    pub async fn peak_queue_size(&self) -> usize {
        self.inner.lock().await.peak_queue_size
    }

    /// Try to dispatch as many queued tasks as the semaphore allows.
    async fn dispatch_pending(&self) {
        if self.shutdown.is_cancelled() {
            return;
        }
        let sem = Arc::clone(&self.semaphore);
        loop {
            let task = {
                let mut inner = self.inner.lock().await;
                if inner.queue.is_empty() {
                    break;
                }
                inner.queue.pop_front()
            };

            let Some(task) = task else { break };
            let task_name = task.name();

            // Try non-blocking permit acquisition on the owned semaphore.
            match sem.clone().try_acquire_owned() {
                Ok(permit) => {
                    let mut inner = self.inner.lock().await;
                    inner.executing += 1;
                    drop(inner);

                    debug!(task = task_name, "DHT task dispatched");

                    // Spawn the task, holding the owned permit until done.
                    self.spawn_task(task, permit);
                }
                Err(_) => {
                    // At capacity — re-queue the task at the front and stop.
                    let mut inner = self.inner.lock().await;
                    if !self.shutdown.is_cancelled() {
                        inner.queue.push_front(task);
                    }
                    break;
                }
            }
        }
    }

    /// Run one task while converting a task panic into a completed task.
    ///
    /// The executor must release its permit and decrement `executing` even
    /// when a task contains an unexpected panic; otherwise shutdown and all
    /// later dispatches can wait forever on stale executor state.
    async fn run_task(task: BoxedDhtTask, shutdown: &CancellationToken) {
        let task_name = task.name();
        let result = AssertUnwindSafe(async {
            tokio::select! {
                _ = shutdown.cancelled() => {}
                _ = task.run() => {}
            }
        })
        .catch_unwind()
        .await;

        if result.is_err() {
            warn!(task = task_name, "DHT task panicked; executor continues");
        }
    }

    /// Spawn a task on the tokio runtime, holding the owned semaphore permit
    /// for the task's lifetime. When the task completes, the permit is
    /// automatically released (via `Drop`), allowing the next queued task
    /// to be dispatched.
    fn spawn_task(&self, task: BoxedDhtTask, _permit: OwnedSemaphorePermit) {
        let inner = Arc::clone(&self.inner);
        let semaphore = Arc::clone(&self.semaphore);
        let shutdown = self.shutdown.clone();
        let idle_notify = Arc::clone(&self.idle_notify);

        // The core task runner: runs the task, then re-dispatches.
        // This function returns a Future that the spawner awaits.
        let run_and_redispatch = async move {
            // Hold the permit for the duration of the task.
            let _held = _permit;

            Self::run_task(task, &shutdown).await;

            // Task completed — update executing count.
            {
                let mut guard = inner.lock().await;
                guard.executing = guard.executing.saturating_sub(1);
            }
            idle_notify.notify_one();

            // Release the permit so the next task can start.
            drop(_held);

            // Re-dispatch any pending tasks now that a slot is free.
            loop {
                if shutdown.is_cancelled() {
                    let mut guard = inner.lock().await;
                    guard.queue.clear();
                    break;
                }

                let next_task = {
                    let mut guard = inner.lock().await;
                    guard.queue.pop_front()
                };

                let Some(next_task) = next_task else { break };

                match semaphore.clone().try_acquire_owned() {
                    Ok(permit) => {
                        {
                            let mut guard = inner.lock().await;
                            guard.executing += 1;
                        }
                        debug!(
                            task = next_task.name(),
                            "DHT task dispatched (after completion)"
                        );

                        // Recursively run the next task in this same
                        // coroutine. This avoids spawning unlimited
                        // tasks and ensures the chain continues.
                        // The permit is held across the recursive call.
                        let _held2 = permit;
                        Self::run_task(next_task, &shutdown).await;

                        {
                            let mut guard = inner.lock().await;
                            guard.executing = guard.executing.saturating_sub(1);
                        }
                        idle_notify.notify_one();
                        drop(_held2);
                        // Loop continues — try to dispatch more.
                    }
                    Err(_) => {
                        // No permits available — re-queue and stop.
                        let mut guard = inner.lock().await;
                        if !shutdown.is_cancelled() {
                            guard.queue.push_front(next_task);
                        }
                        break;
                    }
                }
            }
        };

        tokio::spawn(run_and_redispatch);
    }

    /// Cancel running work and discard queued work.
    pub async fn shutdown(&self) {
        self.cancel();
        self.inner.lock().await.queue.clear();

        loop {
            let notified = self.idle_notify.notified();
            if self.executing_count().await == 0 {
                break;
            }
            notified.await;
        }
    }

    /// Signal cancellation without waiting for asynchronous task teardown.
    pub fn cancel(&self) {
        self.shutdown.cancel();
    }
}

impl fmt::Debug for DhtTaskExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DhtTaskExecutor")
            .field("num_concurrent", &self.num_concurrent)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// DhtTaskQueue — three-lane priority queue
// ---------------------------------------------------------------------------

/// Three-lane DHT task queue, equivalent to C++ `DHTTaskQueueImpl`.
///
/// - **Periodic 1**: Bucket refresh and node lookup tasks (higher priority).
/// - **Periodic 2**: Keep-alive pings and maintenance tasks.
/// - **Immediate**: On-demand tasks triggered by user actions (announce,
///   peer lookup, etc.).
///
/// Each lane has its own `DhtTaskExecutor` with an independent concurrency
/// limit, so that immediate tasks are never starved by periodic maintenance.
pub struct DhtTaskQueue {
    /// Periodic lane 1: bucket refresh, node lookup (C++ periodicTaskQueue1_).
    periodic_executor_1: DhtTaskExecutor,

    /// Periodic lane 2: keep-alive pings, maintenance (C++ periodicTaskQueue2_).
    periodic_executor_2: DhtTaskExecutor,

    /// Immediate lane: on-demand user tasks (C++ immediateTaskQueue_).
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

    /// Add a task to periodic lane 1 (bucket refresh, node lookup).
    ///
    /// Equivalent to C++ `DHTTaskQueueImpl::addPeriodicTask1()`.
    pub async fn add_periodic_task_1(&self, task: BoxedDhtTask) -> bool {
        self.periodic_executor_1.add_task(task).await
    }

    /// Enqueue periodic lane-one work only when that lane is idle.
    pub async fn try_add_periodic_task_1_if_idle(&self, task: BoxedDhtTask) -> bool {
        self.periodic_executor_1.try_add_task_if_idle(task).await
    }

    /// Add a task to periodic lane 2 (keep-alive, maintenance).
    ///
    /// Equivalent to C++ `DHTTaskQueueImpl::addPeriodicTask2()`.
    pub async fn add_periodic_task_2(&self, task: BoxedDhtTask) -> bool {
        self.periodic_executor_2.add_task(task).await
    }

    /// Enqueue periodic lane-two work only when that lane is idle.
    pub async fn try_add_periodic_task_2_if_idle(&self, task: BoxedDhtTask) -> bool {
        self.periodic_executor_2.try_add_task_if_idle(task).await
    }

    /// Add an immediate (on-demand) task.
    ///
    /// Equivalent to C++ `DHTTaskQueueImpl::addImmediateTask()`.
    pub async fn add_immediate_task(&self, task: BoxedDhtTask) -> bool {
        self.immediate_executor.add_task(task).await
    }

    /// Reference to periodic executor 1 (for direct access if needed).
    pub fn periodic_executor_1(&self) -> &DhtTaskExecutor {
        &self.periodic_executor_1
    }

    /// Reference to periodic executor 2.
    pub fn periodic_executor_2(&self) -> &DhtTaskExecutor {
        &self.periodic_executor_2
    }

    /// Reference to immediate executor.
    pub fn immediate_executor(&self) -> &DhtTaskExecutor {
        &self.immediate_executor
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A trivial test task that increments a counter.
    #[derive(Debug)]
    struct CountTask {
        counter: Arc<AtomicUsize>,
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl DhtTask for CountTask {
        async fn run(self: Box<Self>) {
            self.counter.fetch_add(1, Ordering::SeqCst);
        }

        fn name(&self) -> &'static str {
            self.name
        }
    }

    #[tokio::test]
    async fn test_executor_dispatches_task() {
        let executor = DhtTaskExecutor::new(2);
        let counter = Arc::new(AtomicUsize::new(0));

        executor
            .add_task(Box::new(CountTask {
                counter: Arc::clone(&counter),
                name: "test",
            }))
            .await;

        // Give the spawned task time to complete.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_executor_concurrency_limit() {
        let executor = DhtTaskExecutor::new(2);
        let counter = Arc::new(AtomicUsize::new(0));

        // Enqueue 5 tasks — only 2 should execute concurrently.
        for _ in 0..5 {
            executor
                .add_task(Box::new(CountTask {
                    counter: Arc::clone(&counter),
                    name: "concurrent-test",
                }))
                .await;
        }

        // Wait for all tasks to complete.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert_eq!(counter.load(Ordering::SeqCst), 5);
        assert_eq!(executor.executing_count().await, 0);
    }

    #[tokio::test]
    async fn test_task_queue_three_lanes() {
        let queue = DhtTaskQueue::new();
        let c1 = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::new(AtomicUsize::new(0));
        let c3 = Arc::new(AtomicUsize::new(0));

        queue
            .add_periodic_task_1(Box::new(CountTask {
                counter: Arc::clone(&c1),
                name: "p1",
            }))
            .await;
        queue
            .add_periodic_task_2(Box::new(CountTask {
                counter: Arc::clone(&c2),
                name: "p2",
            }))
            .await;
        queue
            .add_immediate_task(Box::new(CountTask {
                counter: Arc::clone(&c3),
                name: "imm",
            }))
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(c1.load(Ordering::SeqCst), 1);
        assert_eq!(c2.load(Ordering::SeqCst), 1);
        assert_eq!(c3.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_executor_queue_size() {
        let executor = DhtTaskExecutor::new(1);
        let counter = Arc::new(AtomicUsize::new(0));

        // A slow task that holds the slot.
        #[derive(Debug)]
        struct SlowTask {
            counter: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl DhtTask for SlowTask {
            async fn run(self: Box<Self>) {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                self.counter.fetch_add(1, Ordering::SeqCst);
            }

            fn name(&self) -> &'static str {
                "slow"
            }
        }

        executor
            .add_task(Box::new(SlowTask {
                counter: Arc::clone(&counter),
            }))
            .await;

        // Add more tasks while the first is running.
        for _ in 0..3 {
            executor
                .add_task(Box::new(CountTask {
                    counter: Arc::clone(&counter),
                    name: "queued",
                }))
                .await;
        }

        // Should have 1 executing + some queued.
        let executing = executor.executing_count().await;
        let queued = executor.queue_size().await;
        assert!(
            executing + queued >= 3,
            "executing={}, queued={}",
            executing,
            queued
        );

        // Wait for all to finish.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn test_executor_coalesces_periodic_work_while_busy() {
        let executor = DhtTaskExecutor::new(1);
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let counter = Arc::new(AtomicUsize::new(0));

        #[derive(Debug)]
        struct BlockingTask {
            started: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
            counter: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl DhtTask for BlockingTask {
            async fn run(self: Box<Self>) {
                self.counter.fetch_add(1, Ordering::SeqCst);
                self.started.notify_one();
                self.release.notified().await;
            }

            fn name(&self) -> &'static str {
                "blocking"
            }
        }

        executor
            .add_task(Box::new(BlockingTask {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
                counter: Arc::clone(&counter),
            }))
            .await;
        started.notified().await;

        assert!(
            !executor
                .try_add_task_if_idle(Box::new(CountTask {
                    counter: Arc::clone(&counter),
                    name: "coalesced",
                }))
                .await
        );
        assert_eq!(executor.queue_size().await, 0);

        release.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_executor_shutdown_cancels_running_task() {
        let executor = DhtTaskExecutor::new(1);
        let started = Arc::new(tokio::sync::Notify::new());
        let counter = Arc::new(AtomicUsize::new(0));

        #[derive(Debug)]
        struct NeverEndingTask {
            started: Arc<tokio::sync::Notify>,
            counter: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl DhtTask for NeverEndingTask {
            async fn run(self: Box<Self>) {
                self.counter.fetch_add(1, Ordering::SeqCst);
                self.started.notify_one();
                std::future::pending::<()>().await;
            }

            fn name(&self) -> &'static str {
                "never-ending"
            }
        }

        executor
            .add_task(Box::new(NeverEndingTask {
                started: Arc::clone(&started),
                counter: Arc::clone(&counter),
            }))
            .await;
        started.notified().await;

        executor.shutdown().await;
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(executor.executing_count().await, 0);
        assert!(
            !executor
                .try_add_task_if_idle(Box::new(CountTask {
                    counter: Arc::clone(&counter),
                    name: "after-shutdown",
                }))
                .await
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_executor_does_not_stall_after_task_panic() {
        #[derive(Debug)]
        struct PanicTask {
            started: Arc<tokio::sync::Notify>,
        }

        #[async_trait::async_trait]
        impl DhtTask for PanicTask {
            async fn run(self: Box<Self>) {
                self.started.notify_one();
                panic!("intentional task panic");
            }

            fn name(&self) -> &'static str {
                "panic"
            }
        }

        let executor = DhtTaskExecutor::new(1);
        let started = Arc::new(tokio::sync::Notify::new());
        assert!(
            executor
                .add_task(Box::new(PanicTask {
                    started: Arc::clone(&started),
                }))
                .await
        );
        started.notified().await;

        let shutdown =
            tokio::time::timeout(std::time::Duration::from_millis(500), executor.shutdown()).await;
        assert!(shutdown.is_ok(), "executor shutdown should not stall");
        assert_eq!(executor.executing_count().await, 0);
    }
}

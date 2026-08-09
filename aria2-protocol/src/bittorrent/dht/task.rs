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
use std::sync::Arc;

use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, trace};

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
}

struct DhtTaskExecutorInner {
    /// FIFO queue of pending tasks.
    queue: VecDeque<BoxedDhtTask>,
    /// Number of currently executing tasks.
    executing: usize,
}

impl DhtTaskExecutor {
    /// Create a new executor with the given concurrency limit.
    pub fn new(num_concurrent: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DhtTaskExecutorInner {
                queue: VecDeque::new(),
                executing: 0,
            })),
            semaphore: Arc::new(Semaphore::new(num_concurrent)),
            num_concurrent,
        }
    }

    /// Enqueue a task for execution.
    ///
    /// If there is capacity, the task is dispatched immediately.
    /// Otherwise it waits in the FIFO queue until a slot opens.
    pub async fn add_task(&self, task: BoxedDhtTask) {
        let task_name = task.name();
        let mut inner = self.inner.lock().await;
        inner.queue.push_back(task);
        trace!(
            task = task_name,
            queue_len = inner.queue.len(),
            executing = inner.executing,
            "DHT task enqueued"
        );
        drop(inner);

        // Try to dispatch pending tasks.
        self.dispatch_pending().await;
    }

    /// Number of currently executing tasks.
    pub async fn executing_count(&self) -> usize {
        self.inner.lock().await.executing
    }

    /// Number of tasks waiting in the queue.
    pub async fn queue_size(&self) -> usize {
        self.inner.lock().await.queue.len()
    }

    /// Try to dispatch as many queued tasks as the semaphore allows.
    async fn dispatch_pending(&self) {
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
                    inner.queue.push_front(task);
                    break;
                }
            }
        }
    }

    /// Spawn a task on the tokio runtime, holding the owned semaphore permit
    /// for the task's lifetime. When the task completes, the permit is
    /// automatically released (via `Drop`), allowing the next queued task
    /// to be dispatched.
    fn spawn_task(&self, task: BoxedDhtTask, _permit: OwnedSemaphorePermit) {
        let inner = Arc::clone(&self.inner);
        let semaphore = Arc::clone(&self.semaphore);

        // The core task runner: runs the task, then re-dispatches.
        // This function returns a Future that the spawner awaits.
        let run_and_redispatch = async move {
            // Hold the permit for the duration of the task.
            let _held = _permit;

            task.run().await;

            // Task completed — update executing count.
            {
                let mut guard = inner.lock().await;
                guard.executing = guard.executing.saturating_sub(1);
            }

            // Release the permit so the next task can start.
            drop(_held);

            // Re-dispatch any pending tasks now that a slot is free.
            loop {
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
                        next_task.run().await;

                        {
                            let mut guard = inner.lock().await;
                            guard.executing = guard.executing.saturating_sub(1);
                        }
                        drop(_held2);
                        // Loop continues — try to dispatch more.
                    }
                    Err(_) => {
                        // No permits available — re-queue and stop.
                        let mut guard = inner.lock().await;
                        guard.queue.push_front(next_task);
                        break;
                    }
                }
            }
        };

        tokio::spawn(run_and_redispatch);
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
    pub async fn add_periodic_task_1(&self, task: BoxedDhtTask) {
        self.periodic_executor_1.add_task(task).await;
    }

    /// Add a task to periodic lane 2 (keep-alive, maintenance).
    ///
    /// Equivalent to C++ `DHTTaskQueueImpl::addPeriodicTask2()`.
    pub async fn add_periodic_task_2(&self, task: BoxedDhtTask) {
        self.periodic_executor_2.add_task(task).await;
    }

    /// Add an immediate (on-demand) task.
    ///
    /// Equivalent to C++ `DHTTaskQueueImpl::addImmediateTask()`.
    pub async fn add_immediate_task(&self, task: BoxedDhtTask) {
        self.immediate_executor.add_task(task).await;
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
}

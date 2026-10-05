use std::collections::VecDeque;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};
// DhtTask trait
// ---------------------------------------------------------------------------

/// Core trait for all DHT tasks.
///
/// The executor manages concurrency and scheduling; implementations define
/// the work performed by a task.
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait DhtTask: Send + fmt::Debug {
    /// Execute the task to completion.
    ///
    /// Implementations should perform their work and return when finished.
    /// The executor ensures that no more than `num_concurrent` tasks run
    /// simultaneously within its lane.
    async fn run(self: Box<Self>);

    /// Human-readable task name for logging.
    fn name(&self) -> &'static str;
}

/// Type-erased boxed DHT task.
pub type BoxedDhtTask = Box<dyn DhtTask>;

// ---------------------------------------------------------------------------
// DhtTaskExecutor
// ---------------------------------------------------------------------------

/// Default concurrency limit per DHT task lane.
pub const DEFAULT_NUM_CONCURRENT: usize = 15;

/// Concurrency-limited implementation used by each DHT task lane.
///
/// Tasks are queued and dispatched up to `num_concurrent` at a time. When a
/// task finishes, the next queued task starts.
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

    /// Return the maximum number of tasks this executor runs concurrently.
    pub fn concurrency_limit(&self) -> usize {
        self.num_concurrent
    }

    /// Return whether this executor has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.shutdown.is_cancelled()
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

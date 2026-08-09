//! Task executor and task queue with priority levels.

use std::collections::VecDeque;

use super::DhtTask;

// -- TaskExecutor -------------------------------------------------------------

/// Executes a queue of DHT tasks with a concurrency limit.
///
/// C++: `DHTTaskExecutor` - manages running and pending tasks.
pub struct TaskExecutor {
    /// Maximum number of concurrent tasks.
    max_concurrent: usize,
    /// Currently executing tasks.
    running: Vec<Box<dyn DhtTask>>,
    /// Pending tasks.
    queue: VecDeque<Box<dyn DhtTask>>,
}

impl TaskExecutor {
    /// Create a new executor with the given concurrency limit.
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent,
            running: Vec::new(),
            queue: VecDeque::new(),
        }
    }

    /// Add a task to the pending queue.
    pub fn add_task(&mut self, task: Box<dyn DhtTask>) {
        self.queue.push_back(task);
    }

    /// Tick the executor: start new tasks and remove finished ones.
    pub fn update(&mut self) {
        // Remove finished tasks
        self.running.retain(|t| !t.finished());

        // Start new tasks up to the concurrency limit
        while self.running.len() < self.max_concurrent {
            let Some(mut task) = self.queue.pop_front() else {
                break;
            };
            task.startup();
            self.running.push(task);
        }
    }

    /// Number of currently executing tasks.
    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    /// Number of pending tasks.
    pub fn queue_size(&self) -> usize {
        self.queue.len()
    }
}

// -- DhtTaskQueue ------------------------------------------------------------

/// The DHT task queue with three priority levels.
///
/// C++: `DHTTaskQueueImpl` - immediate, periodic1, periodic2 queues.
///
/// - **Immediate**: one-shot tasks like peer lookups triggered by user action
/// - **Periodic1**: bucket refresh and similar periodic maintenance
/// - **Periodic2**: peer announcement and other lower-priority periodic tasks
pub struct DhtTaskQueue {
    /// High-priority immediate tasks.
    immediate: TaskExecutor,
    /// Periodic maintenance tasks (bucket refresh, etc.).
    periodic1: TaskExecutor,
    /// Lower-priority periodic tasks (peer announce, etc.).
    periodic2: TaskExecutor,
}

impl DhtTaskQueue {
    /// Create a new task queue with default concurrency limits.
    pub fn new() -> Self {
        Self {
            immediate: TaskExecutor::new(15),
            periodic1: TaskExecutor::new(5),
            periodic2: TaskExecutor::new(5),
        }
    }

    /// Add an immediate (one-shot) task.
    pub fn add_immediate(&mut self, task: Box<dyn DhtTask>) {
        self.immediate.add_task(task);
    }

    /// Add a periodic1 (bucket refresh) task.
    pub fn add_periodic1(&mut self, task: Box<dyn DhtTask>) {
        self.periodic1.add_task(task);
    }

    /// Add a periodic2 (peer announce) task.
    pub fn add_periodic2(&mut self, task: Box<dyn DhtTask>) {
        self.periodic2.add_task(task);
    }

    /// Execute one tick of all task queues.
    pub fn execute(&mut self) {
        self.immediate.update();
        self.periodic1.update();
        self.periodic2.update();
    }

    /// Number of tasks across all queues.
    pub fn total_tasks(&self) -> usize {
        self.immediate.running_count()
            + self.immediate.queue_size()
            + self.periodic1.running_count()
            + self.periodic1.queue_size()
            + self.periodic2.running_count()
            + self.periodic2.queue_size()
    }
}

impl Default for DhtTaskQueue {
    fn default() -> Self {
        Self::new()
    }
}

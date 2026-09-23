//! Bounded process-wide workers for blocking filesystem and CPU work.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::error::{Aria2Error, Result};

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A bounded queue serviced by a fixed set of dedicated blocking threads.
///
/// Callers await queue capacity and completion without occupying a Tokio
/// worker. Panics are contained to one job and reported to its caller.
pub(crate) struct BlockingWorkerPool {
    name: &'static str,
    sender: mpsc::Sender<Job>,
}

impl BlockingWorkerPool {
    pub(crate) fn new(name: &'static str, workers: usize, queue_capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel::<Job>(queue_capacity.max(1));
        let receiver = Arc::new(Mutex::new(receiver));

        for index in 0..workers.max(1) {
            let receiver = Arc::clone(&receiver);
            let thread_name = format!("{name}-{index}");
            std::thread::Builder::new()
                .name(thread_name)
                .spawn(move || {
                    loop {
                        let job = receiver
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .blocking_recv();
                        let Some(job) = job else {
                            break;
                        };
                        let _ = catch_unwind(AssertUnwindSafe(job));
                    }
                })
                .expect("failed to start blocking worker thread");
        }

        Self { name, sender }
    }

    pub(crate) async fn run<T, F>(&self, operation: F, context: &'static str) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let (result_sender, result_receiver) = oneshot::channel();
        self.sender
            .send(Box::new(move || {
                let result = operation();
                let _ = result_sender.send(result);
            }))
            .await
            .map_err(|error| {
                Aria2Error::Io(format!("{} worker queue closed: {error}", self.name))
            })?;

        result_receiver
            .await
            .map_err(|error| Aria2Error::Io(format!("{context} worker task failed: {error}")))?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::BlockingWorkerPool;

    #[tokio::test]
    async fn blocking_operation_runs_on_a_dedicated_thread() {
        let pool = BlockingWorkerPool::new("test-disk", 1, 1);
        let runtime_thread = std::thread::current().id();

        let worker_thread = pool
            .run(|| Ok(std::thread::current().id()), "worker thread test")
            .await
            .unwrap();

        assert_ne!(worker_thread, runtime_thread);
    }

    #[tokio::test]
    async fn full_queue_backpressures_and_cancelled_sender_does_not_run() {
        let pool = Arc::new(BlockingWorkerPool::new("test-bounded", 1, 1));
        let started = Arc::new(Notify::new());
        let (release_worker, wait_for_release) = std::sync::mpsc::channel();

        let first_pool = Arc::clone(&pool);
        let first_started = Arc::clone(&started);
        let first = tokio::spawn(async move {
            first_pool
                .run(
                    move || {
                        first_started.notify_one();
                        wait_for_release.recv().unwrap();
                        Ok(())
                    },
                    "first blocking operation",
                )
                .await
        });

        started.notified().await;

        let second_pool = Arc::clone(&pool);
        let second =
            tokio::spawn(async move { second_pool.run(|| Ok(()), "queued operation").await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.sender.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the queued operation should occupy the bounded queue");

        let cancelled_operation_ran = Arc::new(AtomicBool::new(false));
        let third_started = Arc::new(Notify::new());
        let third_pool = Arc::clone(&pool);
        let third_ran = Arc::clone(&cancelled_operation_ran);
        let third_started_signal = Arc::clone(&third_started);
        let mut third = tokio::spawn(async move {
            third_started_signal.notify_one();
            third_pool
                .run(
                    move || {
                        third_ran.store(true, Ordering::SeqCst);
                        Ok(())
                    },
                    "cancelled operation",
                )
                .await
        });
        third_started.notified().await;

        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut third)
                .await
                .is_err(),
            "a full queue should keep the next sender pending"
        );
        third.abort();
        let _ = third.await;

        release_worker.send(()).unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert!(!cancelled_operation_ran.load(Ordering::SeqCst));
    }
}

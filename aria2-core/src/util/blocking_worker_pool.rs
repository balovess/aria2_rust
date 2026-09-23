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

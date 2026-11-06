//! DHT task scheduling through independent periodic and on-demand lanes.
//!
//! Tasks run asynchronously. Each lane bounds its own concurrent work and
//! cancels queued and running tasks during engine shutdown.

mod executor;
mod queue;
#[cfg(test)]
mod tests;

pub use executor::{BoxedDhtTask, DEFAULT_NUM_CONCURRENT, DhtTask, DhtTaskExecutor};
pub use queue::DhtTaskQueue;

//! Shared bounded worker pool for CPU-heavy digest and piece verification.

use std::sync::OnceLock;

use crate::util::blocking_worker_pool::BlockingWorkerPool;

const MAX_HASH_WORKERS: usize = 4;
const HASH_QUEUE_CAPACITY: usize = 8;

pub(crate) fn shared() -> &'static BlockingWorkerPool {
    static POOL: OnceLock<BlockingWorkerPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, MAX_HASH_WORKERS);
        BlockingWorkerPool::new("aria2-hash", workers, HASH_QUEUE_CAPACITY)
    })
}

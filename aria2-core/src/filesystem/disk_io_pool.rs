//! Shared bounded worker pool for file operations used by download protocols.

use std::sync::OnceLock;

use crate::util::blocking_worker_pool::BlockingWorkerPool;

const MAX_DISK_IO_WORKERS: usize = 8;
const DISK_IO_QUEUE_CAPACITY: usize = 16;

pub(crate) fn shared() -> &'static BlockingWorkerPool {
    static POOL: OnceLock<BlockingWorkerPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(2, MAX_DISK_IO_WORKERS);
        BlockingWorkerPool::new("aria2-disk-io", workers, DISK_IO_QUEUE_CAPACITY)
    })
}

use std::path::{Path, PathBuf};

use tokio::sync::oneshot;

use crate::error::Result;
use crate::filesystem::file_allocation::AllocationStrategy;

use super::cancelled_error;
use super::dispatch::SharedFileAllocationMan;
use super::queue::FileAllocationEntry;

/// Queue single-file allocation and wait until the worker finishes it.
pub async fn enqueue_path(
    manager: &SharedFileAllocationMan,
    path: &Path,
    length: u64,
    strategy: AllocationStrategy,
    secure_falloc: bool,
    gid: u64,
) -> Result<()> {
    if length == 0 || strategy == AllocationStrategy::None {
        return Ok(());
    }

    let (sender, receiver) = oneshot::channel();
    let entry = FileAllocationEntry::single(
        gid,
        path.to_path_buf(),
        length,
        strategy,
        secure_falloc,
        sender,
    );
    manager.write().await.push_entry(entry);
    receiver.await.map_err(|_| cancelled_error())?
}

/// Queue multi-file allocation and wait until every file is allocated.
pub async fn enqueue_multi(
    manager: &SharedFileAllocationMan,
    files: Vec<(PathBuf, u64)>,
    strategy: AllocationStrategy,
    secure_falloc: bool,
    gid: u64,
) -> Result<()> {
    if files.is_empty() || strategy == AllocationStrategy::None {
        return Ok(());
    }

    let (sender, receiver) = oneshot::channel();
    let entry = FileAllocationEntry::multi(gid, files, strategy, secure_falloc, sender);
    manager.write().await.push_entry(entry);
    receiver.await.map_err(|_| cancelled_error())?
}

pub(crate) async fn cancel_gid(manager: &SharedFileAllocationMan, gid: u64) -> usize {
    manager.write().await.cancel_gid(gid)
}

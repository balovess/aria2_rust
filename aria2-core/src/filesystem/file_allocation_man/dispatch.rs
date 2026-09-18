use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::disk_adaptor::{DirectDiskAdaptor, DiskAdaptor};
use crate::filesystem::file_allocation::{self, AllocationStrategy};
use crate::filesystem::file_allocation_iterator::{
    AdaptiveFileAllocationIterator, FileAllocationIterator,
};

use super::cancelled_error;
use super::queue::{AllocationKind, FileAllocationEntry, FileAllocationMan};

/// Thread-safe allocation manager shared by the engine and download commands.
pub type SharedFileAllocationMan = Arc<RwLock<FileAllocationMan>>;

/// Return the process-wide allocation manager and start its worker once.
pub fn shared() -> SharedFileAllocationMan {
    static SHARED: OnceLock<SharedFileAllocationMan> = OnceLock::new();
    static WORKER: OnceLock<()> = OnceLock::new();

    let manager = SHARED
        .get_or_init(|| Arc::new(RwLock::new(FileAllocationMan::new())))
        .clone();

    let worker_manager = Arc::clone(&manager);
    WORKER.get_or_init(move || {
        std::thread::Builder::new()
            .name("aria2-file-allocation".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("file allocation worker runtime must initialize");
                runtime.block_on(worker_loop(worker_manager));
            })
            .expect("file allocation worker thread must start");
    });

    manager
}

#[cfg(test)]
pub(super) fn test_manager() -> SharedFileAllocationMan {
    let manager = Arc::new(RwLock::new(FileAllocationMan::new()));
    tokio::spawn(worker_loop(Arc::clone(&manager)));
    manager
}

async fn worker_loop(manager: SharedFileAllocationMan) {
    debug!("File allocation worker started");
    loop {
        let notifier = { manager.read().await.queue_notifier() };
        let notified = notifier.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let entry = {
            let mut guard = manager.write().await;
            guard.take_next_owned()
        };

        let Some(mut entry) = entry else {
            notified.await;
            continue;
        };

        let result = run_entry_allocation(&mut entry).await;
        manager.write().await.drop_picked();
        if let Some(sender) = entry.done_tx.take() {
            let _ = sender.send(result);
        }
    }
}

async fn run_entry_allocation(entry: &mut FileAllocationEntry) -> Result<()> {
    let started = Instant::now();
    let gid = entry.gid;

    if entry.is_cancelled() {
        return Err(cancelled_error());
    }

    let result = match &entry.kind {
        AllocationKind::Path { path, length } => {
            if *length == 0 || entry.strategy == AllocationStrategy::None {
                return Ok(());
            }
            ensure_parent_dir(path).await?;
            check_disk_space(path, *length).await?;
            if entry.is_cancelled() {
                return Err(cancelled_error());
            }
            allocate_single_file(path, *length, entry).await
        }
        AllocationKind::Multi { files } => {
            if entry.strategy == AllocationStrategy::None {
                return Ok(());
            }
            for (path, length) in files {
                if *length == 0 {
                    continue;
                }
                ensure_parent_dir(path).await?;
                if entry.is_cancelled() {
                    return Err(cancelled_error());
                }
                allocate_single_file(path, *length, entry).await?;
            }
            Ok(())
        }
    };

    match &result {
        Ok(()) => info!(
            gid,
            elapsed_secs = started.elapsed().as_secs_f64(),
            "File allocation done"
        ),
        Err(error) => warn!(gid, error = %error, "File allocation failed"),
    }
    result
}

async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| Aria2Error::Io(error.to_string()))?;
    }
    Ok(())
}

async fn check_disk_space(path: &Path, length: u64) -> Result<()> {
    if file_allocation::check_disk_space_async(path, length)
        .await
        .is_err()
    {
        return Err(Aria2Error::Fatal(FatalError::DiskSpaceExhausted));
    }
    Ok(())
}

async fn current_size(path: &Path) -> u64 {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata.len(),
        Err(_) => 0,
    }
}

async fn allocate_single_file(path: &Path, length: u64, entry: &FileAllocationEntry) -> Result<()> {
    if entry.is_cancelled() {
        return Err(cancelled_error());
    }

    let offset = current_size(path).await;
    if offset >= length {
        debug!(path = %path.display(), "Skipping allocation, file already large enough");
        return Ok(());
    }

    let mut adaptor = DirectDiskAdaptor::new();
    adaptor.open(path).await?;
    let mut adaptor = Some(adaptor);

    let allocation_result: Result<()> = async {
        match entry.strategy {
            AllocationStrategy::Prealloc => {
                let mut iterator = AdaptiveFileAllocationIterator::new_with_secure_falloc(
                    adaptor.take().expect("open adaptor is present"),
                    offset,
                    length,
                    entry.secure_falloc,
                );
                let mut allocation_error = None;
                while !iterator.finished() {
                    if entry.is_cancelled() {
                        allocation_error = Some(cancelled_error());
                        break;
                    }
                    if let Err(error) = iterator.allocate_chunk().await {
                        allocation_error = Some(error);
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                adaptor = Some(iterator.into_inner());
                if let Some(error) = allocation_error {
                    return Err(error);
                }
                Ok(())
            }
            AllocationStrategy::Trunc => {
                if entry.is_cancelled() {
                    return Err(cancelled_error());
                }
                adaptor
                    .as_mut()
                    .expect("open adaptor is present")
                    .truncate(length)
                    .await?;
                if entry.is_cancelled() {
                    return Err(cancelled_error());
                }
                Ok(())
            }
            AllocationStrategy::Falloc | AllocationStrategy::Mmap => {
                if entry.is_cancelled() {
                    return Err(cancelled_error());
                }
                file_allocation::allocate_file(
                    adaptor.as_mut().expect("open adaptor is present"),
                    path,
                    length,
                    AllocationStrategy::Falloc,
                    entry.secure_falloc,
                )
                .await?;
                if entry.is_cancelled() {
                    return Err(cancelled_error());
                }
                Ok(())
            }
            AllocationStrategy::None => Ok(()),
        }
    }
    .await;

    let close_result = match adaptor.as_mut() {
        Some(adaptor) => adaptor.close().await,
        None => Ok(()),
    };
    match (allocation_result, close_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

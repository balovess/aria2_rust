use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Notify, oneshot};
use tracing::debug;

use crate::error::Result;
use crate::filesystem::file_allocation::AllocationStrategy;

use super::cancelled_error;

/// A single path or a multi-file layout to allocate.
#[derive(Debug, Clone)]
pub(crate) enum AllocationKind {
    Path { path: PathBuf, length: u64 },
    Multi { files: Vec<(PathBuf, u64)> },
}

/// A queued allocation request.
pub(crate) struct FileAllocationEntry {
    pub(crate) gid: u64,
    pub(crate) kind: AllocationKind,
    pub(crate) strategy: AllocationStrategy,
    pub(crate) secure_falloc: bool,
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) done_tx: Option<oneshot::Sender<Result<()>>>,
}

impl FileAllocationEntry {
    pub(crate) fn single(
        gid: u64,
        path: PathBuf,
        length: u64,
        strategy: AllocationStrategy,
        secure_falloc: bool,
        done_tx: oneshot::Sender<Result<()>>,
    ) -> Self {
        Self {
            gid,
            kind: AllocationKind::Path { path, length },
            strategy,
            secure_falloc,
            cancelled: Arc::new(AtomicBool::new(false)),
            done_tx: Some(done_tx),
        }
    }

    pub(crate) fn multi(
        gid: u64,
        files: Vec<(PathBuf, u64)>,
        strategy: AllocationStrategy,
        secure_falloc: bool,
        done_tx: oneshot::Sender<Result<()>>,
    ) -> Self {
        Self {
            gid,
            kind: AllocationKind::Multi { files },
            strategy,
            secure_falloc,
            cancelled: Arc::new(AtomicBool::new(false)),
            done_tx: Some(done_tx),
        }
    }

    pub(crate) fn mark_cancelled(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

struct PickedMeta {
    gid: u64,
    cancelled: Arc<AtomicBool>,
}

impl PickedMeta {
    fn from_entry(entry: &FileAllocationEntry) -> Self {
        Self {
            gid: entry.gid,
            cancelled: Arc::clone(&entry.cancelled),
        }
    }
}

/// Queue for sequential file allocation.
pub struct FileAllocationMan {
    queue: VecDeque<FileAllocationEntry>,
    queue_notify: Arc<Notify>,
    picked: Option<PickedMeta>,
}

impl FileAllocationMan {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            queue_notify: Arc::new(Notify::new()),
            picked: None,
        }
    }

    pub(crate) fn push_entry(&mut self, entry: FileAllocationEntry) {
        debug!(
            gid = entry.gid,
            queue_len = self.queue.len(),
            "File allocation entry queued"
        );
        self.queue.push_back(entry);
        self.queue_notify.notify_one();
    }

    pub(super) fn queue_notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.queue_notify)
    }

    pub(super) fn take_next_owned(&mut self) -> Option<FileAllocationEntry> {
        let entry = self.queue.pop_front()?;
        debug!(gid = entry.gid, "File allocation entry picked by worker");
        self.picked = Some(PickedMeta::from_entry(&entry));
        Some(entry)
    }

    pub(super) fn drop_picked(&mut self) {
        self.picked = None;
    }

    #[cfg(test)]
    pub(crate) fn is_picked(&self) -> bool {
        self.picked.is_some()
    }

    #[cfg(test)]
    pub(crate) fn has_next(&self) -> bool {
        !self.queue.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn count_in_queue(&self) -> usize {
        self.queue.len()
    }

    #[cfg(test)]
    pub(crate) fn active_count(&self) -> usize {
        usize::from(self.picked.is_some())
    }

    #[cfg(test)]
    pub(crate) fn total_entries(&self) -> usize {
        self.queue.len() + self.active_count()
    }

    #[cfg(test)]
    pub(crate) fn is_picked_gid(&self, gid: u64) -> bool {
        self.picked.as_ref().is_some_and(|meta| meta.gid == gid)
    }

    #[cfg(test)]
    pub(crate) fn is_queued_gid(&self, gid: u64) -> bool {
        self.queue.iter().any(|entry| entry.gid == gid)
    }

    #[cfg(test)]
    pub(crate) fn cancel_all(&mut self) {
        for entry in self.queue.drain(..) {
            entry.mark_cancelled();
            if let Some(sender) = entry.done_tx {
                let _ = sender.send(Err(cancelled_error()));
            }
        }
        if let Some(meta) = self.picked.as_ref() {
            meta.cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn cancel_gid(&mut self, gid: u64) -> usize {
        let mut cancelled = 0;
        let mut retained = VecDeque::with_capacity(self.queue.len());

        while let Some(entry) = self.queue.pop_front() {
            if entry.gid == gid {
                entry.mark_cancelled();
                if let Some(sender) = entry.done_tx {
                    let _ = sender.send(Err(cancelled_error()));
                }
                cancelled += 1;
            } else {
                retained.push_back(entry);
            }
        }
        self.queue = retained;

        if let Some(meta) = self.picked.as_ref().filter(|meta| meta.gid == gid) {
            meta.cancelled.store(true, Ordering::Relaxed);
            cancelled += 1;
        }

        cancelled
    }
}

impl Default for FileAllocationMan {
    fn default() -> Self {
        Self::new()
    }
}

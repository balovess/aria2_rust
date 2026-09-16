use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use tokio::sync::{Notify, oneshot};
use tracing::debug;

use crate::error::Result;

use super::cancelled_error;
use super::tasks::{CheckIntegrityTask, IntegrityOutcome};

// CheckIntegrityEntry / CheckIntegrityMan
// ---------------------------------------------------------------------------

/// Lightweight metadata of the entry currently being validated.
#[derive(Debug, Clone)]
struct PickedMeta {
    gid: u64,
    total_length: u64,
    cancelled: Arc<AtomicBool>,
    progress: Arc<std::sync::atomic::AtomicU64>,
}

/// A single integrity-check request in the queue.
pub struct CheckIntegrityEntry {
    /// Group ID for logging and lookups.
    pub gid: u64,
    /// The validation task to drive.
    pub task: Box<dyn CheckIntegrityTask>,
    /// Time when this entry was created (for logging elapsed time).
    pub created_at: Instant,
    /// Set when cancelled (engine halt); checked between chunks.
    cancelled: Arc<AtomicBool>,
    /// Progress shared with the dispatcher while validation runs.
    progress: Arc<std::sync::atomic::AtomicU64>,
    /// Completion notification: `Ok(true)` verified, `Ok(false)` mismatch,
    /// `Err` I/O failure or cancellation.
    done_tx: Option<oneshot::Sender<Result<IntegrityOutcome>>>,
}

impl CheckIntegrityEntry {
    pub(super) fn new(
        gid: u64,
        task: Box<dyn CheckIntegrityTask>,
        done_tx: oneshot::Sender<Result<IntegrityOutcome>>,
    ) -> Self {
        Self {
            gid,
            task,
            created_at: Instant::now(),
            cancelled: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            done_tx: Some(done_tx),
        }
    }

    fn mark_cancelled(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub(super) fn progress(&self) -> &std::sync::atomic::AtomicU64 {
        &self.progress
    }

    pub(super) fn take_done_sender(&mut self) -> Option<oneshot::Sender<Result<IntegrityOutcome>>> {
        self.done_tx.take()
    }
}

/// Sequential check-integrity manager (mirrors C++ `SequentialPicker<...>`).
pub struct CheckIntegrityMan {
    queue: VecDeque<CheckIntegrityEntry>,
    picked: Option<PickedMeta>,
    max_concurrent: usize,
    active_count: usize,
    wake: Arc<Notify>,
}

impl CheckIntegrityMan {
    /// Create a new manager (sequential by default, matching C++).
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            picked: None,
            max_concurrent: 1,
            active_count: 0,
            wake: Arc::new(Notify::new()),
        }
    }

    /// Create a manager with the given concurrency limit.
    pub fn with_concurrency(max_concurrent: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            picked: None,
            max_concurrent: max_concurrent.max(1),
            active_count: 0,
            wake: Arc::new(Notify::new()),
        }
    }

    /// Push a new entry to the back of the queue.
    pub fn push_entry(&mut self, entry: CheckIntegrityEntry) {
        debug!(
            gid = entry.gid,
            queue_len = self.queue.len(),
            "Integrity check entry queued"
        );
        self.queue.push_back(entry);
        self.wake.notify_one();
    }

    /// Pick the next entry, moving it out. `None` when the queue is empty or
    /// the concurrency limit is reached.
    pub fn take_next_owned(&mut self) -> Option<CheckIntegrityEntry> {
        if self.active_count >= self.max_concurrent {
            return None;
        }
        let mut entry = self.queue.pop_front()?;
        self.active_count += 1;
        debug!(gid = entry.gid, "Integrity check entry picked by worker");
        let total = entry.task.total_length();
        self.picked = Some(PickedMeta {
            gid: entry.gid,
            total_length: total,
            cancelled: Arc::clone(&entry.cancelled),
            progress: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        });
        entry.progress = Arc::clone(&self.picked.as_ref().expect("picked metadata").progress);
        Some(entry)
    }

    /// Drop the currently picked entry (worker finished with it).
    pub fn drop_picked(&mut self) {
        if self.picked.take().is_some() {
            self.active_count = self.active_count.saturating_sub(1);
            self.wake.notify_one();
        }
    }

    /// Whether an entry is currently being validated.
    pub fn is_picked(&self) -> bool {
        self.picked.is_some()
    }

    /// Whether there are entries waiting in the queue.
    pub fn has_next(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Number of entries waiting in the queue.
    pub fn count_in_queue(&self) -> usize {
        self.queue.len()
    }

    /// Number of currently active validations.
    pub fn active_count(&self) -> usize {
        self.active_count
    }

    /// Progress of the currently active validation, `(current, total)`.
    pub fn current_progress(&self) -> Option<(u64, u64)> {
        self.picked.as_ref().map(|m| {
            (
                m.progress
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .min(m.total_length),
                m.total_length,
            )
        })
    }

    /// Cancel every queued entry (notify waiters) and mark the active one.
    pub fn cancel_all(&mut self) {
        for entry in self.queue.drain(..) {
            entry.mark_cancelled();
            if let Some(tx) = entry.done_tx {
                let _ = tx.send(Err(cancelled_error()));
            }
        }
        if let Some(meta) = self.picked.as_ref() {
            meta.cancelled.store(true, Ordering::Relaxed);
        }
        self.wake.notify_one();
    }

    /// Cancel integrity work belonging to one RequestGroup.
    ///
    /// Queued entries are removed and their waiters are notified immediately.
    /// A picked entry is marked for cooperative cancellation; the worker
    /// observes the flag between validation chunks and still owns its final
    /// cleanup and completion notification.
    pub fn cancel_gid(&mut self, gid: u64) -> bool {
        let mut cancelled = false;
        let mut retained = VecDeque::with_capacity(self.queue.len());

        while let Some(entry) = self.queue.pop_front() {
            if entry.gid == gid {
                entry.mark_cancelled();
                if let Some(tx) = entry.done_tx {
                    let _ = tx.send(Err(cancelled_error()));
                }
                cancelled = true;
            } else {
                retained.push_back(entry);
            }
        }
        self.queue = retained;

        if let Some(meta) = self.picked.as_ref()
            && meta.gid == gid
        {
            meta.cancelled.store(true, Ordering::Relaxed);
            cancelled = true;
        }

        if cancelled {
            self.wake.notify_one();
        }
        cancelled
    }

    pub(super) fn wake_notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }
}

impl Default for CheckIntegrityMan {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------

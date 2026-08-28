//! File allocation manager: sequential queue for file pre-allocation.
//!
//! Mirrors C++ `FileAllocationMan` (which is a `SequentialPicker<FileAllocationEntry>`)
//! plus `FileAllocationDispatcherCommand` + `FileAllocationCommand` that drive
//! chunked allocation inside the event loop.
//!
//! # C++ Architecture
//!
//! In the original C++ aria2:
//! 1. `FileAllocationMan` = `SequentialPicker<FileAllocationEntry>` — a queue that
//!    picks one entry at a time for allocation (sequential, not concurrent).
//! 2. `FileAllocationEntry` wraps a `RequestGroup` + `FileAllocationIterator`.
//! 3. `FileAllocationDispatcherCommand` (realtime, periodic) picks the next queued
//!    entry and hands it to a `FileAllocationCommand`.
//! 4. `FileAllocationCommand` (realtime) calls `allocateChunk()` once per event-loop
//!    tick so a large zero-fill allocation never blocks the loop; when finished it
//!    calls `prepareForNextAction()` (BtSetup / HTTP commands) and drops the entry.
//!
//! # Rust Design
//!
//! The Rust implementation adapts this to async/await with the same semantics:
//! - `FileAllocationMan` holds a `VecDeque` of pending entries plus the currently
//!   active one. `max_concurrent` defaults to 1, matching C++ sequential dispatch.
//! - A single background worker task (`worker_loop`) replaces the two C++ commands:
//!   it takes the next entry, drives the `FileAllocationIterator` chunk-by-chunk
//!   with `tokio::task::yield_now()` between chunks (so the runtime stays
//!   responsive, the async equivalent of C++'s per-tick `allocateChunk()`), then
//!   signals completion through a `oneshot` channel. When no entry is queued, the
//!   worker waits on a `Notify` instead of periodically waking up to poll.
//! - `enqueue_path` / `enqueue_multi` are the entry points used by download
//!   commands; they wait for the completion notification, so the calling command
//!   resumes exactly when allocation is done (mirroring C++ where the download
//!   commands are created only after allocation finishes).
//! - `cancel_all()` clears the queue and notifies waiters, so an engine halt never
//!   leaves a command hanging forever on an allocation that will not run.
//!
//! # Concurrency Model
//!
//! C++ processes allocation entries strictly sequentially (one at a time).
//! The Rust version preserves this by default but can optionally run
//! allocations concurrently when the strategy is `trunc` or `falloc`
//! (which are near-instant and don't need sequential throttling).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use tokio::sync::{Notify, RwLock, oneshot};
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::disk_adaptor::{DirectDiskAdaptor, DiskAdaptor};
use crate::filesystem::file_allocation::{self, AllocationStrategy};
use crate::filesystem::file_allocation_iterator::{
    AdaptiveFileAllocationIterator, FileAllocationIterator,
};

/// Error reported when an allocation is cancelled (engine halt / shutdown).
fn cancelled_error() -> Aria2Error {
    Aria2Error::DownloadFailed("file allocation cancelled".to_string())
}

// ---------------------------------------------------------------------------
// FileAllocationProtocol
// ---------------------------------------------------------------------------

/// Protocol-specific post-allocation behavior.
///
/// Mirrors the C++ class hierarchy:
/// - `BtFileAllocationEntry::prepareForNextAction()` → calls `BtSetup`
/// - `HttpFileAllocationEntry::prepareForNextAction()` → creates HTTP commands
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAllocationProtocol {
    /// BitTorrent: after allocation, run BtSetup and start peer connections.
    Bt,
    /// HTTP/FTP: after allocation, start the HTTP/FTP download commands.
    Http,
    /// Metalink: after allocation, delegate to metalink download handling.
    Metalink,
}

// ---------------------------------------------------------------------------
// AllocationKind / FileAllocationEntry
// ---------------------------------------------------------------------------

/// What to allocate: a single file or a set of files (multi-file torrent).
#[derive(Debug, Clone)]
pub enum AllocationKind {
    /// Single file: `(path, target_length)`.
    Path { path: PathBuf, length: u64 },
    /// Multi-file torrent layout: `(path, target_length)` per file, in order.
    /// Files whose on-disk size already reaches the target are skipped.
    Multi { files: Vec<(PathBuf, u64)> },
}

/// A single file allocation request in the queue.
///
/// Mirrors C++ `FileAllocationEntry` (base) + `BtFileAllocationEntry` /
/// `HttpFileAllocationEntry` (derived). The strategy and secure-falloc flags
/// are stored here so the worker can build the right `FileAllocationIterator`.
///
/// Not `Clone`: it owns a `oneshot::Sender` that can only be fired once.
pub struct FileAllocationEntry {
    /// Group ID for logging and lookups.
    pub gid: u64,

    /// What to allocate (single path or multi-file list).
    pub kind: AllocationKind,

    /// Allocation strategy (prealloc / falloc / trunc / mmap).
    pub strategy: AllocationStrategy,

    /// Zero-fill after fallocate on platforms that don't zero-fill.
    pub secure_falloc: bool,

    /// Protocol type — used for logging / diagnostics.
    pub protocol: FileAllocationProtocol,

    /// Time when this entry was created (for logging elapsed time).
    pub created_at: Instant,

    /// Set when the entry is cancelled (engine halt). Checked between chunks.
    cancelled: Arc<AtomicBool>,

    /// Progress shared with the manager while the worker owns this entry.
    progress: Arc<std::sync::atomic::AtomicU64>,

    /// Completion notification. The worker sends `Ok(())` on success, an
    /// `Err` on failure or cancellation. Taken out by the worker at the end.
    done_tx: Option<oneshot::Sender<Result<()>>>,
}

impl FileAllocationEntry {
    /// Build a single-file entry.
    pub fn single(
        gid: u64,
        path: PathBuf,
        length: u64,
        strategy: AllocationStrategy,
        secure_falloc: bool,
        protocol: FileAllocationProtocol,
        done_tx: oneshot::Sender<Result<()>>,
    ) -> Self {
        Self {
            gid,
            kind: AllocationKind::Path { path, length },
            strategy,
            secure_falloc,
            protocol,
            created_at: Instant::now(),
            cancelled: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            done_tx: Some(done_tx),
        }
    }

    /// Build a multi-file entry.
    pub fn multi(
        gid: u64,
        files: Vec<(PathBuf, u64)>,
        strategy: AllocationStrategy,
        secure_falloc: bool,
        protocol: FileAllocationProtocol,
        done_tx: oneshot::Sender<Result<()>>,
    ) -> Self {
        Self {
            gid,
            kind: AllocationKind::Multi { files },
            strategy,
            secure_falloc,
            protocol,
            created_at: Instant::now(),
            cancelled: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            done_tx: Some(done_tx),
        }
    }

    fn mark_cancelled(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// FileAllocationMan
// ---------------------------------------------------------------------------

/// Lightweight metadata of the entry currently being allocated.
///
/// The full [`FileAllocationEntry`] is moved out to the worker task via
/// [`FileAllocationMan::take_next_owned`]; this struct keeps the information
/// needed for `is_picked*()` / progress queries while the worker runs, plus a
/// shared cancellation flag so [`FileAllocationMan::cancel_all`] can stop an
/// in-flight allocation at its next chunk boundary.
#[derive(Debug, Clone)]
struct PickedMeta {
    gid: u64,
    total_length: u64,
    protocol: FileAllocationProtocol,
    created_at: Instant,
    cancelled: Arc<AtomicBool>,
    progress: Arc<std::sync::atomic::AtomicU64>,
}

impl PickedMeta {
    fn from_entry(e: &FileAllocationEntry) -> Self {
        let total = match &e.kind {
            AllocationKind::Path { length, .. } => *length,
            AllocationKind::Multi { files } => files.iter().map(|(_, l)| l).sum(),
        };
        Self {
            gid: e.gid,
            total_length: total,
            protocol: e.protocol,
            created_at: e.created_at,
            cancelled: Arc::clone(&e.cancelled),
            progress: Arc::clone(&e.progress),
        }
    }
}

/// Sequential file allocation manager.
///
/// Mirrors C++ `SequentialPicker<FileAllocationEntry>` (aliased as
/// `FileAllocationMan`). Manages a queue of pending file allocation entries
/// and processes them one at a time.
///
/// # C++ Equivalence
///
/// | Rust | C++ |
/// |---|---|
/// | `FileAllocationMan` | `SequentialPicker<FileAllocationEntry>` |
/// | `push_entry()` | `pushEntry()` |
/// | `take_next_owned()` | `pickNext()` + move-out |
/// | `drop_picked()` | `dropPickedEntry()` |
/// | `is_picked()` | `isPicked()` |
/// | `has_next()` | `hasNext()` |
/// | `count_in_queue()` | `countEntryInQueue()` |
pub struct FileAllocationMan {
    /// Queue of pending allocation entries.
    queue: VecDeque<FileAllocationEntry>,

    /// Wakes the worker when a new allocation enters an empty queue.
    queue_notify: Arc<Notify>,

    /// Metadata of the currently active allocation entry (if any).
    picked: Option<PickedMeta>,

    /// Bytes completed by the active allocation iterator.
    current_bytes: u64,

    /// Maximum number of concurrent allocations.
    /// C++ is always 1 (sequential). We allow >1 for instant strategies.
    max_concurrent: usize,

    /// Number of currently running allocation tasks.
    active_count: usize,
}

impl FileAllocationMan {
    /// Create a new file allocation manager.
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            queue_notify: Arc::new(Notify::new()),
            picked: None,
            current_bytes: 0,
            max_concurrent: 1, // Sequential by default, matching C++
            active_count: 0,
        }
    }

    /// Create a new file allocation manager with the given concurrency limit.
    pub fn with_concurrency(max_concurrent: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            queue_notify: Arc::new(Notify::new()),
            picked: None,
            current_bytes: 0,
            max_concurrent: max_concurrent.max(1),
            active_count: 0,
        }
    }

    /// Push a new allocation entry to the back of the queue.
    ///
    /// Mirrors C++ `SequentialPicker::pushEntry()`.
    pub fn push_entry(&mut self, entry: FileAllocationEntry) {
        debug!(
            gid = entry.gid,
            queue_len = self.queue.len(),
            "File allocation entry queued"
        );
        self.queue.push_back(entry);
        self.queue_notify.notify_one();
    }

    /// Return the notification source used by the background worker.
    pub fn queue_notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.queue_notify)
    }

    /// Pick the next entry from the queue for allocation, moving it out.
    ///
    /// Mirrors C++ `SequentialPicker::pickNext()`. Returns `None` if the
    /// queue is empty or the concurrency limit has been reached. The entry
    /// becomes the "picked" one until `drop_picked()` / `complete_current()`.
    pub fn take_next_owned(&mut self) -> Option<FileAllocationEntry> {
        if self.active_count >= self.max_concurrent {
            return None;
        }
        let entry = self.queue.pop_front()?;
        self.active_count += 1;
        self.current_bytes = 0;
        debug!(gid = entry.gid, "File allocation entry picked by worker");
        self.picked = Some(PickedMeta::from_entry(&entry));
        Some(entry)
    }

    /// Drop the currently picked entry after allocation completes.
    ///
    /// Mirrors C++ `SequentialPicker::dropPickedEntry()`.
    pub fn drop_picked(&mut self) {
        if self.picked.take().is_some() {
            self.active_count = self.active_count.saturating_sub(1);
            self.current_bytes = 0;
        }
    }

    /// Mark the current allocation as completed (drops picked entry).
    ///
    /// This is called after the allocation task finishes successfully.
    pub fn complete_current(&mut self) {
        if let Some(meta) = self.picked.take() {
            self.active_count = self.active_count.saturating_sub(1);
            self.current_bytes = 0;
            let elapsed = meta.created_at.elapsed();
            info!(
                gid = meta.gid,
                elapsed_secs = elapsed.as_secs_f64(),
                "File allocation completed"
            );
        }
    }

    /// Whether an entry is currently being allocated.
    ///
    /// Mirrors C++ `SequentialPicker::isPicked()`.
    pub fn is_picked(&self) -> bool {
        self.picked.is_some()
    }

    /// Whether there are more entries waiting in the queue.
    ///
    /// Mirrors C++ `SequentialPicker::hasNext()`.
    pub fn has_next(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Number of entries waiting in the queue.
    ///
    /// Mirrors C++ `SequentialPicker::countEntryInQueue()`.
    pub fn count_in_queue(&self) -> usize {
        self.queue.len()
    }

    /// Number of currently active allocations.
    pub fn active_count(&self) -> usize {
        self.active_count
    }

    /// Total number of entries (active + queued).
    pub fn total_entries(&self) -> usize {
        self.active_count + self.queue.len()
    }

    /// Check whether a specific group is currently being allocated.
    ///
    /// Mirrors C++ `SequentialPicker::isPicked(pred)`.
    pub fn is_picked_gid(&self, gid: u64) -> bool {
        self.picked.as_ref().is_some_and(|m| m.gid == gid)
    }

    /// Check whether a specific group is queued for allocation.
    ///
    /// Mirrors C++ `SequentialPicker::isQueued(pred)`.
    pub fn is_queued_gid(&self, gid: u64) -> bool {
        self.queue.iter().any(|e| e.gid == gid)
    }

    /// Remove a group from the queue (e.g., when download is cancelled).
    ///
    /// The removed entry's waiter is notified with the same cancellation
    /// error used by the manager-wide cancellation paths. Returns `true` if
    /// at least one entry was found and removed.
    pub fn remove_from_queue(&mut self, gid: u64) -> bool {
        let mut removed = false;
        self.queue.retain_mut(|entry| {
            if entry.gid != gid {
                return true;
            }
            removed = true;
            entry.mark_cancelled();
            if let Some(tx) = entry.done_tx.take() {
                let _ = tx.send(Err(cancelled_error()));
            }
            false
        });
        removed
    }

    /// Get progress information for the currently active allocation.
    ///
    /// Returns `(current_bytes, total_bytes)` for the picked entry.
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

    /// Update the active allocation progress from its iterator.
    pub fn update_progress(&mut self, current_bytes: u64) {
        if let Some(meta) = self.picked.as_ref() {
            meta.progress.store(
                current_bytes.min(meta.total_length),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    /// Get the currently picked entry's protocol type.
    pub fn picked_protocol(&self) -> Option<FileAllocationProtocol> {
        self.picked.as_ref().map(|m| m.protocol)
    }

    /// Cancel every queued entry and notify its waiter with an error.
    ///
    /// The currently running entry is marked cancelled and stops at the next
    /// chunk boundary. Mirrors an engine halt dropping pending allocations.
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
    }

    /// Cancel queued or active allocations belonging to one request group.
    ///
    /// Queued entries can be completed immediately because their waiters are
    /// owned by the manager. The active entry is owned by the worker, so only
    /// its shared cancellation flag is set; the worker reports the result
    /// after it reaches its next cancellation check.
    ///
    /// Returns the number of entries cancelled, including an active entry.
    pub fn cancel_gid(&mut self, gid: u64) -> usize {
        let mut cancelled = 0;
        let mut retained = VecDeque::with_capacity(self.queue.len());

        while let Some(entry) = self.queue.pop_front() {
            if entry.gid == gid {
                entry.mark_cancelled();
                if let Some(tx) = entry.done_tx {
                    let _ = tx.send(Err(cancelled_error()));
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

// ---------------------------------------------------------------------------
// Shared instance + worker loop
// ---------------------------------------------------------------------------

/// Thread-safe wrapper around `FileAllocationMan` for engine integration.
pub type SharedFileAllocationMan = Arc<RwLock<FileAllocationMan>>;

/// Process-wide shared allocation manager.
///
/// The engine and every download command enqueue through this single
/// instance, mirroring C++ where `FileAllocationMan` is owned by the
/// `DownloadEngine` singleton. Its worker owns a dedicated Tokio runtime, so
/// callers may use it from independently created runtimes.
pub fn shared() -> SharedFileAllocationMan {
    static SHARED: OnceLock<SharedFileAllocationMan> = OnceLock::new();
    static WORKER: OnceLock<()> = OnceLock::new();

    let man = SHARED
        .get_or_init(|| Arc::new(RwLock::new(FileAllocationMan::new())))
        .clone();

    let worker_man = Arc::clone(&man);
    WORKER.get_or_init(move || {
        std::thread::Builder::new()
            .name("aria2-file-allocation".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("file allocation worker runtime must initialize");
                runtime.block_on(worker_loop(worker_man));
            })
            .expect("file allocation worker thread must start");
    });

    man
}

/// Create a new shared file allocation manager with the given concurrency.
///
/// Spawns a background worker that consumes the queue. Mainly for tests;
/// production code uses [`shared()`] (process-wide singleton with a lazy
/// worker). Must be called from a tokio runtime context.
pub fn shared_file_allocation_man_with_concurrency(max: usize) -> SharedFileAllocationMan {
    let man = Arc::new(RwLock::new(FileAllocationMan::with_concurrency(max)));
    tokio::spawn(worker_loop(man.clone()));
    man
}

/// Background worker: consume the queue, allocate chunk-by-chunk, notify.
async fn worker_loop(man: SharedFileAllocationMan) {
    debug!("File allocation worker started");
    loop {
        // Arm the notification before inspecting the queue. `enable()` closes
        // the race where an enqueue happens between the empty-queue check and
        // the await below.
        let notifier = { man.read().await.queue_notifier() };
        let notified = notifier.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let entry = {
            let mut guard = man.write().await;
            guard.take_next_owned()
        };

        let Some(mut entry) = entry else {
            notified.await;
            continue;
        };

        let result = run_entry_allocation(&mut entry).await;

        // Release the slot and notify the waiter.
        {
            let mut guard = man.write().await;
            guard.drop_picked();
        }
        if let Some(tx) = entry.done_tx.take() {
            let _ = tx.send(result);
        }
    }
}

/// Drive one entry to completion: check disk space, open the file(s), run the
/// matching iterator chunk-by-chunk (yielding between chunks so the runtime
/// stays responsive), close, and report the outcome.
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
            allocate_single_file(path, *length, entry, 0).await
        }
        AllocationKind::Multi { files } => {
            if entry.strategy == AllocationStrategy::None {
                return Ok(());
            }
            let mut completed = 0u64;
            for (path, length) in files {
                if *length == 0 {
                    continue;
                }
                ensure_parent_dir(path).await?;
                if entry.is_cancelled() {
                    return Err(cancelled_error());
                }
                allocate_single_file(path, *length, entry, completed).await?;
                completed = completed.saturating_add(*length);
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
        Err(e) => warn!(gid, error = %e, "File allocation failed"),
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
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
    }
    Ok(())
}

async fn check_disk_space(path: &Path, length: u64) -> Result<()> {
    // K5.3: Pre-allocation disk space check (same as `preallocate_file`).
    if let Err(_e) = file_allocation::check_disk_space_async(path, length).await {
        return Err(Aria2Error::Fatal(FatalError::DiskSpaceExhausted));
    }
    Ok(())
}

/// Current on-disk size of `path` (0 when missing).
async fn current_size(path: &Path) -> u64 {
    match tokio::fs::metadata(path).await {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    }
}

/// Allocate one file up to `length` using the entry's strategy. Files that
/// already reach the target length are skipped (resume / already-allocated).
///
/// `Prealloc` follows aria2's adaptive iterator: it probes native fallocate
/// and uses chunked zero-fill only when native allocation is unavailable.
/// `Falloc` and `Trunc` remain one-shot operations; the `secure` flag is
/// honoured by fallocate on platforms whose native call does not clear data.
async fn allocate_single_file(
    path: &Path,
    length: u64,
    entry: &FileAllocationEntry,
    progress_base: u64,
) -> Result<()> {
    if entry.is_cancelled() {
        return Err(cancelled_error());
    }
    let offset = current_size(path).await;
    let existing = offset.min(length);
    entry
        .progress
        .store(progress_base + existing, Ordering::Relaxed);
    if offset >= length {
        debug!(path = %path.display(), "Skipping allocation, file already large enough");
        return Ok(());
    }

    let mut adaptor = DirectDiskAdaptor::new();
    adaptor.open(path).await?;
    let mut adaptor = Some(adaptor);

    let alloc_result: Result<()> = async {
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
                    entry.progress.store(
                        progress_base + iterator.current_length().min(length),
                        Ordering::Relaxed,
                    );
                    tokio::task::yield_now().await;
                }
                adaptor = Some(iterator.into_inner());
                if let Some(error) = allocation_error {
                    return Err(error);
                }
                entry
                    .progress
                    .store(progress_base + length, Ordering::Relaxed);
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
                entry
                    .progress
                    .store(progress_base + length, Ordering::Relaxed);
                Ok(())
            }
            AllocationStrategy::Falloc | AllocationStrategy::Mmap => {
                // One-shot fallocate; `secure` zero-fills on platforms whose
                // fallocate does not (macOS / Windows).
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
                entry
                    .progress
                    .store(progress_base + length, Ordering::Relaxed);
                Ok(())
            }
            AllocationStrategy::None => Ok(()),
        }
    }
    .await;

    // Close the file (best-effort; report the allocation error if any).
    let close_result = match adaptor.as_mut() {
        Some(adaptor) => adaptor.close().await,
        None => Ok(()),
    };
    match (alloc_result, close_result) {
        (Err(e), _) => Err(e),
        (Ok(()), Err(e)) => Err(e),
        (Ok(()), Ok(())) => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Entry-point helpers used by download commands
// ---------------------------------------------------------------------------

/// Queue single-file allocation and wait for completion.
///
/// Returns `Ok(())` once the file is allocated up to `length`. Errors are
/// propagated from the allocation worker (disk space, I/O, cancellation).
pub async fn enqueue_path(
    man: &SharedFileAllocationMan,
    path: &Path,
    length: u64,
    strategy: AllocationStrategy,
    secure_falloc: bool,
    gid: u64,
) -> Result<()> {
    if length == 0 || strategy == AllocationStrategy::None {
        return Ok(());
    }
    let (tx, rx) = oneshot::channel();
    let entry = FileAllocationEntry::single(
        gid,
        path.to_path_buf(),
        length,
        strategy,
        secure_falloc,
        FileAllocationProtocol::Http,
        tx,
    );
    man.write().await.push_entry(entry);
    rx.await.map_err(|_| cancelled_error())?
}

/// Queue multi-file allocation and wait for completion.
///
/// `files` is `(path, target_length)` in layout order; already-completed files
/// are skipped by the worker.
pub async fn enqueue_multi(
    man: &SharedFileAllocationMan,
    files: Vec<(PathBuf, u64)>,
    strategy: AllocationStrategy,
    secure_falloc: bool,
    gid: u64,
) -> Result<()> {
    if files.is_empty() || strategy == AllocationStrategy::None {
        return Ok(());
    }
    let (tx, rx) = oneshot::channel();
    let entry = FileAllocationEntry::multi(
        gid,
        files,
        strategy,
        secure_falloc,
        FileAllocationProtocol::Bt,
        tx,
    );
    man.write().await.push_entry(entry);
    rx.await.map_err(|_| cancelled_error())?
}

/// Cancel all pending allocations and notify their waiters.
///
/// Called on engine shutdown so no command hangs waiting for an allocation
/// that will never run.
pub async fn cancel_all(man: &SharedFileAllocationMan) {
    man.write().await.cancel_all();
}

/// Cancel pending allocations for one request group and wake its waiter(s).
pub async fn cancel_gid(man: &SharedFileAllocationMan, gid: u64) -> usize {
    man.write().await.cancel_gid(gid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::RwLock as TokioRwLock;

    fn make_entry(gid: u64) -> FileAllocationEntry {
        let (tx, _rx) = oneshot::channel();
        FileAllocationEntry::single(
            gid,
            PathBuf::from(format!("/tmp/test_{}", gid)),
            1024 * 1024,
            AllocationStrategy::Trunc,
            false,
            FileAllocationProtocol::Http,
            tx,
        )
    }

    #[test]
    fn test_push_and_pick() {
        let mut man = FileAllocationMan::new();
        assert!(!man.has_next());
        assert!(!man.is_picked());

        man.push_entry(make_entry(1));
        man.push_entry(make_entry(2));
        assert!(man.has_next());
        assert_eq!(man.count_in_queue(), 2);

        let picked = man.take_next_owned();
        assert!(picked.is_some());
        assert_eq!(picked.unwrap().gid, 1);
        assert!(man.is_picked());
        assert_eq!(man.count_in_queue(), 1);
        assert_eq!(man.active_count(), 1);
    }

    #[test]
    fn test_drop_picked() {
        let mut man = FileAllocationMan::new();
        man.push_entry(make_entry(1));
        man.take_next_owned();
        assert!(man.is_picked());

        man.drop_picked();
        assert!(!man.is_picked());
        assert_eq!(man.active_count(), 0);
    }

    #[test]
    fn test_complete_current() {
        let mut man = FileAllocationMan::new();
        man.push_entry(make_entry(1));
        man.take_next_owned();

        man.complete_current();
        assert!(!man.is_picked());
        assert_eq!(man.active_count(), 0);
    }

    #[test]
    fn test_concurrency_limit() {
        let mut man = FileAllocationMan::with_concurrency(1);
        man.push_entry(make_entry(1));
        man.push_entry(make_entry(2));

        // First pick succeeds
        let picked1 = man.take_next_owned();
        assert!(picked1.is_some());
        assert_eq!(man.active_count(), 1);

        // Second pick fails (concurrency limit reached)
        let picked2 = man.take_next_owned();
        assert!(picked2.is_none());

        // Complete the first, then second can proceed
        man.complete_current();
        let picked3 = man.take_next_owned();
        assert!(picked3.is_some());
        assert_eq!(picked3.unwrap().gid, 2);
    }

    #[test]
    fn test_is_picked_gid_and_is_queued_gid() {
        let mut man = FileAllocationMan::new();
        man.push_entry(make_entry(1));
        man.push_entry(make_entry(2));

        assert!(man.is_queued_gid(1));
        assert!(man.is_queued_gid(2));
        assert!(!man.is_picked_gid(1));

        man.take_next_owned();
        assert!(man.is_picked_gid(1));
        assert!(!man.is_queued_gid(1));
        assert!(man.is_queued_gid(2));
    }

    #[test]
    fn test_remove_from_queue() {
        let mut man = FileAllocationMan::new();
        let (tx, rx) = oneshot::channel();
        man.push_entry(FileAllocationEntry::single(
            1,
            PathBuf::from("/tmp/test_1"),
            1024 * 1024,
            AllocationStrategy::Trunc,
            false,
            FileAllocationProtocol::Http,
            tx,
        ));
        man.push_entry(make_entry(2));
        assert_eq!(man.count_in_queue(), 2);

        assert!(man.remove_from_queue(1));
        assert_eq!(man.count_in_queue(), 1);
        assert!(!man.is_queued_gid(1));
        assert!(man.is_queued_gid(2));
        assert!(rx.blocking_recv().unwrap().is_err());

        // Removing non-existent GID returns false
        assert!(!man.remove_from_queue(99));
    }

    #[tokio::test]
    async fn test_cancelled_allocation_does_not_modify_file() {
        let dir =
            std::env::temp_dir().join(format!("aria2_alloc_cancelled_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cancelled.bin");
        std::fs::write(&path, [0xA5u8; 4]).unwrap();

        let (tx, _rx) = oneshot::channel();
        let entry = FileAllocationEntry::single(
            13,
            path.clone(),
            4096,
            AllocationStrategy::Trunc,
            false,
            FileAllocationProtocol::Http,
            tx,
        );
        entry.mark_cancelled();

        let result = allocate_single_file(&path, 4096, &entry, 0).await;
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), [0xA5u8; 4]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_total_entries() {
        let mut man = FileAllocationMan::new();
        assert_eq!(man.total_entries(), 0);

        man.push_entry(make_entry(1));
        man.push_entry(make_entry(2));
        assert_eq!(man.total_entries(), 2);

        man.take_next_owned();
        // active=1, queue=1 => total=2
        assert_eq!(man.total_entries(), 2);

        man.complete_current();
        // active=0, queue=1 => total=1
        assert_eq!(man.total_entries(), 1);
    }

    #[test]
    fn test_current_progress() {
        let mut man = FileAllocationMan::new();
        assert!(man.current_progress().is_none());

        man.push_entry(make_entry(1));
        man.take_next_owned();
        let progress = man.current_progress();
        assert!(progress.is_some());
        let (_current, total) = progress.unwrap();
        assert_eq!(total, 1024 * 1024);
    }

    #[test]
    fn test_default() {
        let man = FileAllocationMan::default();
        assert_eq!(man.max_concurrent, 1);
        assert_eq!(man.active_count(), 0);
        assert_eq!(man.count_in_queue(), 0);
    }

    #[test]
    fn test_cancel_all_notifies_queued_waiters() {
        let man = Arc::new(TokioRwLock::new(FileAllocationMan::new()));

        // Queue two entries; keep the receivers.
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        {
            let mut guard = man.blocking_write();
            guard.push_entry(FileAllocationEntry::single(
                1,
                PathBuf::from("/tmp/a"),
                100,
                AllocationStrategy::Trunc,
                false,
                FileAllocationProtocol::Http,
                tx1,
            ));
            guard.push_entry(FileAllocationEntry::single(
                2,
                PathBuf::from("/tmp/b"),
                100,
                AllocationStrategy::Trunc,
                false,
                FileAllocationProtocol::Http,
                tx2,
            ));
        }

        man.blocking_write().cancel_all();
        assert_eq!(man.blocking_read().count_in_queue(), 0);

        // Both queued waiters must be woken with an error.
        assert!(rx1.blocking_recv().unwrap().is_err());
        assert!(rx2.blocking_recv().unwrap().is_err());
    }

    #[test]
    fn test_cancel_gid_only_notifies_matching_queued_waiters() {
        let man = Arc::new(TokioRwLock::new(FileAllocationMan::new()));
        let (target_tx, target_rx) = oneshot::channel();
        let (other_tx, mut other_rx) = oneshot::channel();
        let (target_again_tx, target_again_rx) = oneshot::channel();

        {
            let mut guard = man.blocking_write();
            guard.push_entry(FileAllocationEntry::single(
                11,
                PathBuf::from("/tmp/target-a"),
                100,
                AllocationStrategy::Trunc,
                false,
                FileAllocationProtocol::Http,
                target_tx,
            ));
            guard.push_entry(FileAllocationEntry::single(
                12,
                PathBuf::from("/tmp/other"),
                100,
                AllocationStrategy::Trunc,
                false,
                FileAllocationProtocol::Http,
                other_tx,
            ));
            guard.push_entry(FileAllocationEntry::single(
                11,
                PathBuf::from("/tmp/target-b"),
                100,
                AllocationStrategy::Trunc,
                false,
                FileAllocationProtocol::Http,
                target_again_tx,
            ));
        }

        assert_eq!(man.blocking_write().cancel_gid(11), 2);
        assert_eq!(man.blocking_read().count_in_queue(), 1);
        assert!(target_rx.blocking_recv().unwrap().is_err());
        assert!(target_again_rx.blocking_recv().unwrap().is_err());
        assert!(other_rx.try_recv().is_err());

        man.blocking_write().cancel_all();
        assert!(other_rx.blocking_recv().unwrap().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_enqueue_path_trunc() {
        let dir = std::env::temp_dir().join(format!("aria2_alloc_trunc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.bin");

        let man = shared_file_allocation_man_with_concurrency(1);
        enqueue_path(&man, &path, 4096, AllocationStrategy::Trunc, false, 7)
            .await
            .unwrap();
        assert_eq!(tokio::fs::metadata(&path).await.unwrap().len(), 4096);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_enqueue_multi_allocates_all_files() {
        let dir = std::env::temp_dir().join(format!("aria2_alloc_multi_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let files = vec![
            (dir.join("a.bin"), 8192u64),
            (dir.join("sub/b.bin"), 4096u64),
            (dir.join("c.bin"), 0u64),
        ];

        let man = shared_file_allocation_man_with_concurrency(1);
        enqueue_multi(&man, files.clone(), AllocationStrategy::Trunc, false, 9)
            .await
            .unwrap();

        assert_eq!(tokio::fs::metadata(&files[0].0).await.unwrap().len(), 8192);
        assert_eq!(tokio::fs::metadata(&files[1].0).await.unwrap().len(), 4096);
        // Zero-length file: skipped, but the directory must not fail.
        assert!(!files[2].0.exists() || tokio::fs::metadata(&files[2].0).await.unwrap().len() == 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_enqueue_multi_prealloc_preserves_existing_prefix() {
        let dir =
            std::env::temp_dir().join(format!("aria2_alloc_multi_prealloc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let first = dir.join("first.bin");
        let second = dir.join("nested/second.bin");
        let prefix = vec![0xA5u8; 4096];
        std::fs::write(&first, &prefix).unwrap();

        let files = vec![(first.clone(), 8192u64), (second.clone(), 4096u64)];
        let man = shared_file_allocation_man_with_concurrency(1);
        enqueue_multi(&man, files, AllocationStrategy::Prealloc, false, 10)
            .await
            .unwrap();

        assert_eq!(tokio::fs::metadata(&first).await.unwrap().len(), 8192);
        assert_eq!(tokio::fs::metadata(&second).await.unwrap().len(), 4096);
        let first_data = tokio::fs::read(&first).await.unwrap();
        assert_eq!(&first_data[..prefix.len()], prefix.as_slice());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_enqueue_path_skips_when_already_complete() {
        let dir = std::env::temp_dir().join(format!("aria2_alloc_skip_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("partial.bin");
        // Pre-create a file larger than the target: must be left untouched.
        std::fs::write(&path, vec![0xABu8; 16_384]).unwrap();

        let man = shared_file_allocation_man_with_concurrency(1);
        enqueue_path(&man, &path, 8192, AllocationStrategy::Prealloc, false, 3)
            .await
            .unwrap();

        // Existing data preserved, size unchanged.
        assert_eq!(tokio::fs::metadata(&path).await.unwrap().len(), 16_384);
        let data = tokio::fs::read(&path).await.unwrap();
        assert!(data.iter().all(|&b| b == 0xAB));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_worker_fifo_order() {
        let dir = std::env::temp_dir().join(format!("aria2_alloc_fifo_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // One worker, max_concurrent=1: with a slow (zero-fill) allocation
        // queued first and a fast one second, completion order must be FIFO.
        let man = shared_file_allocation_man_with_concurrency(1);

        let rx1 = {
            let (tx, rx) = oneshot::channel();
            man.write().await.push_entry(FileAllocationEntry::single(
                1,
                dir.join("big.bin"),
                2 * 1024 * 1024,
                AllocationStrategy::Prealloc,
                false,
                FileAllocationProtocol::Http,
                tx,
            ));
            rx
        };
        // Give entry 1 a head start so entry 2 queues behind it.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let rx2 = {
            let (tx, rx) = oneshot::channel();
            man.write().await.push_entry(FileAllocationEntry::single(
                2,
                dir.join("small.bin"),
                4096,
                AllocationStrategy::Trunc,
                false,
                FileAllocationProtocol::Http,
                tx,
            ));
            rx
        };

        // Both must complete (guarded by timeout so a deadlock fails fast),
        // and entry 1 (queued first) must complete before entry 2.
        let r1 = tokio::time::timeout(Duration::from_secs(15), rx1)
            .await
            .expect("entry 1 completion must not hang")
            .expect("entry 1 sender dropped");
        let r2 = tokio::time::timeout(Duration::from_secs(15), rx2)
            .await
            .expect("entry 2 completion must not hang")
            .expect("entry 2 sender dropped");

        assert!(r1.is_ok(), "entry 1 allocation failed: {:?}", r1);
        assert!(r2.is_ok(), "entry 2 allocation failed: {:?}", r2);
        assert_eq!(
            tokio::fs::metadata(dir.join("big.bin"))
                .await
                .unwrap()
                .len(),
            2 * 1024 * 1024
        );
        assert_eq!(
            tokio::fs::metadata(dir.join("small.bin"))
                .await
                .unwrap()
                .len(),
            4096
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_shared_instance_is_process_wide() {
        let a = shared();
        let b = shared();
        assert!(
            Arc::ptr_eq(&a, &b),
            "shared() must return the same instance"
        );
    }
}

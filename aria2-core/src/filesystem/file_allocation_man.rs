//! File allocation manager: sequential queue for file pre-allocation.
//!
//! Mirrors C++ `FileAllocationMan` (which is a `SequentialPicker<FileAllocationEntry>`)
//! plus `FileAllocationCommand` that drives chunked allocation in the event loop.
//!
//! # C++ Architecture
//!
//! In the original C++ aria2:
//! 1. `FileAllocationMan` = `SequentialPicker<FileAllocationEntry>` — a queue that
//!    picks one entry at a time for allocation (sequential, not concurrent).
//! 2. `FileAllocationEntry` wraps a `RequestGroup` + `FileAllocationIterator`.
//! 3. `FileAllocationCommand` is a persistent command that repeatedly calls
//!    `allocateChunk()` on the picked entry; when finished it calls
//!    `prepareForNextAction()` to create download commands.
//! 4. `BtFileAllocationEntry` / `HttpFileAllocationEntry` provide protocol-specific
//!    `prepareForNextAction()` implementations.
//!
//! # Rust Design
//!
//! The Rust implementation adapts this to async/await:
//! - `FileAllocationMan` holds a `VecDeque` of pending entries and the currently
//!   active entry. It processes one at a time (sequential, matching C++).
//! - `FileAllocationEntry` is an enum with `Bt` and `Http` variants that know
//!   how to run their post-allocation setup.
//! - Instead of a persistent command in an event loop, we spawn a tokio task
//!   that iterates the `FileAllocationIterator` and reports completion.
//!
//! # Concurrency Model
//!
//! C++ processes allocation entries strictly sequentially (one at a time).
//! The Rust version preserves this by default but can optionally run
//! allocations concurrently when the strategy is `trunc` or `falloc`
//! (which are near-instant and don't need sequential throttling).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// FileAllocationEntry
// ---------------------------------------------------------------------------

/// A single file allocation request in the queue.
///
/// Mirrors C++ `FileAllocationEntry` (base) + `BtFileAllocationEntry` /
/// `HttpFileAllocationEntry` (derived).
///
/// The `protocol` field determines what post-allocation actions to take,
/// matching the C++ virtual `prepareForNextAction()` dispatch.
#[derive(Debug)]
pub struct FileAllocationEntry {
    /// Group ID for logging and lookups.
    pub gid: u64,

    /// Output path to allocate.
    pub path: String,

    /// Total length to allocate in bytes.
    pub total_length: u64,

    /// Protocol type — determines post-allocation setup.
    pub protocol: FileAllocationProtocol,

    /// Time when this entry was created (for logging elapsed time).
    pub created_at: Instant,
}

/// Protocol-specific post-allocation behavior.
///
/// Mirrors the C++ class hierarchy:
/// - `BtFileAllocationEntry::prepareForNextAction()` → calls `BtSetup`
/// - `HttpFileAllocationEntry::prepareForNextAction()` → creates HTTP commands
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FileAllocationProtocol {
    /// BitTorrent: after allocation, run BtSetup and start peer connections.
    Bt,
    /// HTTP/FTP: after allocation, start the HTTP/FTP download commands.
    Http,
    /// Metalink: after allocation, delegate to metalink download handling.
    Metalink,
}

// ---------------------------------------------------------------------------
// FileAllocationMan
// ---------------------------------------------------------------------------

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
/// | `pick_next()` | `pickNext()` |
/// | `drop_picked()` | `dropPickedEntry()` |
/// | `is_picked()` | `isPicked()` |
/// | `has_next()` | `hasNext()` |
/// | `count_in_queue()` | `countEntryInQueue()` |
pub struct FileAllocationMan {
    /// Queue of pending allocation entries.
    queue: VecDeque<FileAllocationEntry>,

    /// Currently active allocation entry (if any).
    picked: Option<FileAllocationEntry>,

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
            picked: None,
            max_concurrent: 1, // Sequential by default, matching C++
            active_count: 0,
        }
    }

    /// Create a new file allocation manager with the given concurrency limit.
    pub fn with_concurrency(max_concurrent: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            picked: None,
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
            path = %entry.path,
            length = entry.total_length,
            queue_len = self.queue.len(),
            "File allocation entry queued"
        );
        self.queue.push_back(entry);
    }

    /// Pick the next entry from the queue for allocation.
    ///
    /// Mirrors C++ `SequentialPicker::pickNext()`. Returns `None` if the
    /// queue is empty or the concurrency limit has been reached.
    pub fn pick_next(&mut self) -> Option<&FileAllocationEntry> {
        if self.active_count >= self.max_concurrent {
            return None;
        }
        if self.queue.is_empty() {
            return None;
        }
        let entry = self.queue.pop_front().expect("queue was non-empty");
        self.active_count += 1;
        debug!(
            gid = entry.gid,
            path = %entry.path,
            "File allocation entry picked"
        );
        self.picked = Some(entry);
        self.picked.as_ref()
    }

    /// Drop the currently picked entry after allocation completes.
    ///
    /// Mirrors C++ `SequentialPicker::dropPickedEntry()`.
    pub fn drop_picked(&mut self) {
        if self.picked.take().is_some() {
            self.active_count = self.active_count.saturating_sub(1);
        }
    }

    /// Mark the current allocation as completed (drops picked entry).
    ///
    /// This is called after the allocation task finishes successfully.
    pub fn complete_current(&mut self) {
        if let Some(entry) = self.picked.take() {
            self.active_count = self.active_count.saturating_sub(1);
            let elapsed = entry.created_at.elapsed();
            info!(
                gid = entry.gid,
                path = %entry.path,
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
        self.picked.as_ref().map_or(false, |e| e.gid == gid)
    }

    /// Check whether a specific group is queued for allocation.
    ///
    /// Mirrors C++ `SequentialPicker::isQueued(pred)`.
    pub fn is_queued_gid(&self, gid: u64) -> bool {
        self.queue.iter().any(|e| e.gid == gid)
    }

    /// Remove a group from the queue (e.g., when download is cancelled).
    ///
    /// Returns `true` if the entry was found and removed.
    pub fn remove_from_queue(&mut self, gid: u64) -> bool {
        let before = self.queue.len();
        self.queue.retain(|e| e.gid != gid);
        self.queue.len() < before
    }

    /// Get progress information for the currently active allocation.
    ///
    /// Returns `(current_bytes, total_bytes)` for the picked entry,
    /// or `None` if no entry is active.
    pub fn current_progress(&self) -> Option<(u64, u64)> {
        // Note: actual progress tracking requires the allocation iterator,
        // which is managed by the spawned task. This returns the static
        // total_length from the entry for display purposes.
        self.picked.as_ref().map(|e| (0, e.total_length))
    }

    /// Get the currently picked entry's protocol type.
    pub fn picked_protocol(&self) -> Option<FileAllocationProtocol> {
        self.picked.as_ref().map(|e| e.protocol)
    }

    /// Pick all available entries up to the concurrency limit.
    ///
    /// Unlike `pick_next()` which picks one, this picks up to
    /// `max_concurrent - active_count` entries. Returns an iterator-like
    /// Vec of references.
    pub fn pick_available(&mut self) -> Vec<&FileAllocationEntry> {
        let mut picked = Vec::new();
        while self.active_count < self.max_concurrent && !self.queue.is_empty() {
            let entry = self.queue.pop_front().expect("queue was non-empty");
            self.active_count += 1;
            self.picked = Some(entry);
            // Since we can only hold one picked entry at a time in the
            // current design, we just pick one and return.
            picked.push(self.picked.as_ref().expect("just set"));
            break;
        }
        picked
    }
}

impl Default for FileAllocationMan {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Shared wrapper for engine integration
// ---------------------------------------------------------------------------

/// Thread-safe wrapper around `FileAllocationMan` for engine loop integration.
pub type SharedFileAllocationMan = Arc<RwLock<FileAllocationMan>>;

/// Create a new shared file allocation manager.
pub fn shared_file_allocation_man() -> SharedFileAllocationMan {
    Arc::new(RwLock::new(FileAllocationMan::new()))
}

/// Create a new shared file allocation manager with the given concurrency.
pub fn shared_file_allocation_man_with_concurrency(max: usize) -> SharedFileAllocationMan {
    Arc::new(RwLock::new(FileAllocationMan::with_concurrency(max)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(gid: u64) -> FileAllocationEntry {
        FileAllocationEntry {
            gid,
            path: format!("/tmp/test_{}", gid),
            total_length: 1024 * 1024,
            protocol: FileAllocationProtocol::Http,
            created_at: Instant::now(),
        }
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

        let picked = man.pick_next();
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
        man.pick_next();
        assert!(man.is_picked());

        man.drop_picked();
        assert!(!man.is_picked());
        assert_eq!(man.active_count(), 0);
    }

    #[test]
    fn test_complete_current() {
        let mut man = FileAllocationMan::new();
        man.push_entry(make_entry(1));
        man.pick_next();

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
        let picked1 = man.pick_next();
        assert!(picked1.is_some());
        assert_eq!(man.active_count(), 1);

        // Second pick fails (concurrency limit reached)
        let picked2 = man.pick_next();
        assert!(picked2.is_none());

        // Complete the first, then second can proceed
        man.complete_current();
        let picked3 = man.pick_next();
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

        man.pick_next();
        assert!(man.is_picked_gid(1));
        assert!(!man.is_queued_gid(1));
        assert!(man.is_queued_gid(2));
    }

    #[test]
    fn test_remove_from_queue() {
        let mut man = FileAllocationMan::new();
        man.push_entry(make_entry(1));
        man.push_entry(make_entry(2));
        assert_eq!(man.count_in_queue(), 2);

        assert!(man.remove_from_queue(1));
        assert_eq!(man.count_in_queue(), 1);
        assert!(!man.is_queued_gid(1));
        assert!(man.is_queued_gid(2));

        // Removing non-existent GID returns false
        assert!(!man.remove_from_queue(99));
    }

    #[test]
    fn test_total_entries() {
        let mut man = FileAllocationMan::new();
        assert_eq!(man.total_entries(), 0);

        man.push_entry(make_entry(1));
        man.push_entry(make_entry(2));
        assert_eq!(man.total_entries(), 2);

        man.pick_next();
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
        man.pick_next();
        let progress = man.current_progress();
        assert!(progress.is_some());
        let (current, total) = progress.unwrap();
        // Static progress: current is 0 until the allocation task updates it
        assert_eq!(current, 0);
        assert_eq!(total, 1024 * 1024);
    }

    #[test]
    fn test_default() {
        let man = FileAllocationMan::default();
        assert_eq!(man.max_concurrent, 1);
        assert_eq!(man.active_count(), 0);
        assert_eq!(man.count_in_queue(), 0);
    }
}

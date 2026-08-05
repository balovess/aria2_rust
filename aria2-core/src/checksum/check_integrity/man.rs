//! Check integrity manager: sequential queue + background worker for chunked
//! file integrity validation.
//!
//! Mirrors C++ `CheckIntegrityMan` (a `SequentialPicker<CheckIntegrityEntry>`)
//! plus `CheckIntegrityDispatcherCommand` + `CheckIntegrityCommand` that drive
//! chunked validation inside the event loop.
//!
//! # C++ Architecture
//!
//! 1. `CheckIntegrityMan` = `SequentialPicker<CheckIntegrityEntry>` — a queue
//!    that picks one entry at a time (sequential, not concurrent).
//! 2. `CheckIntegrityEntry` holds an `IteratableValidator` (piece-hash or
//!    whole-file) plus the post-check actions (`onDownloadFinished` /
//!    `onDownloadIncomplete`).
//! 3. `CheckIntegrityDispatcherCommand` (realtime, periodic) picks the next
//!    queued entry and hands it to a `CheckIntegrityCommand`.
//! 4. `CheckIntegrityCommand` calls `validateChunk()` once per event-loop tick
//!    so validating a large file never blocks the loop; when `finished()` it
//!    branches on the download being complete (→ allocation/download) or not
//!    (→ re-download), then drops the picked entry.
//!
//! # Rust Design
//!
//! The same semantics are provided with a background tokio task:
//! - `CheckIntegrityMan` holds a `VecDeque` of pending entries plus the active
//!   one; `max_concurrent` defaults to 1 (C++-matching sequential dispatch).
//! - A single worker loop takes the next entry, drives the
//!   [`CheckIntegrityTask`] chunk-by-chunk with `yield_now()` between chunks
//!   (async equivalent of per-tick `validateChunk()`), then signals the
//!   outcome (`Ok(true)` = verified, `Ok(false)` = mismatched, `Err` =
//!   I/O failure or cancellation) through a `oneshot` channel.
//! - [`enqueue`] waits for the outcome, so the calling download command
//!   resumes exactly when validation is done (mirroring C++ where the next
//!   commands are created only after the check completes).
//! - [`cancel_all`] clears the queue and notifies waiters so an engine halt
//!   never leaves a command hanging on a check that will not run.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{RwLock, oneshot};
use tracing::{debug, info, warn};

use crate::checksum::message_digest::{HashType, MessageDigest};
use crate::error::{Aria2Error, Result};

/// How long the worker sleeps when the queue is empty before polling again.
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Error reported when a queued integrity check is cancelled (engine halt).
fn cancelled_error() -> Aria2Error {
    Aria2Error::DownloadFailed("integrity check cancelled".to_string())
}

// ---------------------------------------------------------------------------
// CheckIntegrityTask
// ---------------------------------------------------------------------------

/// A chunked integrity validation task.
///
/// Equivalent to C++ `IteratableValidator`: it validates the data one chunk at
/// a time and reports progress / completion / outcome. The worker drives it.
/// `Send + Sync` so tasks can live in the process-wide shared manager and be
/// driven across tokio worker threads.
#[async_trait]
pub trait CheckIntegrityTask: Send + Sync {
    /// Total byte length of the data being validated.
    fn total_length(&self) -> u64;
    /// Byte length validated so far.
    fn current_length(&self) -> u64;
    /// Whether all chunks have been validated.
    fn is_finished(&self) -> bool;
    /// Validate the next chunk. Must be called repeatedly until `is_finished`.
    async fn validate_chunk(&mut self) -> Result<()>;
    /// Whether every validated chunk matched its expected digest.
    /// Only meaningful once `is_finished()` is true.
    fn passed(&self) -> bool;
}

// ---------------------------------------------------------------------------
// FileChunkValidator
// ---------------------------------------------------------------------------

/// Validates a file on disk chunk-by-chunk against expected piece digests.
///
/// Replaces the C++ `IteratableChunkChecksumValidator` for download paths that
/// write plain files (no `PieceStorage`). The file is opened lazily and read
/// at `piece_length`-sized offsets; each chunk's digest is compared against
/// the corresponding expected digest.
pub struct FileChunkValidator {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    piece_length: u64,
    total_length: u64,
    expected: Vec<Vec<u8>>,
    algo: HashType,
    current_piece: usize,
    finished: bool,
    passed: bool,
}

impl FileChunkValidator {
    /// Create a new file chunk validator.
    ///
    /// * `path` — file to validate
    /// * `piece_length` — chunk size in bytes
    /// * `total_length` — total file length
    /// * `expected_hex` — expected digest per chunk (lowercase hex)
    /// * `algo` — hash algorithm
    pub fn new(
        path: PathBuf,
        piece_length: u64,
        total_length: u64,
        expected_hex: Vec<String>,
        algo: HashType,
    ) -> Result<Self> {
        let expected: Vec<Vec<u8>> = expected_hex
            .iter()
            .map(|h| hex::decode(h).map_err(|e| Aria2Error::Io(format!("bad digest hex: {e}"))))
            .collect::<Result<Vec<_>>>()?;
        let finished = expected.is_empty();
        Ok(Self {
            path,
            file: None,
            piece_length: piece_length.max(1),
            total_length,
            expected,
            algo,
            current_piece: 0,
            finished,
            // `passed` starts true and flips only on a mismatch.
            passed: true,
        })
    }

    async fn ensure_open(&mut self) -> Result<()> {
        if self.file.is_none() {
            let f = tokio::fs::File::open(&self.path)
                .await
                .map_err(|e| Aria2Error::Io(format!("open {}: {}", self.path.display(), e)))?;
            self.file = Some(f);
        }
        Ok(())
    }
}

#[async_trait]
impl CheckIntegrityTask for FileChunkValidator {
    fn total_length(&self) -> u64 {
        self.total_length
    }

    fn current_length(&self) -> u64 {
        (self.current_piece as u64 * self.piece_length).min(self.total_length)
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    async fn validate_chunk(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.ensure_open().await?;
        let file = self.file.as_mut().expect("file opened above");

        let offset = self.current_piece as u64 * self.piece_length;
        let end = (offset + self.piece_length).min(self.total_length);
        let len = (end - offset) as usize;
        let mut buf = vec![0u8; len];

        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| Aria2Error::Io(e.to_string()))?;
        // Short reads are allowed: a truncated file yields a short final
        // chunk, whose digest will not match, so the check reports a mismatch
        // instead of failing with an I/O error (C++ behaviour: an incomplete
        // file fails validation and is re-downloaded).
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| Aria2Error::Io(format!("read {}: {}", self.path.display(), e)))?;
        buf.truncate(n);

        let digest = MessageDigest::new(self.algo);
        let mut digest = digest;
        digest.update(&buf);
        let actual = digest.finalize();

        let ok = self
            .expected
            .get(self.current_piece)
            .map_or(false, |expected| expected == &actual);
        if !ok {
            warn!(
                path = %self.path.display(),
                piece = self.current_piece,
                "Integrity check mismatch on piece"
            );
            self.passed = false;
        }

        self.current_piece += 1;
        if self.current_piece >= self.expected.len() {
            self.finished = true;
        }
        Ok(())
    }

    fn passed(&self) -> bool {
        self.passed && self.finished
    }
}

// ---------------------------------------------------------------------------
// CheckIntegrityEntry / CheckIntegrityMan
// ---------------------------------------------------------------------------

/// Lightweight metadata of the entry currently being validated.
#[derive(Debug, Clone)]
struct PickedMeta {
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
    done_tx: Option<oneshot::Sender<Result<bool>>>,
}

impl CheckIntegrityEntry {
    fn mark_cancelled(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// Sequential check-integrity manager (mirrors C++ `SequentialPicker<...>`).
pub struct CheckIntegrityMan {
    queue: VecDeque<CheckIntegrityEntry>,
    picked: Option<PickedMeta>,
    max_concurrent: usize,
    active_count: usize,
}

impl CheckIntegrityMan {
    /// Create a new manager (sequential by default, matching C++).
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            picked: None,
            max_concurrent: 1,
            active_count: 0,
        }
    }

    /// Create a manager with the given concurrency limit.
    pub fn with_concurrency(max_concurrent: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            picked: None,
            max_concurrent: max_concurrent.max(1),
            active_count: 0,
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
    }
}

impl Default for CheckIntegrityMan {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Shared instance + worker loop
// ---------------------------------------------------------------------------

/// Thread-safe wrapper for engine integration.
pub type SharedCheckIntegrityMan = Arc<RwLock<CheckIntegrityMan>>;

/// Process-wide shared check-integrity manager with a lazily spawned worker.
/// Must be called from a tokio runtime context.
pub fn shared() -> SharedCheckIntegrityMan {
    static SHARED: OnceLock<SharedCheckIntegrityMan> = OnceLock::new();
    static WORKER: OnceLock<()> = OnceLock::new();

    let man = SHARED
        .get_or_init(|| Arc::new(RwLock::new(CheckIntegrityMan::new())))
        .clone();
    let _ = WORKER.get_or_init(|| {
        tokio::spawn(worker_loop(man.clone()));
        ()
    });
    man
}

/// Shared manager with the given concurrency (mainly for tests; spawns its
/// own worker).
pub fn shared_with_concurrency(max: usize) -> SharedCheckIntegrityMan {
    let man = Arc::new(RwLock::new(CheckIntegrityMan::with_concurrency(max)));
    tokio::spawn(worker_loop(man.clone()));
    man
}

/// Background worker: consume the queue, drive validation chunk-by-chunk,
/// notify the waiter.
async fn worker_loop(man: SharedCheckIntegrityMan) {
    debug!("Check integrity worker started");
    loop {
        let entry = {
            let mut guard = man.write().await;
            guard.take_next_owned()
        };

        let Some(mut entry) = entry else {
            tokio::time::sleep(IDLE_POLL_INTERVAL).await;
            continue;
        };

        let result = run_validation(&mut entry).await;

        {
            let mut guard = man.write().await;
            guard.drop_picked();
        }
        if let Some(tx) = entry.done_tx.take() {
            let _ = tx.send(result);
        }
    }
}

/// Drive one entry to completion.
async fn run_validation(entry: &mut CheckIntegrityEntry) -> Result<bool> {
    let started = Instant::now();
    let gid = entry.gid;

    if entry.is_cancelled() {
        return Err(cancelled_error());
    }

    let result: Result<bool> = async {
        while !entry.task.is_finished() {
            if entry.is_cancelled() {
                return Err(cancelled_error());
            }
            entry.task.validate_chunk().await?;
            entry.progress.store(
                entry.task.current_length().min(entry.task.total_length()),
                Ordering::Relaxed,
            );
            // Cooperative scheduling: never hog a worker thread on big files.
            tokio::task::yield_now().await;
        }
        Ok(entry.task.passed())
    }
    .await;

    match &result {
        Ok(true) => info!(
            gid,
            elapsed_secs = started.elapsed().as_secs_f64(),
            "Integrity check passed"
        ),
        Ok(false) => warn!(
            gid,
            elapsed_secs = started.elapsed().as_secs_f64(),
            "Integrity check failed (piece mismatch)"
        ),
        Err(e) => warn!(gid, error = %e, "Integrity check failed"),
    }
    result
}

// ---------------------------------------------------------------------------
// Entry-point helpers used by download commands
// ---------------------------------------------------------------------------

/// Queue an integrity check and wait for its outcome.
///
/// Returns `Ok(true)` when the data verified, `Ok(false)` when a piece
/// mismatched, and `Err` on I/O failure or cancellation.
pub async fn enqueue(
    man: &SharedCheckIntegrityMan,
    gid: u64,
    task: Box<dyn CheckIntegrityTask>,
) -> Result<bool> {
    let (tx, rx) = oneshot::channel();
    let entry = CheckIntegrityEntry {
        gid,
        task,
        created_at: Instant::now(),
        cancelled: Arc::new(AtomicBool::new(false)),
        progress: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        done_tx: Some(tx),
    };
    man.write().await.push_entry(entry);
    rx.await.map_err(|_| cancelled_error())?
}

/// Build a [`FileChunkValidator`] task for a single file.
///
/// Returns `None` when there is nothing to validate (no expected digests, a
/// zero-length file, or the file does not exist yet).
pub fn file_task(
    path: &Path,
    piece_length: u64,
    total_length: u64,
    expected_hex: Vec<String>,
    algo: HashType,
) -> Result<Option<Box<dyn CheckIntegrityTask>>> {
    if expected_hex.is_empty() || total_length == 0 || !path.exists() {
        return Ok(None);
    }
    Ok(Some(Box::new(FileChunkValidator::new(
        path.to_path_buf(),
        piece_length,
        total_length,
        expected_hex,
        algo,
    )?)))
}

/// Cancel all pending checks and notify their waiters (engine shutdown).
pub async fn cancel_all(man: &SharedCheckIntegrityMan) {
    man.write().await.cancel_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aria2_ci_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sha1_hex(data: &[u8]) -> String {
        use sha1::Digest;
        let mut h = sha1::Sha1::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_file_validator_passes_and_fails() {
        let dir = test_dir("valid");
        let path = dir.join("f.bin");
        // 8 bytes, 2 pieces of 4 bytes.
        let data = b"aaaabbbb".to_vec();
        std::fs::write(&path, &data).unwrap();

        let expected = vec![sha1_hex(&data[0..4]), sha1_hex(&data[4..8])];
        let man = shared_with_concurrency(1);

        // Correct digests → Ok(true).
        let task = file_task(&path, 4, 8, expected.clone(), HashType::Sha1)
            .unwrap()
            .expect("task created");
        assert!(enqueue(&man, 1, task).await.unwrap());

        // Tampered first piece → Ok(false).
        let mut bad = data.clone();
        bad[0] ^= 0xFF;
        std::fs::write(&path, &bad).unwrap();
        let task = file_task(&path, 4, 8, expected.clone(), HashType::Sha1)
            .unwrap()
            .expect("task created");
        assert!(!enqueue(&man, 2, task).await.unwrap());

        // Tampered last piece → Ok(false).
        let mut bad_last = data.clone();
        bad_last[7] ^= 0x01;
        std::fs::write(&path, &bad_last).unwrap();
        let task = file_task(&path, 4, 8, expected, HashType::Sha1)
            .unwrap()
            .expect("task created");
        assert!(!enqueue(&man, 3, task).await.unwrap());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_file_task_none_when_no_digests_or_missing() {
        let dir = test_dir("none");
        let path = dir.join("missing.bin");

        // Missing file → None.
        assert!(
            file_task(&path, 4, 8, vec!["aa".to_string()], HashType::Sha1)
                .unwrap()
                .is_none()
        );
        // Empty digest list → None.
        let path2 = dir.join("exists.bin");
        std::fs::write(&path2, b"hello").unwrap();
        assert!(
            file_task(&path2, 4, 5, Vec::new(), HashType::Sha1)
                .unwrap()
                .is_none()
        );
        // Zero length → None.
        assert!(
            file_task(&path2, 4, 0, vec!["aa".to_string()], HashType::Sha1)
                .unwrap()
                .is_none()
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_queue_semantics() {
        let mut man = CheckIntegrityMan::new();
        assert!(!man.is_picked());
        assert!(!man.has_next());

        let (tx, _rx) = oneshot::channel();
        man.push_entry(CheckIntegrityEntry {
            gid: 1,
            task: Box::new(
                FileChunkValidator::new(
                    PathBuf::from("/tmp/x"),
                    4,
                    8,
                    vec![sha1_hex(b"aaaa"), sha1_hex(b"bbbb")],
                    HashType::Sha1,
                )
                .unwrap(),
            ),
            created_at: Instant::now(),
            cancelled: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            done_tx: Some(tx),
        });
        assert!(man.has_next());
        assert_eq!(man.count_in_queue(), 1);

        let entry = man.take_next_owned().expect("picked");
        assert_eq!(entry.gid, 1);
        assert!(man.is_picked());
        assert_eq!(man.active_count(), 1);

        man.drop_picked();
        assert!(!man.is_picked());
        assert_eq!(man.active_count(), 0);
    }

    #[test]
    fn test_cancel_all_notifies_queued() {
        let man = Arc::new(RwLock::new(CheckIntegrityMan::new()));
        let (tx1, rx1) = oneshot::channel();
        {
            let mut guard = man.blocking_write();
            guard.push_entry(CheckIntegrityEntry {
                gid: 1,
                task: Box::new(
                    FileChunkValidator::new(
                        PathBuf::from("/tmp/x"),
                        4,
                        8,
                        vec!["aa".to_string()],
                        HashType::Sha1,
                    )
                    .unwrap(),
                ),
                created_at: Instant::now(),
                cancelled: Arc::new(AtomicBool::new(false)),
                progress: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                done_tx: Some(tx1),
            });
        }
        man.blocking_write().cancel_all();
        assert_eq!(man.blocking_read().count_in_queue(), 0);
        assert!(rx1.blocking_recv().unwrap().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_shared_instance_is_process_wide() {
        let a = shared();
        let b = shared();
        assert!(Arc::ptr_eq(&a, &b));
    }
}

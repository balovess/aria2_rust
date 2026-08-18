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
use std::time::Instant;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{Notify, RwLock, oneshot};
use tracing::{debug, info, warn};

use crate::checksum::checksum::Checksum;
use crate::checksum::message_digest::{HashType, MessageDigest};
use crate::error::{Aria2Error, Result};
use crate::request::request_group::RequestGroup;
use crate::util::rwlock_ext::RwLockRecover;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrityOutcome {
    pub verified: bool,
    pub failed_piece_indices: Vec<usize>,
    pub verified_piece_indices: Vec<usize>,
}

fn expected_piece_count(total_length: u64, piece_length: u64) -> usize {
    total_length.div_ceil(piece_length.max(1)) as usize
}

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
    /// Piece indexes that failed validation. Empty for validators that do not
    /// expose piece-level outcomes.
    fn failed_piece_indices(&self) -> Vec<usize> {
        Vec::new()
    }
    /// Piece indexes that passed validation. Empty for validators that do not
    /// expose piece-level outcomes.
    fn verified_piece_indices(&self) -> Vec<usize> {
        Vec::new()
    }
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
    failed_indices: Vec<usize>,
    verified_indices: Vec<usize>,
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
        if !expected_hex.is_empty()
            && expected_hex.len() != expected_piece_count(total_length, piece_length)
        {
            return Err(Aria2Error::Parse(format!(
                "piece digest count mismatch: expected {}, got {}",
                expected_piece_count(total_length, piece_length),
                expected_hex.len()
            )));
        }
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
            failed_indices: Vec::new(),
            verified_indices: Vec::new(),
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
            self.failed_indices.push(self.current_piece);
        } else {
            self.verified_indices.push(self.current_piece);
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

    fn failed_piece_indices(&self) -> Vec<usize> {
        self.failed_indices.clone()
    }

    fn verified_piece_indices(&self) -> Vec<usize> {
        self.verified_indices.clone()
    }
}

/// Validates the logical byte stream of a multi-file torrent.
///
/// Files are presented in torrent order and are treated as one contiguous
/// stream, so a piece may be hashed from more than one physical file.
pub struct MultiFileChunkValidator {
    files: Vec<(PathBuf, u64)>,
    piece_length: u64,
    total_length: u64,
    expected: Vec<Vec<u8>>,
    algo: HashType,
    current_piece: usize,
    finished: bool,
    passed: bool,
    failed_indices: Vec<usize>,
    verified_indices: Vec<usize>,
}

impl MultiFileChunkValidator {
    pub fn new(
        files: Vec<(PathBuf, u64)>,
        piece_length: u64,
        total_length: u64,
        expected_hex: Vec<String>,
        algo: HashType,
    ) -> Result<Self> {
        if !expected_hex.is_empty()
            && expected_hex.len() != expected_piece_count(total_length, piece_length)
        {
            return Err(Aria2Error::Parse(format!(
                "piece digest count mismatch: expected {}, got {}",
                expected_piece_count(total_length, piece_length),
                expected_hex.len()
            )));
        }
        let expected = expected_hex
            .iter()
            .map(|h| hex::decode(h).map_err(|e| Aria2Error::Io(format!("bad digest hex: {e}"))))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            files,
            piece_length: piece_length.max(1),
            total_length,
            finished: expected.is_empty(),
            expected,
            algo,
            current_piece: 0,
            passed: true,
            failed_indices: Vec::new(),
            verified_indices: Vec::new(),
        })
    }

    async fn read_piece(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let piece_end = offset + length as u64;
        let mut logical_start = 0u64;
        let mut output = Vec::with_capacity(length);
        for (path, file_length) in &self.files {
            let file_start = logical_start;
            let file_end = file_start + *file_length;
            logical_start = file_end;
            if file_end <= offset || file_start >= piece_end || *file_length == 0 {
                continue;
            }
            let read_start = offset.max(file_start);
            let read_end = piece_end.min(file_end);
            let mut file = tokio::fs::File::open(path)
                .await
                .map_err(|e| Aria2Error::Io(format!("open {}: {}", path.display(), e)))?;
            file.seek(std::io::SeekFrom::Start(read_start - file_start))
                .await
                .map_err(|e| Aria2Error::Io(format!("seek {}: {}", path.display(), e)))?;
            let count = (read_end - read_start) as usize;
            let mut buf = vec![0u8; count];
            let mut read = 0;
            while read < count {
                let n = file
                    .read(&mut buf[read..])
                    .await
                    .map_err(|e| Aria2Error::Io(format!("read {}: {}", path.display(), e)))?;
                if n == 0 {
                    // A physically truncated entry is an incomplete piece,
                    // not a fatal validation error. Keep the bytes available
                    // so the digest mismatch selects the re-download path.
                    break;
                }
                read += n;
            }
            output.extend_from_slice(&buf[..read]);
        }
        Ok(output)
    }
}

#[async_trait]
impl CheckIntegrityTask for MultiFileChunkValidator {
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
        let offset = self.current_piece as u64 * self.piece_length;
        let length = (self.total_length - offset).min(self.piece_length) as usize;
        let data = self.read_piece(offset, length).await?;
        let mut digest = MessageDigest::new(self.algo);
        digest.update(&data);
        let actual = digest.finalize();
        if self.expected.get(self.current_piece) != Some(&actual) {
            self.passed = false;
            self.failed_indices.push(self.current_piece);
        } else {
            self.verified_indices.push(self.current_piece);
        }
        self.current_piece += 1;
        self.finished = self.current_piece >= self.expected.len();
        Ok(())
    }
    fn passed(&self) -> bool {
        self.passed && self.finished
    }

    fn failed_piece_indices(&self) -> Vec<usize> {
        self.failed_indices.clone()
    }

    fn verified_piece_indices(&self) -> Vec<usize> {
        self.verified_indices.clone()
    }
}

/// Validates one whole file against a configured checksum while yielding
/// between bounded reads.
///
/// This is the common post-download validator for protocols that expose one
/// whole-file checksum rather than per-piece hashes. Keeping it in the same
/// dispatcher gives HTTP, Metalink, FTP, and SFTP the same cancellation and
/// lifecycle behavior as piece-integrity checks.
pub struct FileChecksumTask {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    total_length: u64,
    current_length: u64,
    expected_hex: String,
    digest: Option<MessageDigest>,
    finished: bool,
    passed: bool,
}

impl FileChecksumTask {
    pub fn new(path: PathBuf, total_length: u64, checksum: Checksum) -> Self {
        Self {
            path,
            file: None,
            total_length,
            current_length: 0,
            expected_hex: checksum.expected_hex().to_owned(),
            digest: Some(MessageDigest::new(checksum.hash_type())),
            finished: false,
            passed: false,
        }
    }

    async fn ensure_open(&mut self) -> Result<()> {
        if self.file.is_none() {
            let file = tokio::fs::File::open(&self.path).await.map_err(|error| {
                Aria2Error::Io(format!("Failed to open {}: {}", self.path.display(), error))
            })?;
            self.file = Some(file);
        }
        Ok(())
    }
}

#[async_trait]
impl CheckIntegrityTask for FileChecksumTask {
    fn total_length(&self) -> u64 {
        self.total_length
    }

    fn current_length(&self) -> u64 {
        self.current_length
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    async fn validate_chunk(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.ensure_open().await?;

        let mut buffer = vec![0u8; 64 * 1024];
        let bytes_read = self
            .file
            .as_mut()
            .expect("file opened above")
            .read(&mut buffer)
            .await
            .map_err(|error| {
                Aria2Error::Io(format!("Failed to read {}: {}", self.path.display(), error))
            })?;

        if bytes_read == 0 {
            let actual_hex = self
                .digest
                .take()
                .expect("checksum digest is present until EOF")
                .finalize_hex();
            self.passed = actual_hex.eq_ignore_ascii_case(&self.expected_hex);
            self.finished = true;
        } else {
            self.digest
                .as_mut()
                .expect("checksum digest is present before EOF")
                .update(&buffer[..bytes_read]);
            self.current_length += bytes_read as u64;
        }
        Ok(())
    }

    fn passed(&self) -> bool {
        self.finished && self.passed
    }
}

// ---------------------------------------------------------------------------
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

    fn wake_notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
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
    static WORKER: OnceLock<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
        OnceLock::new();

    let man = SHARED
        .get_or_init(|| Arc::new(RwLock::new(CheckIntegrityMan::new())))
        .clone();
    // A worker is tied to the runtime that spawned it. Recreate it when the
    // previous runtime has shut down instead of retaining a stale start flag.
    let worker = WORKER.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = match worker.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard
        .as_ref()
        .is_none_or(tokio::task::JoinHandle::is_finished)
    {
        *guard = Some(tokio::spawn(worker_loop(man.clone())));
    }
    man
}

/// Shared manager with the given concurrency (mainly for tests; spawns its
/// own worker).
pub fn shared_with_concurrency(max: usize) -> SharedCheckIntegrityMan {
    let man = Arc::new(RwLock::new(CheckIntegrityMan::with_concurrency(max)));
    tokio::spawn(worker_loop(man.clone()));
    man
}

/// Background worker: pick queued entries, drive validation chunk-by-chunk,
/// then notify the waiter with the validation outcome.
async fn worker_loop(man: SharedCheckIntegrityMan) {
    debug!("Check integrity worker started");
    let wake = {
        let guard = man.read().await;
        guard.wake_notifier()
    };

    loop {
        let entry = {
            let mut guard = man.write().await;
            guard.take_next_owned()
        };

        let Some(mut entry) = entry else {
            // Queue insertion and cancellation both notify this waiter. The
            // worker therefore consumes no timer wakeups while idle.
            wake.notified().await;
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
async fn run_validation(entry: &mut CheckIntegrityEntry) -> Result<IntegrityOutcome> {
    let started = Instant::now();
    let gid = entry.gid;

    if entry.is_cancelled() {
        return Err(cancelled_error());
    }

    let result: Result<IntegrityOutcome> = async {
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
        Ok(IntegrityOutcome {
            verified: entry.task.passed(),
            failed_piece_indices: entry.task.failed_piece_indices(),
            verified_piece_indices: entry.task.verified_piece_indices(),
        })
    }
    .await;

    match &result {
        Ok(outcome) if outcome.verified => info!(
            gid,
            elapsed_secs = started.elapsed().as_secs_f64(),
            "Integrity check passed"
        ),
        Ok(_) => warn!(
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

/// Queue an integrity check and return its completion receiver.
async fn enqueue_entry(
    man: &SharedCheckIntegrityMan,
    gid: u64,
    task: Box<dyn CheckIntegrityTask>,
) -> oneshot::Receiver<Result<IntegrityOutcome>> {
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
    rx
}

/// Queue an integrity check and wait for its detailed validation outcome.
pub async fn enqueue_with_outcome(
    man: &SharedCheckIntegrityMan,
    gid: u64,
    task: Box<dyn CheckIntegrityTask>,
) -> Result<IntegrityOutcome> {
    enqueue_entry(man, gid, task)
        .await
        .await
        .map_err(|_| cancelled_error())?
}

pub async fn enqueue(
    man: &SharedCheckIntegrityMan,
    gid: u64,
    task: Box<dyn CheckIntegrityTask>,
) -> Result<bool> {
    let outcome = enqueue_entry(man, gid, task)
        .await
        .await
        .map_err(|_| cancelled_error())??;
    Ok(outcome.verified)
}

/// Build a [`FileChunkValidator`] task for a single file.
///
/// Returns `None` when there is nothing to validate (no expected digests, a
/// zero-length file, or the file does not exist yet).
pub fn multi_file_task(
    files: Vec<(PathBuf, u64)>,
    piece_length: u64,
    total_length: u64,
    expected_hex: Vec<String>,
    algo: HashType,
) -> Result<Option<Box<dyn CheckIntegrityTask>>> {
    // A missing non-empty physical file is an incomplete payload, not an
    // integrity-check I/O failure. Let the owning BT command enter its normal
    // piece-download path, matching the single-file helper's behavior.
    if expected_hex.is_empty()
        || total_length == 0
        || files.is_empty()
        || files
            .iter()
            .any(|(path, length)| *length > 0 && !path.is_file())
    {
        return Ok(None);
    }
    Ok(Some(Box::new(MultiFileChunkValidator::new(
        files,
        piece_length,
        total_length,
        expected_hex,
        algo,
    )?)))
}

/// Truncate an output file when it contains bytes beyond the declared length.
pub async fn cut_trailing_garbage(path: &Path, expected_length: u64) -> Result<()> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Aria2Error::FileIo(format!("{}: {error}", path.display()))),
    };
    if metadata.len() > expected_length {
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .await
            .map_err(|error| Aria2Error::FileOpen(format!("{}: {error}", path.display())))?;
        file.set_len(expected_length)
            .await
            .map_err(|error| Aria2Error::FileIo(format!("{}: {error}", path.display())))?;
    }
    Ok(())
}

/// Truncate each physical file in a logical multi-file stream to its declared length.
pub async fn cut_multi_file_trailing_garbage(files: &[(PathBuf, u64)]) -> Result<()> {
    for (path, expected_length) in files {
        cut_trailing_garbage(path, *expected_length).await?;
    }
    Ok(())
}

#[cfg(test)]
mod trailing_garbage_tests {
    use super::{cut_multi_file_trailing_garbage, cut_trailing_garbage, multi_file_task};
    use crate::checksum::message_digest::HashType;

    #[tokio::test]
    async fn truncates_single_file_only_when_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("single.bin");
        tokio::fs::write(&path, vec![0u8; 12]).await.unwrap();
        cut_trailing_garbage(&path, 8).await.unwrap();
        assert_eq!(tokio::fs::metadata(&path).await.unwrap().len(), 8);
        cut_trailing_garbage(&path, 8).await.unwrap();
        assert_eq!(tokio::fs::metadata(&path).await.unwrap().len(), 8);
    }

    #[tokio::test]
    async fn truncates_each_multi_file_entry() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.bin");
        let second = dir.path().join("second.bin");
        tokio::fs::write(&first, vec![0u8; 13]).await.unwrap();
        tokio::fs::write(&second, vec![0u8; 7]).await.unwrap();
        cut_multi_file_trailing_garbage(&[(first.clone(), 5), (second.clone(), 3)])
            .await
            .unwrap();
        assert_eq!(tokio::fs::metadata(first).await.unwrap().len(), 5);
        assert_eq!(tokio::fs::metadata(second).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn skips_multi_file_integrity_task_when_payload_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present.bin");
        let missing = dir.path().join("missing.bin");
        tokio::fs::write(&present, b"abcd").await.unwrap();

        let task = multi_file_task(
            vec![(present, 4), (missing, 4)],
            4,
            8,
            vec!["00".to_string(); 2],
            HashType::Sha1,
        )
        .unwrap();

        assert!(task.is_none());
    }
}

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

/// Queue a whole-file checksum validation through the shared lifecycle-aware
/// integrity dispatcher.
pub async fn enqueue_file_checksum_for_group(
    man: &SharedCheckIntegrityMan,
    group: Arc<std::sync::RwLock<RequestGroup>>,
    path: &Path,
    total_length: u64,
    checksum: Checksum,
) -> Result<bool> {
    let outcome = enqueue_with_outcome_for_group(
        man,
        group,
        Box::new(FileChecksumTask::new(
            path.to_path_buf(),
            total_length,
            checksum,
        )),
    )
    .await?;
    Ok(outcome.verified)
}

/// Cancel all pending checks and notify their waiters (engine shutdown).
pub async fn cancel_all(man: &SharedCheckIntegrityMan) {
    man.write().await.cancel_all();
}

/// Cancel integrity validation for one RequestGroup.
pub async fn cancel_gid(man: &SharedCheckIntegrityMan, gid: u64) -> bool {
    man.write().await.cancel_gid(gid)
}

fn request_group_cancellation_error(group: &RequestGroup) -> Option<Aria2Error> {
    if group.is_removed() {
        Some(Aria2Error::DownloadFailed(
            "Download cancelled by user".to_string(),
        ))
    } else if group.is_paused_flag() {
        Some(Aria2Error::DownloadFailed("Download paused".to_string()))
    } else if group.is_force_halt_requested() || group.is_halt_requested() {
        Some(Aria2Error::DownloadFailed("Download halted".to_string()))
    } else {
        None
    }
}

/// Queue an integrity check while observing its owning RequestGroup.
///
/// The validator worker remains the owner of validation state, but lifecycle
/// control belongs to the RequestGroup. The group's lifecycle notification
/// wakes this waiter immediately when pause/remove/halt changes state.
pub async fn enqueue_with_outcome_for_group(
    man: &SharedCheckIntegrityMan,
    group: Arc<std::sync::RwLock<RequestGroup>>,
    task: Box<dyn CheckIntegrityTask>,
) -> Result<IntegrityOutcome> {
    let gid = group.recover().gid().value();
    let lifecycle_notify = group.recover().lifecycle_notifier();
    // Queue before observing lifecycle state so cancellation can always find
    // and complete the entry, even when the group was already stopped before
    // this function was first polled.
    let receiver = enqueue_entry(man, gid, task).await;
    let mut validation = Box::pin(async move { receiver.await.map_err(|_| cancelled_error())? });

    loop {
        let lifecycle_changed = lifecycle_notify.notified();
        tokio::pin!(lifecycle_changed);
        lifecycle_changed.as_mut().enable();

        let cancellation_error = {
            let group_guard = group.recover();
            request_group_cancellation_error(&group_guard)
        };
        if let Some(error) = cancellation_error {
            cancel_gid(man, gid).await;
            let _ = validation.await;
            return Err(error);
        }

        tokio::select! {
            result = &mut validation => return result,
            _ = &mut lifecycle_changed => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::GroupId;
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::time::Duration;

    struct SlowIntegrityTask {
        remaining_chunks: usize,
        current_length: u64,
    }

    #[async_trait]
    impl CheckIntegrityTask for SlowIntegrityTask {
        fn total_length(&self) -> u64 {
            (self.remaining_chunks as u64 + 1) * 1024
        }

        fn current_length(&self) -> u64 {
            self.current_length
        }

        fn is_finished(&self) -> bool {
            self.remaining_chunks == 0
        }

        async fn validate_chunk(&mut self) -> Result<()> {
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.remaining_chunks -= 1;
            self.current_length += 1024;
            Ok(())
        }

        fn passed(&self) -> bool {
            self.is_finished()
        }
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aria2_ci_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn run_active_cancellation(
        trigger: impl FnOnce(&Arc<std::sync::RwLock<RequestGroup>>),
    ) -> Result<IntegrityOutcome> {
        let man = shared_with_concurrency(1);
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(5),
            vec!["http://example.test/payload".to_string()],
            crate::request::request_group::DownloadOptions::default(),
        )));
        let man_for_validation = Arc::clone(&man);
        let group_for_validation = Arc::clone(&group);
        let validation = tokio::spawn(async move {
            enqueue_with_outcome_for_group(
                &man_for_validation,
                group_for_validation,
                Box::new(SlowIntegrityTask {
                    remaining_chunks: 100,
                    current_length: 0,
                }),
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if man.read().await.is_picked() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("integrity validation should become active");

        trigger(&group);
        let result = tokio::time::timeout(Duration::from_secs(1), validation)
            .await
            .expect("lifecycle cancellation should be prompt")
            .expect("validation task should not panic");
        assert_eq!(man.read().await.active_count(), 0);
        result
    }

    fn sha1_hex(data: &[u8]) -> String {
        use sha1::Digest;
        let mut h = sha1::Sha1::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    #[tokio::test]
    async fn test_multi_file_task_piece_crosses_file_boundary() {
        let dir = test_dir("multi_cross");
        let first = dir.join("first");
        let second = dir.join("second");
        tokio::fs::write(&first, b"abcd").await.unwrap();
        tokio::fs::write(&second, b"efghij").await.unwrap();
        let expected = vec![sha1_hex(b"abcdef"), sha1_hex(b"ghij")];
        let task = multi_file_task(
            vec![(first, 4), (second, 6)],
            6,
            10,
            expected,
            HashType::Sha1,
        )
        .unwrap()
        .unwrap();
        assert!(
            enqueue(&shared_with_concurrency(1), 90, task)
                .await
                .unwrap()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_multi_file_task_truncated_existing_file_is_mismatch() {
        let dir = test_dir("multi_truncated");
        let first = dir.join("first");
        let second = dir.join("second");
        tokio::fs::write(&first, b"abcd").await.unwrap();
        // The metadata declares six bytes, but the existing file contains only
        // the first two. The missing bytes must be treated as a bad piece so
        // the normal re-download path can repair the payload.
        tokio::fs::write(&second, b"ef").await.unwrap();
        let expected = vec![sha1_hex(b"abcdef"), sha1_hex(b"ghij")];
        let task = multi_file_task(
            vec![(first, 4), (second, 6)],
            6,
            10,
            expected,
            HashType::Sha1,
        )
        .unwrap()
        .unwrap();
        let outcome = enqueue_with_outcome(&shared_with_concurrency(1), 92, task)
            .await
            .expect("a truncated existing file is an integrity mismatch");

        assert!(!outcome.verified);
        assert_eq!(outcome.verified_piece_indices, vec![0]);
        assert_eq!(outcome.failed_piece_indices, vec![1]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn test_multi_file_task_detects_late_file_corruption() {
        let dir = test_dir("multi_bad");
        let first = dir.join("first");
        let second = dir.join("second");
        tokio::fs::write(&first, b"abcd").await.unwrap();
        tokio::fs::write(&second, b"efghij").await.unwrap();
        let expected = vec![sha1_hex(b"abcdef"), sha1_hex(b"ghij")];
        tokio::fs::write(&second, b"efgXij").await.unwrap();
        let task = multi_file_task(
            vec![(first, 4), (second, 6)],
            6,
            10,
            expected,
            HashType::Sha1,
        )
        .unwrap()
        .unwrap();
        assert!(
            !enqueue(&shared_with_concurrency(1), 91, task)
                .await
                .unwrap()
        );
        let _ = std::fs::remove_dir_all(dir);
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
    async fn test_enqueue_with_outcome_preserves_piece_indices() {
        let dir = test_dir("outcome");
        let path = dir.join("f.bin");
        let data = b"aaaabbbb".to_vec();
        std::fs::write(&path, &data).unwrap();

        let expected = vec![sha1_hex(&data[0..4]), sha1_hex(b"xxxx")];
        let task = file_task(&path, 4, 8, expected, HashType::Sha1)
            .unwrap()
            .expect("task created");
        let outcome = enqueue_with_outcome(&shared_with_concurrency(1), 4, task)
            .await
            .unwrap();

        assert!(!outcome.verified);
        assert_eq!(outcome.verified_piece_indices, vec![0]);
        assert_eq!(outcome.failed_piece_indices, vec![1]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_file_checksum_dispatcher_streams_and_reports_mismatch() {
        let dir = test_dir("whole_file_checksum");
        let path = dir.join("payload.bin");
        let data: Vec<u8> = (0..131_072).map(|index| (index % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let expected = MessageDigest::hash_hex(HashType::Sha256, &data);
        let man = shared_with_concurrency(1);

        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(93),
            vec!["http://example.test/payload".to_string()],
            crate::request::request_group::DownloadOptions::default(),
        )));
        assert!(
            enqueue_file_checksum_for_group(
                &man,
                group,
                &path,
                data.len() as u64,
                Checksum::new(HashType::Sha256, &expected).unwrap(),
            )
            .await
            .unwrap()
        );

        let mut corrupted = data;
        corrupted[70_000] ^= 0x01;
        std::fs::write(&path, corrupted).unwrap();
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(94),
            vec!["http://example.test/payload".to_string()],
            crate::request::request_group::DownloadOptions::default(),
        )));
        assert!(
            !enqueue_file_checksum_for_group(
                &man,
                group,
                &path,
                131_072,
                Checksum::new(HashType::Sha256, &expected).unwrap(),
            )
            .await
            .unwrap()
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_group_pause_cancels_active_integrity_validation() {
        let result = run_active_cancellation(|group| {
            group.write().unwrap().pause().unwrap();
        })
        .await;
        assert!(matches!(
            result,
            Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_group_remove_cancels_active_integrity_validation() {
        let result = run_active_cancellation(|group| {
            group.write().unwrap().remove().unwrap();
        })
        .await;
        assert!(matches!(
            result,
            Err(Aria2Error::DownloadFailed(message)) if message == "Download cancelled by user"
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_group_halt_cancels_active_integrity_validation() {
        let result = run_active_cancellation(|group| {
            group
                .read()
                .unwrap()
                .request_halt(crate::request::request_group::HaltReason::UserRequest);
        })
        .await;
        assert!(matches!(
            result,
            Err(Aria2Error::DownloadFailed(message)) if message == "Download halted"
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_group_already_paused_cancels_queued_integrity_validation() {
        let man = shared_with_concurrency(1);
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(6),
            vec!["http://example.test/payload".to_string()],
            crate::request::request_group::DownloadOptions::default(),
        )));
        group.write().unwrap().pause().unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            enqueue_with_outcome_for_group(
                &man,
                group,
                Box::new(SlowIntegrityTask {
                    remaining_chunks: 100,
                    current_length: 0,
                }),
            ),
        )
        .await
        .expect("an already paused group must cancel queued validation promptly");

        assert!(matches!(
            result,
            Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
        ));
        assert_eq!(man.read().await.count_in_queue(), 0);
        assert_eq!(man.read().await.active_count(), 0);
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
    fn test_file_validator_rejects_mismatched_piece_count() {
        let result = FileChunkValidator::new(
            PathBuf::from("/tmp/payload.bin"),
            4,
            8,
            vec![sha1_hex(b"aaaa")],
            HashType::Sha1,
        );

        assert!(matches!(
            result,
            Err(Aria2Error::Parse(message)) if message.contains("digest count mismatch")
        ));
    }

    #[test]
    fn test_multi_file_validator_rejects_mismatched_piece_count() {
        let result = MultiFileChunkValidator::new(
            vec![(PathBuf::from("/tmp/first.bin"), 8)],
            4,
            8,
            vec![sha1_hex(b"aaaa")],
            HashType::Sha1,
        );

        assert!(matches!(
            result,
            Err(Aria2Error::Parse(message)) if message.contains("digest count mismatch")
        ));
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
                        vec!["aa".to_string(), "bb".to_string()],
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

    #[test]
    fn test_cancel_gid_notifies_only_matching_queued_entry() {
        let mut man = CheckIntegrityMan::new();
        let (target_tx, target_rx) = oneshot::channel();
        let (other_tx, mut other_rx) = oneshot::channel();

        for (gid, done_tx) in [(7, target_tx), (8, other_tx)] {
            man.push_entry(CheckIntegrityEntry {
                gid,
                task: Box::new(SlowIntegrityTask {
                    remaining_chunks: 1,
                    current_length: 0,
                }),
                created_at: Instant::now(),
                cancelled: Arc::new(AtomicBool::new(false)),
                progress: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                done_tx: Some(done_tx),
            });
        }

        assert!(man.cancel_gid(7));
        assert_eq!(man.count_in_queue(), 1);
        assert!(target_rx.blocking_recv().unwrap().is_err());
        assert!(other_rx.try_recv().is_err());
        assert_eq!(man.take_next_owned().unwrap().gid, 8);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_shared_instance_is_process_wide() {
        let a = shared();
        let b = shared();
        assert!(Arc::ptr_eq(&a, &b));
    }
}

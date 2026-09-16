use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Semaphore;
use tracing::warn;

use crate::checksum::checksum::Checksum;
use crate::checksum::message_digest::{HashType, MessageDigest};
use crate::error::{Aria2Error, Result};

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

const MAX_HASH_WORKERS: usize = 4;

fn hash_slots() -> &'static Arc<Semaphore> {
    static HASH_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    HASH_SLOTS.get_or_init(|| {
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, MAX_HASH_WORKERS);
        Arc::new(Semaphore::new(workers))
    })
}

async fn hash_bytes_async(algo: HashType, data: Vec<u8>) -> Result<Vec<u8>> {
    let permit = hash_slots()
        .clone()
        .acquire_owned()
        .await
        .map_err(|error| Aria2Error::Io(format!("integrity hash dispatcher closed: {error}")))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        Ok::<_, Aria2Error>(MessageDigest::hash_data(algo, &data))
    })
    .await
    .map_err(|error| Aria2Error::Io(format!("integrity hash task failed: {error}")))?
}

async fn update_digest_async(digest: MessageDigest, data: Vec<u8>) -> Result<MessageDigest> {
    let permit = hash_slots()
        .clone()
        .acquire_owned()
        .await
        .map_err(|error| Aria2Error::Io(format!("integrity hash dispatcher closed: {error}")))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut digest = digest;
        digest.update(&data);
        digest
    })
    .await
    .map_err(|error| Aria2Error::Io(format!("integrity hash task failed: {error}")))
}

async fn finalize_digest_async(digest: MessageDigest) -> Result<String> {
    let permit = hash_slots()
        .clone()
        .acquire_owned()
        .await
        .map_err(|error| Aria2Error::Io(format!("integrity hash dispatcher closed: {error}")))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        digest.finalize_hex()
    })
    .await
    .map_err(|error| Aria2Error::Io(format!("integrity hash task failed: {error}")))
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
        // Retry short reads and treat EOF before the requested length as a
        // truncated final chunk, so the digest reports a mismatch instead of
        // failing with an I/O error.
        let mut read = 0;
        while read < len {
            let n = file
                .read(&mut buf[read..])
                .await
                .map_err(|e| Aria2Error::Io(format!("read {}: {}", self.path.display(), e)))?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);

        let actual = hash_bytes_async(self.algo, buf).await?;

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
        let actual = hash_bytes_async(self.algo, data).await?;
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
            let digest = self
                .digest
                .take()
                .expect("checksum digest is present until EOF");
            let actual_hex = finalize_digest_async(digest).await?;
            self.passed = actual_hex.eq_ignore_ascii_case(&self.expected_hex);
            self.finished = true;
        } else {
            buffer.truncate(bytes_read);
            let digest = self
                .digest
                .take()
                .expect("checksum digest is present before EOF");
            self.digest = Some(update_digest_async(digest, buffer).await?);
            self.current_length += bytes_read as u64;
        }
        Ok(())
    }

    fn passed(&self) -> bool {
        self.finished && self.passed
    }
}

// ---------------------------------------------------------------------------

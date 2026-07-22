//! BT download progress persistence system
//!
//! Provides functionality to save BT download progress to `.aria2` control files,
//! with atomic write support and automatic detection of C++ binary format vs text format.
//! Binary format (v0/v1) is compatible with the original C++ aria2 implementation.

use std::fmt::{Display, Formatter};
use std::fs;
use std::io::{Cursor, Read as IoRead, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha1::{Digest, Sha1};

use crate::error::{Aria2Error, FatalError, Result};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Length of the BT info-hash in bytes
pub const INFO_HASH_LENGTH: usize = 20;

/// Default block length used for in-flight piece bitfield calculation
pub const DEFAULT_BLOCK_LENGTH: u32 = 16 * 1024;

/// File suffix for progress control files
pub const CTRL_FILE_SUFFIX: &str = ".aria2";

/// Temporary file suffix used during atomic writes
const TEMP_FILE_SUFFIX: &str = "__temp";

/// Binary format version 1 (network byte order)
const FORMAT_VERSION_V1: u16 = 1;

/// Extension bit indicating a BitTorrent download
const EXTENSION_BT: u32 = 0x00000001;

// ---------------------------------------------------------------------------
// InFlightPiece
// ---------------------------------------------------------------------------

/// Represents a partially downloaded piece that is currently in progress.
///
/// When saving progress, in-flight pieces are serialized so that downloads
/// can resume exactly where they left off, without re-downloading blocks
/// that have already been completed within a piece.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InFlightPiece {
    /// Zero-based piece index
    pub index: u32,
    /// Total length of this piece in bytes
    pub length: u32,
    /// Per-block completion bitmap within this piece
    pub bitfield: Vec<u8>,
}

impl InFlightPiece {
    /// Create a new in-flight piece record
    pub fn new(index: u32, length: u32, bitfield: Vec<u8>) -> Self {
        Self {
            index,
            length,
            bitfield,
        }
    }

    /// Calculate the expected bitfield length for a piece of the given length.
    ///
    /// Each block is `DEFAULT_BLOCK_LENGTH` bytes; one bit per block, rounded
    /// up to whole bytes.
    pub fn expected_bitfield_len(piece_length: u32) -> usize {
        if piece_length == 0 {
            return 0;
        }
        let block_length: u64 = DEFAULT_BLOCK_LENGTH as u64;
        let num_blocks = ((piece_length as u64 + block_length - 1) / block_length) as usize;
        (num_blocks + 7) / 8
    }
}

// ---------------------------------------------------------------------------
// PeerAddr
// ---------------------------------------------------------------------------

/// BT download progress Peer address information
#[derive(Clone, Debug)]
pub struct PeerAddr {
    /// IP address
    pub ip: String,
    /// Port number
    pub port: u16,
}

impl Display for PeerAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.ip, self.port)
    }
}

// ---------------------------------------------------------------------------
// DownloadStats
// ---------------------------------------------------------------------------

/// BT download statistics
#[derive(Clone, Debug, Default)]
pub struct DownloadStats {
    /// Uploaded bytes
    pub uploaded_bytes: u64,
    /// Downloaded bytes
    pub downloaded_bytes: u64,
    /// Upload speed (bytes/sec)
    pub upload_speed: f64,
    /// Download speed (bytes/sec)
    pub download_speed: f64,
    /// Elapsed time (seconds)
    pub elapsed_seconds: u64,
}

// ---------------------------------------------------------------------------
// BtProgress
// ---------------------------------------------------------------------------

/// BT download progress data structure
///
/// Contains all state information for BT download, used for persisting to disk.
/// Supports both binary (C++ compatible) and text format serialization.
#[derive(Clone, Debug)]
pub struct BtProgress {
    /// Torrent info_hash
    pub info_hash: [u8; 20],
    /// Downloaded piece bitmap
    pub bitfield: Vec<u8>,
    /// Connected peer list
    pub peers: Vec<PeerAddr>,
    /// Download statistics
    pub stats: DownloadStats,
    /// Length of each piece
    pub piece_length: u32,
    /// Total size
    pub total_size: u64,
    /// Total number of pieces
    pub num_pieces: u32,
    /// Total uploaded bytes (persisted across sessions)
    pub upload_length: u64,
    /// In-flight partially downloaded pieces
    pub in_flight_pieces: Vec<InFlightPiece>,
    /// Whether this is a BitTorrent download (affects binary format extension bit)
    pub is_torrent: bool,
    /// Save time
    pub save_time: SystemTime,
    /// Format version number
    pub version: u32,
}

impl Default for BtProgress {
    fn default() -> Self {
        BtProgress {
            info_hash: [0u8; 20],
            bitfield: Vec::new(),
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 0,
            total_size: 0,
            num_pieces: 0,
            upload_length: 0,
            in_flight_pieces: Vec::new(),
            is_torrent: true,
            save_time: SystemTime::UNIX_EPOCH,
            version: 1,
        }
    }
}

impl BtProgress {
    /// Convert info_hash to 40-character hex string
    ///
    /// # Returns
    ///
    /// Returns lowercase hex string representation
    pub fn to_hex_hash(&self) -> String {
        self.info_hash
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect()
    }

    /// Calculate completion percentage
    ///
    /// Calculates download completion ratio based on bits set in bitfield.
    ///
    /// # Returns
    ///
    /// Returns completion ratio between 0.0 and 1.0
    pub fn completion_ratio(&self) -> f64 {
        if self.num_pieces == 0 || self.bitfield.is_empty() {
            return 0.0;
        }

        let mut set_bits = 0u32;
        for &byte in &self.bitfield {
            set_bits += byte.count_ones();
        }

        set_bits as f64 / self.num_pieces as f64
    }
}

// ---------------------------------------------------------------------------
// Binary serialization helpers
// ---------------------------------------------------------------------------

/// Serialize a `BtProgress` into C++ compatible binary format (version 1, big-endian).
fn serialize_binary(progress: &BtProgress) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256 + progress.bitfield.len());

    // Version (16 bits, big-endian)
    buf.extend_from_slice(&FORMAT_VERSION_V1.to_be_bytes());

    // Extension (32 bits): set BT bit if this is a torrent download
    let ext: u32 = if progress.is_torrent {
        EXTENSION_BT
    } else {
        0
    };
    buf.extend_from_slice(&ext.to_be_bytes());

    // InfoHash length + hash data (only written for torrent downloads)
    if progress.is_torrent {
        buf.extend_from_slice(&(INFO_HASH_LENGTH as u32).to_be_bytes());
        buf.extend_from_slice(&progress.info_hash);
    } else {
        buf.extend_from_slice(&0u32.to_be_bytes());
    }

    // pieceLength (32 bits)
    buf.extend_from_slice(&progress.piece_length.to_be_bytes());

    // totalLength (64 bits)
    buf.extend_from_slice(&progress.total_size.to_be_bytes());

    // uploadLength (64 bits)
    buf.extend_from_slice(&progress.upload_length.to_be_bytes());

    // bitfieldLength (32 bits) + bitfield
    buf.extend_from_slice(&(progress.bitfield.len() as u32).to_be_bytes());
    buf.extend_from_slice(&progress.bitfield);

    // In-flight pieces
    buf.extend_from_slice(&(progress.in_flight_pieces.len() as u32).to_be_bytes());
    for piece in &progress.in_flight_pieces {
        buf.extend_from_slice(&piece.index.to_be_bytes());
        buf.extend_from_slice(&piece.length.to_be_bytes());
        buf.extend_from_slice(&(piece.bitfield.len() as u32).to_be_bytes());
        buf.extend_from_slice(&piece.bitfield);
    }

    buf
}

/// Compute SHA-1 digest of the given data.
fn compute_sha1(data: &[u8]) -> [u8; 20] {
    let result = Sha1::digest(data);
    let mut digest = [0u8; 20];
    digest.copy_from_slice(result.as_slice());
    digest
}

/// Detected binary format version.
enum DetectedFormat {
    /// Version 0: host byte order (legacy C++ format)
    BinaryV0,
    /// Version 1: network byte order (current C++ format)
    BinaryV1,
    /// Not a recognized binary format; treat as text
    Text,
}

/// Detect the format of a progress file by inspecting the first two bytes.
fn detect_format(data: &[u8]) -> DetectedFormat {
    if data.len() < 2 {
        return DetectedFormat::Text;
    }
    let version = u16::from_be_bytes([data[0], data[1]]);
    match version {
        0 => DetectedFormat::BinaryV0,
        1 => DetectedFormat::BinaryV1,
        _ => DetectedFormat::Text,
    }
}

/// Read a big-endian u32 from the cursor (used for BinaryV1).
fn read_u32_be(cursor: &mut Cursor<&[u8]>) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    cursor.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

/// Read a native-endian u32 from the cursor (used for BinaryV0).
fn read_u32_ne(cursor: &mut Cursor<&[u8]>) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    cursor.read_exact(&mut b)?;
    Ok(u32::from_ne_bytes(b))
}

/// Read a big-endian u64 from the cursor (used for BinaryV1).
fn read_u64_be(cursor: &mut Cursor<&[u8]>) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    cursor.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

/// Read a native-endian u64 from the cursor (used for BinaryV0).
fn read_u64_ne(cursor: &mut Cursor<&[u8]>) -> std::io::Result<u64> {
    let mut b = [0u8; 8];
    cursor.read_exact(&mut b)?;
    Ok(u64::from_ne_bytes(b))
}

/// Deserialize binary format progress data.
///
/// Supports both version 0 (host byte order) and version 1 (network byte order).
/// If `validate_hash` is true and the file contains a BT info-hash, it will be
/// compared against `expected_hash`.
fn deserialize_binary(
    data: &[u8],
    expected_hash: &[u8; 20],
    validate_hash: bool,
) -> Result<BtProgress> {
    let format = detect_format(data);
    let mut cursor = Cursor::new(data);

    // Select the appropriate byte-order readers based on detected format
    let (read_u32, read_u64): (
        fn(&mut Cursor<&[u8]>) -> std::io::Result<u32>,
        fn(&mut Cursor<&[u8]>) -> std::io::Result<u64>,
    ) = match format {
        DetectedFormat::BinaryV1 => (read_u32_be, read_u64_be),
        DetectedFormat::BinaryV0 => (read_u32_ne, read_u64_ne),
        DetectedFormat::Text => (read_u32_be, read_u64_be),
    };

    // Version (16 bits)
    let mut vbuf = [0u8; 2];
    cursor
        .read_exact(&mut vbuf)
        .map_err(|e| Aria2Error::Io(format!("Failed to read version: {}", e)))?;
    let version = u16::from_be_bytes(vbuf);

    // Extension (32 bits) - check BT bit
    let mut ext_buf = [0u8; 4];
    cursor
        .read_exact(&mut ext_buf)
        .map_err(|e| Aria2Error::Io(format!("Failed to read extension: {}", e)))?;
    let is_bt = (ext_buf[3] & 1) != 0;

    // InfoHash length (32 bits) + hash bytes
    let hash_len = read_u32(&mut cursor)
        .map_err(|e| Aria2Error::Io(format!("Failed to read hash length: {}", e)))?;
    let mut saved_hash = [0u8; 20];
    if hash_len > 0 && hash_len as usize <= INFO_HASH_LENGTH {
        cursor
            .read_exact(&mut saved_hash[..hash_len as usize])
            .map_err(|e| Aria2Error::Io(format!("Failed to read info hash: {}", e)))?;
        if validate_hash
            && is_bt
            && saved_hash[..hash_len as usize] != expected_hash[..hash_len as usize]
        {
            return Err(Aria2Error::Io("info hash mismatch".to_string()));
        }
    }

    // pieceLength (32 bits)
    let piece_length = read_u32(&mut cursor)
        .map_err(|e| Aria2Error::Io(format!("Failed to read piece length: {}", e)))?;
    if piece_length == 0 {
        return Err(Aria2Error::Io(
            "piece length must not be 0".to_string(),
        ));
    }

    // totalLength (64 bits)
    let total_length = read_u64(&mut cursor)
        .map_err(|e| Aria2Error::Io(format!("Failed to read total length: {}", e)))?;

    // uploadLength (64 bits)
    let upload_length = read_u64(&mut cursor)
        .map_err(|e| Aria2Error::Io(format!("Failed to read upload length: {}", e)))?;

    // bitfieldLength (32 bits) + bitfield data
    let bf_len = read_u32(&mut cursor)
        .map_err(|e| Aria2Error::Io(format!("Failed to read bitfield length: {}", e)))?;
    let mut bitfield = vec![0u8; bf_len as usize];
    cursor
        .read_exact(&mut bitfield)
        .map_err(|e| Aria2Error::Io(format!("Failed to read bitfield: {}", e)))?;

    // Derive numPieces from totalLength and pieceLength
    let num_pieces = ((total_length + piece_length as u64 - 1) / piece_length as u64) as u32;

    // In-flight pieces
    let num_in_flight = read_u32(&mut cursor)
        .map_err(|e| Aria2Error::Io(format!("Failed to read num in-flight: {}", e)))?;
    let mut in_flight_pieces = Vec::with_capacity(num_in_flight as usize);
    for _ in 0..num_in_flight {
        let idx = read_u32(&mut cursor)
            .map_err(|e| Aria2Error::Io(format!("Failed to read piece index: {}", e)))?;
        let len = read_u32(&mut cursor)
            .map_err(|e| Aria2Error::Io(format!("Failed to read piece length: {}", e)))?;
        let pbf_len = read_u32(&mut cursor).map_err(|e| {
            Aria2Error::Io(format!("Failed to read piece bitfield length: {}", e))
        })?;
        let mut pbf = vec![0u8; pbf_len as usize];
        cursor.read_exact(&mut pbf).map_err(|e| {
            Aria2Error::Io(format!("Failed to read piece bitfield: {}", e))
        })?;
        in_flight_pieces.push(InFlightPiece::new(idx, len, pbf));
    }

    Ok(BtProgress {
        info_hash: saved_hash,
        bitfield,
        peers: Vec::new(),
        stats: DownloadStats {
            uploaded_bytes: upload_length,
            ..Default::default()
        },
        piece_length,
        total_size: total_length,
        num_pieces,
        upload_length,
        in_flight_pieces,
        is_torrent: is_bt,
        save_time: SystemTime::now(),
        version: version as u32,
    })
}

// ---------------------------------------------------------------------------
// BtProgressManager
// ---------------------------------------------------------------------------

/// BT progress file manager
///
/// Manages BT download progress save, load, delete, and list operations.
/// Supports both binary (C++ compatible) and text format, with automatic
/// format detection on load. Uses atomic writes to ensure existing progress
/// files are not corrupted in abnormal situations.
pub struct BtProgressManager {
    /// Progress file storage directory
    progress_dir: PathBuf,
    /// SHA-1 digest of the last saved progress; used for deduplication
    /// to avoid writing identical control files.
    last_digest: Option<[u8; 20]>,
}

impl BtProgressManager {
    /// Create new BT progress manager
    ///
    /// Automatically creates specified directory if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `progress_dir` - Progress file storage directory path
    ///
    /// # Errors
    ///
    /// Returns error when unable to create directory
    pub fn new(progress_dir: &Path) -> Result<Self> {
        fs::create_dir_all(progress_dir).map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!(
                "Failed to create progress directory {}: {}",
                progress_dir.display(),
                e
            )))
        })?;

        info!(path = %progress_dir.display(), "BT progress manager initialized");

        Ok(BtProgressManager {
            progress_dir: progress_dir.to_path_buf(),
            last_digest: None,
        })
    }

    /// Generate progress file path using the standard `.aria2` suffix
    pub fn get_progress_file_path(&self, info_hash: &[u8; 20]) -> PathBuf {
        let hex_hash: String = info_hash.iter().map(|b| format!("{:02x}", b)).collect();
        self.progress_dir.join(format!("{}{}", hex_hash, CTRL_FILE_SUFFIX))
    }

    /// Check whether a progress file exists for the given info_hash
    pub fn exists(&self, info_hash: &[u8; 20]) -> bool {
        self.get_progress_file_path(info_hash).is_file()
    }

    /// Save BT download progress to file using binary format.
    ///
    /// Uses atomic write strategy: writes to a `__temp` suffixed temporary
    /// file first, then replaces the original file via rename operation,
    /// ensuring existing progress files are not corrupted if exceptions
    /// occur during write process.
    ///
    /// # Arguments
    ///
    /// * `info_hash` - Torrent info_hash (20 bytes)
    /// * `progress` - Progress data to save
    ///
    /// # Errors
    ///
    /// Returns error when file write fails
    pub fn save_progress(&self, info_hash: &[u8; 20], progress: &BtProgress) -> Result<()> {
        let file_path = self.get_progress_file_path(info_hash);

        debug!(
            path = %file_path.display(),
            hash = %progress.to_hex_hash(),
            "Saving BT progress (binary format)"
        );

        // Temporary file with __temp suffix (matching C++ aria2 convention)
        let tmp_path = {
            let mut s = file_path.as_os_str().to_owned();
            s.push(TEMP_FILE_SUFFIX);
            PathBuf::from(s)
        };

        let content = serialize_binary(progress);
        {
            let mut file = fs::File::create(&tmp_path).map_err(|e| {
                Aria2Error::Io(format!("Failed to create temporary progress file: {}", e))
            })?;

            file.write_all(&content)
                .map_err(|e| Aria2Error::Io(format!("Failed to write progress data: {}", e)))?;

            file.flush().map_err(|e| {
                Aria2Error::Io(format!("Failed to flush progress file buffer: {}", e))
            })?;
        }

        // Atomic rename
        fs::rename(&tmp_path, &file_path).map_err(|e| {
            // Clean up temporary file
            let _ = fs::remove_file(&tmp_path);
            Aria2Error::Io(format!("Failed to rename progress file: {}", e))
        })?;

        info!(
            path = %file_path.display(),
            pieces = progress.num_pieces,
            ratio = progress.completion_ratio(),
            "BT progress saved successfully"
        );

        Ok(())
    }

    /// Save progress with SHA-1 deduplication.
    ///
    /// Computes a SHA-1 digest of the serialized binary data and compares
    /// it with the digest from the previous save. If the data is unchanged,
    /// the write is skipped entirely, reducing unnecessary disk I/O.
    ///
    /// Returns `true` if the file was actually written, `false` if skipped
    /// because the content was unchanged.
    pub fn save_progress_with_dedup(
        &mut self,
        info_hash: &[u8; 20],
        progress: &BtProgress,
    ) -> Result<bool> {
        let content = serialize_binary(progress);
        let digest = compute_sha1(&content);

        if self.last_digest == Some(digest) {
            debug!("Progress unchanged, skipping save (dedup)");
            return Ok(false);
        }

        let file_path = self.get_progress_file_path(info_hash);

        debug!(
            path = %file_path.display(),
            hash = %progress.to_hex_hash(),
            "Saving BT progress (binary format, dedup)"
        );

        let tmp_path = {
            let mut s = file_path.as_os_str().to_owned();
            s.push(TEMP_FILE_SUFFIX);
            PathBuf::from(s)
        };

        {
            let mut file = fs::File::create(&tmp_path).map_err(|e| {
                Aria2Error::Io(format!("Failed to create temporary progress file: {}", e))
            })?;

            file.write_all(&content)
                .map_err(|e| Aria2Error::Io(format!("Failed to write progress data: {}", e)))?;

            file.flush().map_err(|e| {
                Aria2Error::Io(format!("Failed to flush progress file buffer: {}", e))
            })?;
        }

        // Atomic rename
        fs::rename(&tmp_path, &file_path).map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            Aria2Error::Io(format!("Failed to rename progress file: {}", e))
        })?;

        self.last_digest = Some(digest);

        info!(
            path = %file_path.display(),
            pieces = progress.num_pieces,
            ratio = progress.completion_ratio(),
            "BT progress saved successfully (dedup)"
        );

        Ok(true)
    }

    /// Load BT download progress file.
    ///
    /// Automatically detects the file format by reading the first two bytes:
    /// - If they decode to version 0 or 1 (big-endian u16), the file is treated
    ///   as C++ binary format.
    /// - Otherwise, the file is parsed as text format.
    ///
    /// Validates whether info_hash in file matches provided parameter when
    /// the binary format includes a BT extension bit.
    ///
    /// # Arguments
    ///
    /// * `info_hash` - Expected info_hash (20 bytes) for validation
    ///
    /// # Returns
    ///
    /// Returns loaded progress data
    ///
    /// # Errors
    ///
    /// - File doesn't exist or read failure
    /// - File format invalid or corrupted
    /// - info_hash mismatch
    pub fn load_progress(&self, info_hash: &[u8; 20]) -> Result<BtProgress> {
        let file_path = self.get_progress_file_path(info_hash);

        debug!(
            path = %file_path.display(),
            hash = %info_hash.iter().map(|b| format!("{:02x}", b)).collect::<String>(),
            "Loading BT progress"
        );

        // Read raw bytes to allow both binary and text detection
        let raw_data = fs::read(&file_path).map_err(|e| {
            Aria2Error::Io(format!("Failed to read progress file: {}", e))
        })?;

        if raw_data.is_empty() {
            return Err(Aria2Error::Io("Progress file is empty".to_string()));
        }

        // Detect format from the first two bytes
        match detect_format(&raw_data) {
            DetectedFormat::BinaryV1 | DetectedFormat::BinaryV0 => {
                debug!("Detected binary format, parsing as C++ compatible");
                deserialize_binary(&raw_data, info_hash, true)
            }
            DetectedFormat::Text => {
                // Interpret as UTF-8 text
                let content = String::from_utf8(raw_data).map_err(|e| {
                    Aria2Error::Io(format!("Failed to parse progress file as text: {}", e))
                })?;
                self.parse_text_format(info_hash, &content, &file_path)
            }
        }
    }

    /// Serialize BtProgress to text format.
    ///
    /// Retained for backward compatibility and human-readable output.
    /// Uses `write!` macro instead of `format!` + `push_str` to reduce
    /// memory allocations. Pre-allocates output buffer based on estimated
    /// size to avoid repeated reallocations.
    #[allow(dead_code)]
    fn serialize_progress_text(&self, progress: &BtProgress) -> String {
        let estimated_size = 512 + progress.peers.len() * 24 + progress.bitfield.len() * 3;
        let mut output = String::with_capacity(estimated_size);

        use std::fmt::Write;
        output.push_str("[Download]\n");
        let _ = writeln!(output, "info_hash={}", progress.to_hex_hash());
        let _ = writeln!(output, "version={}", progress.version);
        let _ = writeln!(output, "num_pieces={}", progress.num_pieces);
        let _ = writeln!(output, "piece_length={}", progress.piece_length);
        let _ = writeln!(output, "total_size={}", progress.total_size);
        let _ = writeln!(output, "downloaded={}", progress.stats.downloaded_bytes);
        let _ = writeln!(output, "uploaded={}", progress.upload_length);
        let _ = writeln!(output, "elapsed={}", progress.stats.elapsed_seconds);

        let _ = write!(output, "bitfield=");
        for &byte in &progress.bitfield {
            let _ = write!(output, "{:02x}", byte);
        }
        let _ = writeln!(output);

        output.push_str("[Peers]\n");
        for peer in &progress.peers {
            let _ = writeln!(output, "{}", peer);
        }

        output
    }

    /// Parse text format progress file
    fn parse_text_format(
        &self,
        expected_hash: &[u8; 20],
        content: &str,
        file_path: &Path,
    ) -> Result<BtProgress> {
        let mut progress = BtProgress {
            info_hash: *expected_hash,
            bitfield: Vec::new(),
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 0,
            total_size: 0,
            num_pieces: 0,
            upload_length: 0,
            in_flight_pieces: Vec::new(),
            is_torrent: true,
            save_time: SystemTime::now(),
            version: 1,
        };

        let mut current_section = String::new();

        for line in content.lines() {
            let line = line.trim();

            if line.is_empty() {
                continue;
            }

            // Detect section header
            if line.starts_with('[') && line.ends_with(']') {
                current_section = line[1..line.len() - 1].to_string();
                continue;
            }

            match current_section.as_str() {
                "Download" => {
                    if let Some((key, value)) = line.split_once('=') {
                        match key.trim() {
                            "info_hash" => {
                                let file_hash = value.trim().to_lowercase();
                                let expected_hex: String =
                                    expected_hash.iter().map(|b| format!("{:02x}", b)).collect();
                                if file_hash != expected_hex {
                                    return Err(Aria2Error::Io(format!(
                                        "Progress file info_hash mismatch: file={}, expected={}",
                                        file_hash, expected_hex
                                    )));
                                }
                            }
                            "version" => {
                                progress.version = value.trim().parse::<u32>().unwrap_or(1);
                            }
                            "num_pieces" => {
                                progress.num_pieces = value.trim().parse::<u32>().unwrap_or(0);
                            }
                            "piece_length" => {
                                progress.piece_length = value.trim().parse::<u32>().unwrap_or(0);
                            }
                            "total_size" => {
                                progress.total_size = value.trim().parse::<u64>().unwrap_or(0);
                            }
                            "downloaded" => {
                                progress.stats.downloaded_bytes =
                                    value.trim().parse::<u64>().unwrap_or(0);
                            }
                            "uploaded" => {
                                progress.upload_length =
                                    value.trim().parse::<u64>().unwrap_or(0);
                                progress.stats.uploaded_bytes = progress.upload_length;
                            }
                            "elapsed" => {
                                progress.stats.elapsed_seconds =
                                    value.trim().parse::<u64>().unwrap_or(0);
                            }
                            "bitfield" => {
                                progress.bitfield = parse_bitfield_hex(value.trim());
                            }
                            _ => {}
                        }
                    }
                }
                "Peers" => {
                    let peer = parse_peer_addr(line);
                    if let Some(p) = peer {
                        progress.peers.push(p);
                    }
                }
                _ => {}
            }
        }

        info!(
            path = %file_path.display(),
            pieces = progress.num_pieces,
            ratio = progress.completion_ratio(),
            "BT progress loaded successfully"
        );

        Ok(progress)
    }

    /// Delete progress file for specified info_hash
    pub fn remove_progress(&self, info_hash: &[u8; 20]) -> Result<()> {
        let file_path = self.get_progress_file_path(info_hash);

        debug!(
            hash = %info_hash.iter().map(|b| format!("{:02x}", b)).collect::<String>(),
            "Deleting BT progress"
        );

        if file_path.exists() {
            fs::remove_file(&file_path)
                .map_err(|e| Aria2Error::Io(format!("Failed to delete progress file: {}", e)))?;

            info!(path = %file_path.display(), "BT progress file deleted");
        } else {
            warn!(path = %file_path.display(), "Progress file does not exist");
        }

        Ok(())
    }

    /// List all saved progress files
    ///
    /// Scans all `.aria2` files in progress directory, extracts their info_hash.
    pub fn list_saved_progresses(&self) -> Vec<[u8; 20]> {
        let mut hashes = Vec::new();

        if let Ok(entries) = fs::read_dir(&self.progress_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name() {
                    let name_str = name.to_string_lossy();
                    if let Some(hex_hash) = name_str.strip_suffix(CTRL_FILE_SUFFIX) {
                        if let Ok(hash) = Self::hex_to_info_hash(hex_hash) {
                            hashes.push(hash);
                        } else {
                            warn!(
                                filename = %name_str,
                                "Cannot parse info_hash from progress file name"
                            );
                        }
                    }
                }
            }
        }

        debug!(count = hashes.len(), "Listed all saved BT progress");

        hashes
    }

    /// Convert hex string to info_hash
    fn hex_to_info_hash(hex_str: &str) -> std::result::Result<[u8; 20], ()> {
        if hex_str.len() != 40 {
            return Err(());
        }

        let mut hash = [0u8; 20];
        for (i, byte) in hash.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
        }

        Ok(hash)
    }
}

// ---------------------------------------------------------------------------
// Free functions (extracted from BtProgressManager for reuse)
// ---------------------------------------------------------------------------

/// Parse bitfield hex string into bytes
fn parse_bitfield_hex(hex_str: &str) -> Vec<u8> {
    let hex_str = hex_str.trim();
    if hex_str.is_empty() {
        return Vec::new();
    }

    (0..hex_str.len())
        .step_by(2)
        .filter_map(|i| {
            if i + 1 < hex_str.len() {
                u8::from_str_radix(&hex_str[i..i + 2], 16).ok()
            } else {
                None
            }
        })
        .collect()
}

/// Parse peer address string (e.g. "192.168.1.1:6881")
fn parse_peer_addr(addr_str: &str) -> Option<PeerAddr> {
    let addr_str = addr_str.trim();
    if addr_str.is_empty() {
        return None;
    }

    // Find last colon (IPv6 addresses may contain multiple colons)
    if let Some(colon_pos) = addr_str.rfind(':') {
        let ip = addr_str[..colon_pos].trim().to_string();
        let port: u16 = addr_str[colon_pos + 1..].trim().parse().ok()?;

        Some(PeerAddr { ip, port })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_addr_display() {
        let peer = PeerAddr {
            ip: "192.168.1.1".to_string(),
            port: 6881,
        };
        assert_eq!(format!("{}", peer), "192.168.1.1:6881");
    }

    #[test]
    fn test_bt_progress_to_hex_hash() {
        let progress = BtProgress {
            info_hash: [
                0xAB, 0xCD, 0x12, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            ..Default::default()
        };
        assert_eq!(
            progress.to_hex_hash(),
            "abcd123400000000000000000000000000000000"
        );
    }

    #[test]
    fn test_completion_ratio_zero_pieces() {
        let progress = BtProgress {
            num_pieces: 0,
            bitfield: vec![],
            ..Default::default()
        };
        assert_eq!(progress.completion_ratio(), 0.0);
    }

    #[test]
    fn test_completion_ratio_full() {
        let progress = BtProgress {
            num_pieces: 4,
            bitfield: vec![0xFF],
            ..Default::default()
        };
        assert!(progress.completion_ratio() > 0.0);
    }

    #[test]
    fn test_parse_bitfield_hex() {
        let result = parse_bitfield_hex("ff00ff");
        assert_eq!(result, vec![0xFF, 0x00, 0xFF]);
    }

    #[test]
    fn test_parse_bitfield_empty() {
        let result = parse_bitfield_hex("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_peer_addr_ipv4() {
        let peer = parse_peer_addr("192.168.1.100:6881").unwrap();
        assert_eq!(peer.ip, "192.168.1.100");
        assert_eq!(peer.port, 6881);
    }

    #[test]
    fn test_parse_peer_addr_invalid() {
        assert!(parse_peer_addr("invalid").is_none());
        assert!(parse_peer_addr("").is_none());
    }

    #[test]
    fn test_hex_to_info_hash_valid() {
        let hex = "abcdef1234567890abcdef1234567890abcdef12";
        let hash = BtProgressManager::hex_to_info_hash(hex).unwrap();
        assert_eq!(
            hash,
            [
                0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56, 0x78, 0x90, 0xAB, 0xCD, 0xEF, 0x12, 0x34,
                0x56, 0x78, 0x90, 0xAB, 0xCD, 0xEF, 0x12
            ]
        );
    }

    #[test]
    fn test_hex_to_info_hash_invalid_length() {
        assert!(BtProgressManager::hex_to_info_hash("abc123").is_err());
    }

    #[test]
    fn test_in_flight_piece_expected_bitfield_len() {
        // piece_length = 262144 (256 KiB), block_length = 16384
        // num_blocks = ceil(262144 / 16384) = 16
        // bitfield_len = ceil(16 / 8) = 2
        assert_eq!(InFlightPiece::expected_bitfield_len(262144), 2);

        // piece_length = 32768, block_length = 16384
        // num_blocks = 2, bitfield_len = 1
        assert_eq!(InFlightPiece::expected_bitfield_len(32768), 1);

        // Zero piece length
        assert_eq!(InFlightPiece::expected_bitfield_len(0), 0);
    }

    #[test]
    fn test_binary_roundtrip() {
        let hash: [u8; 20] = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB,
            0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67,
        ];
        let progress = BtProgress {
            info_hash: hash,
            bitfield: vec![0xFF, 0x0F],
            peers: Vec::new(),
            stats: DownloadStats {
                uploaded_bytes: 12345,
                ..Default::default()
            },
            piece_length: 262144,
            total_size: 1048576,
            num_pieces: 4,
            upload_length: 12345,
            in_flight_pieces: vec![
                InFlightPiece::new(0, 262144, vec![0xFF]),
                InFlightPiece::new(2, 262144, vec![0x80]),
            ],
            is_torrent: true,
            save_time: SystemTime::now(),
            version: 1,
        };

        let data = serialize_binary(&progress);
        let loaded = deserialize_binary(&data, &hash, true).unwrap();

        assert_eq!(loaded.info_hash, hash);
        assert_eq!(loaded.bitfield, vec![0xFF, 0x0F]);
        assert_eq!(loaded.piece_length, 262144);
        assert_eq!(loaded.total_size, 1048576);
        assert_eq!(loaded.num_pieces, 4);
        assert_eq!(loaded.upload_length, 12345);
        assert!(loaded.is_torrent);
        assert_eq!(loaded.in_flight_pieces.len(), 2);
        assert_eq!(loaded.in_flight_pieces[0].index, 0);
        assert_eq!(loaded.in_flight_pieces[0].length, 262144);
        assert_eq!(loaded.in_flight_pieces[0].bitfield, vec![0xFF]);
        assert_eq!(loaded.in_flight_pieces[1].index, 2);
        assert_eq!(loaded.in_flight_pieces[1].bitfield, vec![0x80]);
    }

    #[test]
    fn test_binary_non_torrent() {
        let hash: [u8; 20] = [0u8; 20];
        let progress = BtProgress {
            info_hash: hash,
            bitfield: vec![0xAA],
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 1048576,
            total_size: 10485760,
            num_pieces: 10,
            upload_length: 0,
            in_flight_pieces: Vec::new(),
            is_torrent: false,
            save_time: SystemTime::now(),
            version: 1,
        };

        let data = serialize_binary(&progress);
        let loaded = deserialize_binary(&data, &hash, false).unwrap();

        assert!(!loaded.is_torrent);
        assert_eq!(loaded.bitfield, vec![0xAA]);
        assert_eq!(loaded.upload_length, 0);
        assert!(loaded.in_flight_pieces.is_empty());
    }

    #[test]
    fn test_detect_format() {
        // Version 0 (host byte order for u16 0)
        assert!(matches!(detect_format(&[0x00, 0x00]), DetectedFormat::BinaryV0));

        // Version 1 (big-endian u16 1)
        assert!(matches!(detect_format(&[0x00, 0x01]), DetectedFormat::BinaryV1));

        // Too short
        assert!(matches!(detect_format(&[0x00]), DetectedFormat::Text));

        // Unknown version -> text
        assert!(matches!(detect_format(&[0x00, 0x05]), DetectedFormat::Text));
    }

    #[test]
    fn test_binary_hash_mismatch() {
        let hash: [u8; 20] = [0x01; 20];
        let progress = BtProgress {
            info_hash: hash,
            bitfield: vec![0xFF],
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 262144,
            total_size: 1048576,
            num_pieces: 4,
            upload_length: 0,
            in_flight_pieces: Vec::new(),
            is_torrent: true,
            save_time: SystemTime::now(),
            version: 1,
        };

        let data = serialize_binary(&progress);
        let wrong_hash: [u8; 20] = [0x02; 20];
        let result = deserialize_binary(&data, &wrong_hash, true);
        assert!(result.is_err());
    }

    #[test]
    fn test_save_and_load_binary() {
        let dir = std::env::temp_dir().join("bt_progress_binary_test");
        let _ = fs::create_dir_all(&dir);
        let manager = BtProgressManager::new(&dir).unwrap();

        let hash: [u8; 20] = [
            0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
        ];
        let progress = BtProgress {
            info_hash: hash,
            bitfield: vec![0xFF, 0x0F, 0x03],
            peers: vec![PeerAddr {
                ip: "10.0.0.1".to_string(),
                port: 6881,
            }],
            stats: DownloadStats {
                uploaded_bytes: 5000,
                downloaded_bytes: 900000,
                ..Default::default()
            },
            piece_length: 524288,
            total_size: 2097152,
            num_pieces: 4,
            upload_length: 5000,
            in_flight_pieces: Vec::new(),
            is_torrent: true,
            save_time: SystemTime::now(),
            version: 1,
        };

        // Save and reload
        manager.save_progress(&hash, &progress).unwrap();
        let loaded = manager.load_progress(&hash).unwrap();

        assert_eq!(loaded.info_hash, hash);
        assert_eq!(loaded.bitfield, vec![0xFF, 0x0F, 0x03]);
        assert_eq!(loaded.piece_length, 524288);
        assert_eq!(loaded.total_size, 2097152);
        assert_eq!(loaded.upload_length, 5000);
        assert_eq!(loaded.stats.uploaded_bytes, 5000);
        assert!(loaded.is_torrent);
        // Note: peers are not serialized in binary format
        assert!(loaded.peers.is_empty());

        // Cleanup
        let _ = manager.remove_progress(&hash);
    }

    #[test]
    fn test_save_progress_with_dedup() {
        let dir = std::env::temp_dir().join("bt_progress_dedup_test");
        let _ = fs::create_dir_all(&dir);
        let mut manager = BtProgressManager::new(&dir).unwrap();

        let hash: [u8; 20] = [0x42; 20];
        let progress = BtProgress {
            info_hash: hash,
            bitfield: vec![0xFF],
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 262144,
            total_size: 1048576,
            num_pieces: 4,
            upload_length: 0,
            in_flight_pieces: Vec::new(),
            is_torrent: true,
            save_time: SystemTime::now(),
            version: 1,
        };

        // First save should write
        let written = manager.save_progress_with_dedup(&hash, &progress).unwrap();
        assert!(written);

        // Second save with identical data should be skipped
        let written = manager.save_progress_with_dedup(&hash, &progress).unwrap();
        assert!(!written);

        // Cleanup
        let _ = manager.remove_progress(&hash);
    }

    #[test]
    fn test_text_format_roundtrip() {
        let dir = std::env::temp_dir().join("bt_progress_text_test");
        let _ = fs::create_dir_all(&dir);
        let manager = BtProgressManager::new(&dir).unwrap();

        let hash: [u8; 20] = [
            0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let progress = BtProgress {
            info_hash: hash,
            bitfield: vec![0xFF, 0x00],
            peers: vec![PeerAddr {
                ip: "192.168.1.1".to_string(),
                port: 6881,
            }],
            stats: DownloadStats {
                uploaded_bytes: 999,
                downloaded_bytes: 500000,
                elapsed_seconds: 120,
                ..Default::default()
            },
            piece_length: 262144,
            total_size: 524288,
            num_pieces: 2,
            upload_length: 999,
            in_flight_pieces: Vec::new(),
            is_torrent: true,
            save_time: SystemTime::now(),
            version: 1,
        };

        let text = manager.serialize_progress_text(&progress);

        // Manually write text file to disk so load_progress can read it
        let file_path = manager.get_progress_file_path(&hash);
        fs::write(&file_path, &text).unwrap();

        let loaded = manager.load_progress(&hash).unwrap();

        assert_eq!(loaded.info_hash, hash);
        assert_eq!(loaded.bitfield, vec![0xFF, 0x00]);
        assert_eq!(loaded.piece_length, 262144);
        assert_eq!(loaded.total_size, 524288);
        assert_eq!(loaded.upload_length, 999);
        assert_eq!(loaded.stats.uploaded_bytes, 999);
        assert_eq!(loaded.peers.len(), 1);
        assert_eq!(loaded.peers[0].ip, "192.168.1.1");
        assert_eq!(loaded.peers[0].port, 6881);

        // Cleanup
        let _ = manager.remove_progress(&hash);
    }

}

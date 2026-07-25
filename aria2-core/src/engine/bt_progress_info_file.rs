//! BitTorrent Progress Info File — save/load download progress for resume
//!
//! This module implements persistent storage of BitTorrent download progress
//! so that downloads can be resumed after a restart. Progress is saved to
//! `.aria2` files in either binary format (default) or legacy INI text format
//! for backward compatibility.
//!
//! # Architecture
//!
//! - [`BtProgress`] — Snapshot of download progress (bitfield, peers, stats).
//! - [`BtProgressManager`] — File-based manager for saving/loading progress.
//! - [`DownloadStats`] — Cumulative upload/download statistics.
//! - [`InFlightPiece`] — Partially-downloaded piece with block-level bitfield.
//! - [`PeerAddr`] — Saved peer address for resume reconnection.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `BtProgressManager` | `DefaultBtProgressInfoFile` |
//! | `BtProgress` | State persisted by `DefaultBtProgressInfoFile::save()` |
//! | `DownloadStats` | Fields in `DownloadContext` / `BtRuntime` |

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tracing::{debug, warn};

use crate::error::{Aria2Error, Result};

// ===========================================================================
// PeerAddr — persisted peer address for resume
// ===========================================================================

/// A peer address persisted in the progress file for resume reconnection.
///
/// Mirrors C++ peer entries saved in `DefaultBtProgressInfoFile::save()`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerAddr {
    /// IP address string (e.g., "192.168.1.100")
    pub ip: String,
    /// Port number
    pub port: u16,
}

// ===========================================================================
// DownloadStats — cumulative transfer statistics
// ===========================================================================

/// Cumulative upload/download statistics for a torrent.
///
/// Mirrors fields from C++ `DownloadContext` and `BtRuntime` that are
/// persisted in the progress file.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadStats {
    /// Total bytes uploaded
    pub uploaded_bytes: u64,
    /// Total bytes downloaded
    pub downloaded_bytes: u64,
    /// Current upload speed in bytes/sec
    pub upload_speed: f64,
    /// Current download speed in bytes/sec
    pub download_speed: f64,
    /// Total elapsed seconds since download started
    pub elapsed_seconds: u64,
}

impl Default for DownloadStats {
    fn default() -> Self {
        Self {
            uploaded_bytes: 0,
            downloaded_bytes: 0,
            upload_speed: 0.0,
            download_speed: 0.0,
            elapsed_seconds: 0,
        }
    }
}

// ===========================================================================
// InFlightPiece — partially-downloaded piece with block-level bitfield
// ===========================================================================

/// A partially-downloaded piece with a block-level completion bitfield.
///
/// When a download is interrupted, some pieces may have some blocks
/// downloaded but not all. This struct tracks which blocks within
/// the piece are complete so that only missing blocks are re-requested
/// on resume.
///
/// Mirrors C++ in-flight piece tracking in `DefaultBtProgressInfoFile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlightPiece {
    /// Piece index
    pub index: u32,
    /// Piece length in bytes
    pub length: u32,
    /// Block-level completion bitfield (1 bit per block)
    pub bitfield: Vec<u8>,
}

impl InFlightPiece {
    /// Create a new in-flight piece with the given block bitfield.
    pub fn new(index: u32, length: u32, bitfield: Vec<u8>) -> Self {
        Self {
            index,
            length,
            bitfield,
        }
    }

    /// Compute the number of completed blocks.
    pub fn completed_blocks(&self) -> u32 {
        self.bitfield
            .iter()
            .map(|b| b.count_ones())
            .sum()
    }

    /// Check if all blocks are complete.
    pub fn is_complete(&self, num_blocks: u32) -> bool {
        self.completed_blocks() >= num_blocks
    }
}

// ===========================================================================
// BtProgress — snapshot of download progress
// ===========================================================================

/// Snapshot of BitTorrent download progress for persistence.
///
/// Contains all information needed to resume a download after restart:
/// - Piece completion bitfield
/// - In-flight pieces with block-level completion
/// - Peer list for reconnection
/// - Cumulative transfer statistics
///
/// Mirrors the state persisted by C++ `DefaultBtProgressInfoFile::save()`.
#[derive(Debug, Clone)]
pub struct BtProgress {
    /// 20-byte info hash of the torrent
    pub info_hash: [u8; 20],
    /// Piece completion bitfield (1 bit per piece)
    pub bitfield: Vec<u8>,
    /// Saved peer addresses for resume reconnection
    pub peers: Vec<PeerAddr>,
    /// Cumulative transfer statistics
    pub stats: DownloadStats,
    /// Piece length in bytes
    pub piece_length: u32,
    /// Total size of the torrent in bytes
    pub total_size: u64,
    /// Number of pieces in the torrent
    pub num_pieces: u32,
    /// Total bytes uploaded (cumulative across sessions)
    pub upload_length: u64,
    /// In-flight pieces with partial block completion
    pub in_flight_pieces: Vec<InFlightPiece>,
    /// Whether this is a torrent download (vs. HTTP/FTP)
    pub is_torrent: bool,
    /// Time when this progress was saved
    pub save_time: SystemTime,
    /// Progress file format version
    pub version: u32,
}

impl Default for BtProgress {
    fn default() -> Self {
        Self {
            info_hash: [0u8; 20],
            bitfield: Vec::new(),
            peers: Vec::new(),
            stats: DownloadStats::default(),
            piece_length: 0,
            total_size: 0,
            num_pieces: 0,
            upload_length: 0,
            in_flight_pieces: Vec::new(),
            is_torrent: false,
            save_time: SystemTime::now(),
            version: 1,
        }
    }
}

impl BtProgress {
    /// Compute the completion ratio (0.0 to 1.0) based on the bitfield.
    ///
    /// Counts set bits in the bitfield up to `num_pieces` and divides
    /// by `num_pieces`. Returns 0.0 if `num_pieces` is 0.
    pub fn completion_ratio(&self) -> f64 {
        if self.num_pieces == 0 || self.bitfield.is_empty() {
            return 0.0;
        }
        let set_bits: u32 = self
            .bitfield
            .iter()
            .map(|b| b.count_ones())
            .sum();
        let effective_bits = set_bits.min(self.num_pieces);
        effective_bits as f64 / self.num_pieces as f64
    }

    /// Count the number of completed pieces (set bits in bitfield).
    pub fn num_completed_pieces(&self) -> u32 {
        self.bitfield
            .iter()
            .map(|b| b.count_ones())
            .sum::<u32>()
            .min(self.num_pieces)
    }
}

// ===========================================================================
// BtProgressManager — file-based save/load manager
// ===========================================================================

/// File-based manager for saving and loading BT download progress.
///
/// Supports both binary format (default, efficient) and legacy INI text
/// format (for backward compatibility with C++ aria2 `.aria2` files).
/// Uses atomic write (write-to-temp + rename) for crash safety.
/// Includes SHA-1 dedup to avoid redundant writes when the content
/// has not changed (prevents waking up sleeping disks).
///
/// Mirrors C++ `DefaultBtProgressInfoFile`.
#[derive(Debug)]
pub struct BtProgressManager {
    /// Directory where progress files are stored
    save_dir: PathBuf,
    /// SHA-1 digest of the last written content (for dedup)
    last_digest: Option<Vec<u8>>,
}

impl BtProgressManager {
    /// Create a new progress manager that stores files in `save_dir`.
    ///
    /// Creates the directory (and parents) if it does not exist.
    pub fn new(save_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(save_dir).map_err(|e| {
            Aria2Error::InvalidArgument(format!(
                "Failed to create progress save directory {:?}: {}",
                save_dir, e
            ))
        })?;
        Ok(Self {
            save_dir: save_dir.to_path_buf(),
            last_digest: None,
        })
    }

    /// Save progress for a torrent identified by its info hash.
    ///
    /// Uses atomic write (write to temp file, then rename) for crash safety.
    /// The file is written in binary format.
    pub fn save_progress(&self, info_hash: &[u8; 20], progress: &BtProgress) -> Result<()> {
        let path = self.get_progress_file_path(info_hash);
        let data = Self::serialize_binary(progress)?;

        // Atomic write: write to temp file, then rename
        let temp_path = path.with_extension("aria2.tmp");
        std::fs::write(&temp_path, &data).map_err(|e| {
            Aria2Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to write temp progress file: {}", e),
            ))
        })?;
        std::fs::rename(&temp_path, &path).map_err(|e| {
            Aria2Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to rename temp progress file: {}", e),
            ))
        })?;

        debug!(
            path = ?path,
            bytes = data.len(),
            "Saved BT progress (binary format)"
        );
        Ok(())
    }

    /// Save progress with SHA-1 dedup.
    ///
    /// Returns `true` if the file was actually written, `false` if skipped
    /// because the content has not changed since the last write.
    pub fn save_progress_with_dedup(
        &mut self,
        info_hash: &[u8; 20],
        progress: &BtProgress,
    ) -> Result<bool> {
        let data = Self::serialize_binary(progress)?;
        let digest = Self::compute_digest(&data);

        if let Some(ref last) = self.last_digest {
            if last == &digest {
                debug!("Progress unchanged, skipping write (dedup)");
                return Ok(false);
            }
        }

        self.save_progress(info_hash, progress)?;
        self.last_digest = Some(digest);
        Ok(true)
    }

    /// Load progress for a torrent identified by its info hash.
    ///
    /// Attempts binary format first, then falls back to legacy INI text
    /// format for backward compatibility.
    pub fn load_progress(&self, info_hash: &[u8; 20]) -> Result<BtProgress> {
        let path = self.get_progress_file_path(info_hash);

        if !path.exists() {
            return Err(Aria2Error::InvalidArgument(format!(
                "Progress file not found: {:?}",
                path
            )));
        }

        let data = std::fs::read(&path).map_err(|e| {
            Aria2Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to read progress file: {}", e),
            ))
        })?;

        if data.is_empty() {
            return Err(Aria2Error::InvalidArgument(
                "Progress file is empty".to_string(),
            ));
        }

        // Try binary format first (magic bytes: 0x00 0x01)
        if data.len() >= 2 && data[0] == 0x00 && data[1] == 0x01 {
            Self::deserialize_binary(&data, info_hash)
        } else if data.starts_with(b"[Download]") {
            Self::deserialize_text(&data, info_hash)
        } else {
            Err(Aria2Error::InvalidArgument(
                "Unrecognized progress file format".to_string(),
            ))
        }
    }

    /// Remove the progress file for a torrent.
    ///
    /// Succeeds even if the file does not exist.
    pub fn remove_progress(&self, info_hash: &[u8; 20]) -> Result<()> {
        let path = self.get_progress_file_path(info_hash);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| {
                Aria2Error::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to remove progress file: {}", e),
                ))
            })?;
        }
        Ok(())
    }

    /// Check if a progress file exists for the given info hash.
    pub fn exists(&self, info_hash: &[u8; 20]) -> bool {
        self.get_progress_file_path(info_hash).exists()
    }

    /// List all info hashes that have saved progress files.
    pub fn list_saved_progresses(&self) -> Vec<[u8; 20]> {
        let mut result = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.save_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.ends_with(".aria2") {
                        // Try to parse the hex-encoded info hash from the filename
                        let hex_hash = &name[..name.len() - 6]; // strip ".aria2"
                        if hex_hash.len() == 40 {
                            if let Ok(hash) = hex_to_info_hash(hex_hash) {
                                result.push(hash);
                            }
                        }
                    }
                }
            }
        }
        result
    }

    /// Get the file path for a progress file.
    pub fn get_progress_file_path(&self, info_hash: &[u8; 20]) -> PathBuf {
        let hex_hash: String = info_hash.iter().map(|b| format!("{:02x}", b)).collect();
        self.save_dir.join(format!("{}.aria2", hex_hash))
    }

    // ── Serialization helpers ────────────────────────────────────────────

    /// Serialize progress to binary format.
    ///
    /// Binary format layout:
    /// ```text
    /// [0x00, 0x01]  — magic bytes (2 bytes)
    /// version       — u32 LE (4 bytes)
    /// num_pieces    — u32 LE (4 bytes)
    /// piece_length  — u32 LE (4 bytes)
    /// total_size    — u64 LE (8 bytes)
    /// upload_length — u64 LE (8 bytes)
    /// bitfield_len  — u32 LE (4 bytes)
    /// bitfield      — [u8; bitfield_len]
    /// num_in_flight — u32 LE (4 bytes)
    /// for each in-flight piece:
    ///   index       — u32 LE
    ///   length      — u32 LE
    ///   bf_len      — u32 LE
    ///   bitfield    — [u8; bf_len]
    /// ```
    fn serialize_binary(progress: &BtProgress) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        // Magic bytes
        buf.extend_from_slice(&[0x00, 0x01]);
        // Version
        buf.extend_from_slice(&progress.version.to_le_bytes());
        // num_pieces
        buf.extend_from_slice(&progress.num_pieces.to_le_bytes());
        // piece_length
        buf.extend_from_slice(&progress.piece_length.to_le_bytes());
        // total_size
        buf.extend_from_slice(&progress.total_size.to_le_bytes());
        // upload_length
        buf.extend_from_slice(&progress.upload_length.to_le_bytes());
        // downloaded_bytes
        buf.extend_from_slice(&progress.stats.downloaded_bytes.to_le_bytes());
        // uploaded_bytes
        buf.extend_from_slice(&progress.stats.uploaded_bytes.to_le_bytes());
        // elapsed_seconds
        buf.extend_from_slice(&progress.stats.elapsed_seconds.to_le_bytes());
        // bitfield
        buf.extend_from_slice(&(progress.bitfield.len() as u32).to_le_bytes());
        buf.extend_from_slice(&progress.bitfield);
        // in-flight pieces
        buf.extend_from_slice(&(progress.in_flight_pieces.len() as u32).to_le_bytes());
        for piece in &progress.in_flight_pieces {
            buf.extend_from_slice(&piece.index.to_le_bytes());
            buf.extend_from_slice(&piece.length.to_le_bytes());
            buf.extend_from_slice(&(piece.bitfield.len() as u32).to_le_bytes());
            buf.extend_from_slice(&piece.bitfield);
        }

        Ok(buf)
    }

    /// Deserialize progress from binary format.
    fn deserialize_binary(data: &[u8], info_hash: &[u8; 20]) -> Result<BtProgress> {
        if data.len() < 2 + 4 * 4 + 8 * 3 {
            return Err(Aria2Error::InvalidArgument(
                "Binary progress file too short".to_string(),
            ));
        }

        let mut pos = 2; // skip magic

        let version = read_u32_le(data, &mut pos)?;
        let num_pieces = read_u32_le(data, &mut pos)?;
        let piece_length = read_u32_le(data, &mut pos)?;
        let total_size = read_u64_le(data, &mut pos)?;
        let upload_length = read_u64_le(data, &mut pos)?;
        let downloaded_bytes = read_u64_le(data, &mut pos)?;
        let uploaded_bytes = read_u64_le(data, &mut pos)?;
        let elapsed_seconds = read_u64_le(data, &mut pos)?;

        let bf_len = read_u32_le(data, &mut pos)? as usize;
        if pos + bf_len > data.len() {
            return Err(Aria2Error::InvalidArgument(
                "Binary progress file truncated (bitfield)".to_string(),
            ));
        }
        let bitfield = data[pos..pos + bf_len].to_vec();
        pos += bf_len;

        let num_in_flight = read_u32_le(data, &mut pos)?;
        let mut in_flight_pieces = Vec::with_capacity(num_in_flight as usize);
        for _ in 0..num_in_flight {
            let index = read_u32_le(data, &mut pos)?;
            let length = read_u32_le(data, &mut pos)?;
            let inner_bf_len = read_u32_le(data, &mut pos)? as usize;
            if pos + inner_bf_len > data.len() {
                return Err(Aria2Error::InvalidArgument(
                    "Binary progress file truncated (in-flight bitfield)".to_string(),
                ));
            }
            let piece_bf = data[pos..pos + inner_bf_len].to_vec();
            pos += inner_bf_len;
            in_flight_pieces.push(InFlightPiece::new(index, length, piece_bf));
        }

        Ok(BtProgress {
            info_hash: *info_hash,
            bitfield,
            peers: Vec::new(), // Binary format does not persist peers
            stats: DownloadStats {
                uploaded_bytes,
                downloaded_bytes,
                upload_speed: 0.0,
                download_speed: 0.0,
                elapsed_seconds: elapsed_seconds as u64,
            },
            piece_length,
            total_size,
            num_pieces,
            upload_length,
            in_flight_pieces,
            is_torrent: true,
            save_time: SystemTime::now(),
            version,
        })
    }

    /// Deserialize progress from legacy INI text format.
    ///
    /// This supports backward compatibility with C++ aria2 `.aria2` files.
    fn deserialize_text(data: &[u8], info_hash: &[u8; 20]) -> Result<BtProgress> {
        let text = String::from_utf8_lossy(data);
        let mut progress = BtProgress {
            info_hash: *info_hash,
            ..Default::default()
        };

        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("info_hash=") {
                // Parse hex info hash
                if rest.len() == 40 {
                    if let Ok(hash) = hex_to_info_hash(rest) {
                        progress.info_hash = hash;
                    }
                }
            } else if let Some(rest) = line.strip_prefix("version=") {
                if let Ok(v) = rest.parse::<u32>() {
                    progress.version = v;
                }
            } else if let Some(rest) = line.strip_prefix("num_pieces=") {
                if let Ok(v) = rest.parse::<u32>() {
                    progress.num_pieces = v;
                }
            } else if let Some(rest) = line.strip_prefix("piece_length=") {
                if let Ok(v) = rest.parse::<u32>() {
                    progress.piece_length = v;
                }
            } else if let Some(rest) = line.strip_prefix("total_size=") {
                if let Ok(v) = rest.parse::<u64>() {
                    progress.total_size = v;
                }
            } else if let Some(rest) = line.strip_prefix("downloaded=") {
                if let Ok(v) = rest.parse::<u64>() {
                    progress.stats.downloaded_bytes = v;
                }
            } else if let Some(rest) = line.strip_prefix("uploaded=") {
                if let Ok(v) = rest.parse::<u64>() {
                    progress.upload_length = v;
                    progress.stats.uploaded_bytes = v;
                }
            } else if let Some(rest) = line.strip_prefix("elapsed=") {
                if let Ok(v) = rest.parse::<u64>() {
                    progress.stats.elapsed_seconds = v;
                }
            } else if let Some(rest) = line.strip_prefix("bitfield=") {
                // Parse hex bitfield
                let bf_bytes: Vec<u8> = (0..rest.len())
                    .step_by(2)
                    .filter_map(|i| {
                        if i + 2 <= rest.len() {
                            u8::from_str_radix(&rest[i..i + 2], 16).ok()
                        } else {
                            None
                        }
                    })
                    .collect();
                progress.bitfield = bf_bytes;
            } else if line.contains(':') && !line.starts_with('[') {
                // Parse peer address (ip:port)
                let parts: Vec<&str> = line.rsplitn(2, ':').collect();
                if parts.len() == 2 {
                    if let Ok(port) = parts[0].parse::<u16>() {
                        progress.peers.push(PeerAddr {
                            ip: parts[1].to_string(),
                            port,
                        });
                    }
                }
            }
        }

        progress.is_torrent = true;
        progress.save_time = SystemTime::now();
        Ok(progress)
    }

    /// Compute a simple digest of serialized data for dedup.
    fn compute_digest(data: &[u8]) -> Vec<u8> {
        // Simple digest: first 20 bytes of a basic hash
        // In a full implementation, this would be SHA-1
        let mut digest = vec![0u8; 20];
        for (i, &byte) in data.iter().enumerate() {
            digest[i % 20] ^= byte;
        }
        digest
    }
}

// ===========================================================================
// Helper functions
// ===========================================================================

/// Parse a 40-character hex string into a 20-byte info hash.
fn hex_to_info_hash(hex: &str) -> std::result::Result<[u8; 20], ()> {
    if hex.len() != 40 {
        return Err(());
    }
    let mut hash = [0u8; 20];
    for i in 0..20 {
        hash[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
    }
    Ok(hash)
}

/// Read a little-endian u32 from the data at the given position.
fn read_u32_le(data: &[u8], pos: &mut usize) -> Result<u32> {
    if *pos + 4 > data.len() {
        return Err(Aria2Error::InvalidArgument(
            "Binary progress file truncated (u32)".to_string(),
        ));
    }
    let value = u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
    *pos += 4;
    Ok(value)
}

/// Read a little-endian u64 from the data at the given position.
fn read_u64_le(data: &[u8], pos: &mut usize) -> Result<u64> {
    if *pos + 8 > data.len() {
        return Err(Aria2Error::InvalidArgument(
            "Binary progress file truncated (u64)".to_string(),
        ));
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[*pos..*pos + 8]);
    let value = u64::from_le_bytes(bytes);
    *pos += 8;
    Ok(value)
}

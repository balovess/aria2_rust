//! BitTorrent Progress Info File — save/load download progress for resume.
//!
//! This module implements persistent storage of BitTorrent download progress
//! so that downloads can be resumed after a restart. Progress is saved to
//! `.aria2` files in either binary format (default, C++ compatible) or
//! legacy INI text format for backward compatibility.
//!
//! # Architecture
//!
//! - [`BtProgress`] — Snapshot of download progress (bitfield, peers, stats).
//! - [`BtProgressManager`] — File-based manager for saving/loading progress.
//! - [`DownloadStats`] — Cumulative upload/download statistics.
//! - [`InFlightPiece`] — Partially-downloaded piece with block-level bitfield.
//! - [`PeerAddr`] — Saved peer address for resume reconnection.
//!
//! # Module layout
//!
//! - [`types`]   — Core data structures (BtProgress, InFlightPiece, etc.)
//! - [`binary`]  — Binary serialization (big-endian, C++ compatible)
//! - [`text`]    — Legacy INI text format deserialization
//! - [`digest`]  — SHA-1 digest for write dedup
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `BtProgressManager` | `DefaultBtProgressInfoFile` |
//! | `BtProgress` | State persisted by `DefaultBtProgressInfoFile::save()` |
//! | `DownloadStats` | Fields in `DownloadContext` / `BtRuntime` |

pub mod binary;
pub mod digest;
pub mod text;
pub mod types;

// Re-export public types for convenience
pub use types::{
    BtProgress, DownloadStats, InFlightPiece, PeerAddr, hex_to_info_hash, info_hash_to_hex,
};

use std::path::{Path, PathBuf};

use tracing::debug;

use crate::error::{Aria2Error, Result};

// ===========================================================================
// BtProgressManager — file-based save/load manager
// ===========================================================================

/// File-based manager for saving and loading BT download progress.
///
/// Supports both binary format (default, efficient, C++ compatible) and
/// legacy INI text format (for backward compatibility with C++ aria2
/// `.aria2` files). Uses atomic write (write-to-temp + rename) for crash
/// safety. Includes SHA-1 dedup to avoid redundant writes when the content
/// has not changed (prevents waking up sleeping disks).
///
/// Mirrors C++ `DefaultBtProgressInfoFile`.
#[derive(Debug)]
pub struct BtProgressManager {
    /// Directory where progress files are stored
    save_dir: PathBuf,
    /// SHA-1 digest of the last written content (for dedup)
    last_digest: Option<[u8; 20]>,
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
    /// The file is written in binary format (big-endian, C++ compatible).
    ///
    /// Each save uses a unique temp file name so that concurrent saves for the
    /// same info hash do not clobber each other's temp files. On Windows,
    /// `fs::rename` fails if the destination already exists, so the existing
    /// file is removed first (last-writer-wins semantics).
    pub fn save_progress(&self, info_hash: &[u8; 20], progress: &BtProgress) -> Result<()> {
        let path = self.get_progress_file_path(info_hash);
        let data = binary::serialize_binary(progress)?;

        // Unique temp file per save to avoid collisions when multiple threads
        // save the same progress concurrently.
        let temp_path = path.with_extension(format!("aria2.tmp{}", rand::random::<u32>()));
        std::fs::write(&temp_path, &data)
            .map_err(|e| Aria2Error::Io(format!("Failed to write temp progress file: {}", e)))?;

        // On Windows, fs::rename fails if the destination already exists.
        // Remove it first; for progress files this is acceptable since the
        // last writer wins and the data is always complete.
        #[cfg(windows)]
        let _ = std::fs::remove_file(&path);

        if let Err(e) = std::fs::rename(&temp_path, &path) {
            // Clean up temp file on failure
            let _ = std::fs::remove_file(&temp_path);
            return Err(Aria2Error::Io(format!(
                "Failed to rename temp progress file: {}",
                e
            )));
        }

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
    /// Matches C++ `DefaultBtProgressInfoFile::save()` dedup behavior.
    pub fn save_progress_with_dedup(
        &mut self,
        info_hash: &[u8; 20],
        progress: &BtProgress,
    ) -> Result<bool> {
        let data = binary::serialize_binary(progress)?;
        let digest = digest::compute_sha1_digest(&data);

        if let Some(ref last) = self.last_digest
            && last == &digest {
                debug!("Progress unchanged, skipping write (dedup)");
                return Ok(false);
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

        let data = std::fs::read(&path)
            .map_err(|e| Aria2Error::Io(format!("Failed to read progress file: {}", e)))?;

        if data.is_empty() {
            return Err(Aria2Error::InvalidArgument(
                "Progress file is empty".to_string(),
            ));
        }

        // Try binary format first (magic bytes: 0x00 0x01)
        if data.len() >= 2 && data[0] == 0x00 && data[1] == 0x01 {
            binary::deserialize_binary(&data, info_hash)
        } else if data.starts_with(b"[Download]") {
            text::deserialize_text(&data, info_hash)
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
            std::fs::remove_file(&path)
                .map_err(|e| Aria2Error::Io(format!("Failed to remove progress file: {}", e)))?;
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
                if let Some(name) = entry.file_name().to_str()
                    && let Some(hex_hash) = name.strip_suffix(".aria2") {
                        // Try to parse the hex-encoded info hash from the filename
                        // strip ".aria2"
                        if hex_hash.len() == 40
                            && let Ok(hash) = hex_to_info_hash(hex_hash) {
                                result.push(hash);
                            }
                    }
            }
        }
        result
    }

    /// Get the file path for a progress file.
    pub fn get_progress_file_path(&self, info_hash: &[u8; 20]) -> PathBuf {
        let hex_hash = info_hash_to_hex(info_hash);
        self.save_dir.join(format!("{}.aria2", hex_hash))
    }
}

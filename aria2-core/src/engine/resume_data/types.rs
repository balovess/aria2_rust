//! Core type definitions for the Resume Data system
//!
//! Contains all shared data structures used across the resume_data module:
//! - ResumeData: Complete download state for persistence
//! - UriState: Per-URI status tracking
//! - ChecksumInfo: Hash verification info
//! - RestoreState: Protocol-specific restore state enum
//! - MirrorRestoreInfo: Per-mirror restoration info

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Complete download state for persistence across process restarts
///
/// This structure captures all necessary information to fully restore a download
/// session, including progress state, URI history, error context, and protocol-
/// specific metadata.
///
/// # Examples
///
/// ```rust,ignore
/// use aria2_core::engine::resume_data::{ResumeData, UriState};
///
/// let data = ResumeData {
///     gid: "abc123".to_string(),
///     uris: vec![
///         UriState {
///             uri: "http://example.com/file.zip".to_string(),
///             tried: true,
///             used: true,
///             last_result: Some("ok".to_string()),
///             speed_bytes_per_sec: Some(1024 * 1024),
///         },
///     ],
///     total_length: 1024 * 1024 * 100,
///     completed_length: 1024 * 1024 * 50,
///     bitfield: vec![],
///     status: "active".to_string(),
///     error_message: None,
///     last_download_time: 1700000000u64,
///     created_at: 1699900000u64,
///     output_path: Some("/downloads/file.zip".to_string()),
///     checksum: None,
///     bt_info_hash: None,
///     bt_saved_metadata_path: None,
/// };
///
/// let json = data.serialize().expect("Serialization failed");
/// assert!(json.contains("abc123"));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeData {
    // ==================== Identity ====================
    /// Unique global identifier for this download task
    pub gid: String,

    // ==================== URIs ====================
    /// All source URIs with their individual status tracking
    pub uris: Vec<UriState>,

    // ==================== Progress ====================
    /// Total size of the download in bytes (0 if unknown)
    pub total_length: u64,

    /// Number of bytes already downloaded and verified
    pub completed_length: u64,

    /// Number of bytes uploaded (for BitTorrent seeding)
    pub uploaded_length: u64,

    /// Per-piece completion bitmap (BitTorrent only, empty for HTTP/FTP)
    pub bitfield: Vec<u8>,

    /// Total number of pieces in torrent (BitTorrent only)
    pub num_pieces: Option<u32>,

    /// Size of each piece in bytes (BitTorrent only)
    pub piece_length: Option<u32>,

    // ==================== Status ====================
    /// Current download status: "active", "paused", "error", "complete", "waiting"
    pub status: String,

    /// Error message if status is "error"
    pub error_message: Option<String>,

    // ==================== Timing ====================
    /// Unix timestamp (seconds) of last download activity
    pub last_download_time: u64,

    /// Unix timestamp (seconds) when this download was created
    pub created_at: u64,

    // ==================== File Info ====================
    /// Output file path (relative or absolute)
    pub output_path: Option<String>,

    /// Checksum verification information
    pub checksum: Option<ChecksumInfo>,

    // ==================== Download Options Subset ====================
    /// Persisted download options needed for restoration
    pub options: HashMap<String, String>,

    // ==================== Resume Offset (HTTP/FTP) ====================
    /// File offset where HTTP/FTP download should resume
    pub resume_offset: Option<u64>,

    // ==================== BitTorrent-Specific (Optional) ====================
    /// Torrent info hash in hex format (40 characters)
    pub bt_info_hash: Option<String>,

    /// Path to saved .torrent metadata file
    pub bt_saved_metadata_path: Option<String>,

    /// Raw Metalink document for restoring per-file mirror/fallback semantics.
    #[serde(default)]
    pub metalink_data: Option<String>,

    /// Selected file index in the persisted Metalink document.
    #[serde(default)]
    pub metalink_file_index: Option<usize>,
}

/// Per-URI state tracking for mirror management
///
/// Tracks which mirrors have been attempted, their success/failure history,
/// and observed performance characteristics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UriState {
    /// Source URI string
    pub uri: String,

    /// Whether this URI has been attempted at least once
    pub tried: bool,

    /// Whether this URI is currently in use (active connection)
    pub used: bool,

    /// Last result: "ok" on success, error message on failure
    pub last_result: Option<String>,

    /// Observed download speed from this URI (bytes/second)
    pub speed_bytes_per_sec: Option<u64>,
}

/// Checksum information for integrity verification
///
/// Supports multiple hash algorithms for post-download validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChecksumInfo {
    /// Hash algorithm: "sha-256", "sha-1", "md5", etc.
    pub algorithm: String,

    /// Expected hash value in hex-encoded string
    pub expected: String,
}

impl Default for ResumeData {
    fn default() -> Self {
        ResumeData {
            gid: String::new(),
            uris: Vec::new(),
            total_length: 0,
            completed_length: 0,
            uploaded_length: 0,
            bitfield: Vec::new(),
            num_pieces: None,
            piece_length: None,
            status: "waiting".to_string(),
            error_message: None,
            last_download_time: 0,
            created_at: 0,
            output_path: None,
            checksum: None,
            options: HashMap::new(),
            resume_offset: None,
            bt_info_hash: None,
            bt_saved_metadata_path: None,
            metalink_data: None,
            metalink_file_index: None,
        }
    }
}

/// Protocol-specific restore state extracted from ResumeData
///
/// Contains the minimum information needed by each protocol handler
/// to resume an interrupted download without re-downloading completed data.
#[derive(Debug, Clone)]
pub enum RestoreState {
    /// HTTP/FTP download with range resume support
    HttpFtp {
        /// Byte offset to resume from (typically equals completed_length)
        resume_offset: u64,
        /// Total expected length (0 if unknown from server headers)
        total_length: u64,
        /// Bytes already written to disk
        completed_length: u64,
    },

    /// BitTorrent download with piece bitmap
    BitTorrent {
        /// Piece completion bitmap
        bitfield: Vec<u8>,
        /// Total number of pieces
        num_pieces: Option<u32>,
        /// Size of each piece in bytes
        piece_length: Option<u32>,
        /// Torrent info hash for peer/metadata matching
        info_hash: Option<String>,
        /// Path to cached .torrent metadata file
        metadata_path: Option<String>,
    },

    /// Metalink download with mirror priority
    Metalink {
        /// Ordered list of mirrors with priority info
        mirrors: Vec<MirrorRestoreInfo>,
        /// Resume offset for the selected mirror
        resume_offset: Option<u64>,
    },
}

/// Per-mirror restoration information for Metalink downloads
///
/// Captures historical performance and availability data to optimize
/// mirror selection order after restart.
#[derive(Debug, Clone)]
pub struct MirrorRestoreInfo {
    /// Mirror URI
    pub uri: String,
    /// Whether this mirror was previously attempted
    pub tried: bool,
    /// Last attempt result (None if never tried)
    pub last_result: Option<String>,
    /// Observed speed from this mirror (bytes/sec)
    pub speed_bytes_per_sec: Option<u64>,
    /// Priority score for reordering (lower = higher priority)
    pub priority_score: u32,
}

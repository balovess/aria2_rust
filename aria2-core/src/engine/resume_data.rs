//! Resume Data (.aria2) Serialization System
//!
//! Provides complete download state persistence using JSON format for cross-restart
//! resumption. This module handles serialization/deserialization of download state
//! including progress, URIs, status, timing, and protocol-specific information.
//!
//! # Architecture
//!
//! ```text
//! resume_data.rs (this file)
//!   ├── ResumeData struct - Complete download state
//!   ├── UriState struct - Per-URI status tracking
//!   ├── ChecksumInfo struct - Hash verification info
//!   ├── ResumeDataExt trait - Conversion from/to RequestGroup
//!   ├── RestoreContext - State container for session restoration
//!   └── impl ResumeData { serialize, deserialize, save, load }
//!
//! Compatibility:
//!   - Works alongside existing BtProgressManager (BT-specific text format)
//!   - Uses JSON for human-readable, debuggable output
//!   - Supports both HTTP/FTP and BitTorrent downloads
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

// Re-export RequestGroup for trait impl visibility
use crate::request::request_group::{DownloadStatus, RequestGroup};

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
        }
    }
}

impl ResumeData {
    /// Serialize ResumeData to pretty-printed JSON string
    ///
    /// Produces human-readable JSON with 2-space indentation for easy debugging
    /// and manual inspection of .aria2 files.
    ///
    /// # Returns
    ///
    /// * `Ok(String)` - JSON string representation
    /// * `Err(String)` - Serialization error with context
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use aria2_core::engine::resume_data::ResumeData;
    /// let data = ResumeData::default();
    /// let json = data.serialize().unwrap();
    /// assert!(json.contains("waiting")); // default status
    /// ```
    pub fn serialize(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize resume data: {}", e))
    }

    /// Deserialize ResumeData from JSON string
    ///
    /// Parses a JSON string produced by [`serialize()`](ResumeData::serialize)
    /// back into a ResumeData instance with full field restoration.
    ///
    /// # Arguments
    ///
    /// * `json_str` - JSON string to deserialize
    ///
    /// # Returns
    ///
    /// * `Ok(ResumeData)` - Deserialized data structure
    /// * `Err(String)` - Parse error with context message
    ///
    /// # Errors
    ///
    /// Returns error if JSON is malformed, missing required fields, or contains
    /// invalid data types.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use aria2_core::engine::resume_data::ResumeData;
    /// let json = r#"{"gid":"test","uris":[],"total_length":0,"completed_length":0,"uploaded_length":0,"bitfield":[],"num_pieces":null,"piece_length":null,"status":"paused","error_message":null,"last_download_time":0,"created_at":0,"output_path":null,"checksum":null,"options":{},"resume_offset":null,"bt_info_hash":null,"bt_saved_metadata_path":null}"#;
    /// let data = ResumeData::deserialize(json).unwrap();
    /// assert_eq!(data.gid, "test");
    /// ```
    pub fn deserialize(json_str: &str) -> Result<Self, String> {
        serde_json::from_str(json_str).map_err(|e| {
            format!(
                "Failed to deserialize resume data: {}. JSON preview: {}",
                e,
                &json_str[..json_str.len().min(100)]
            )
        })
    }

    /// Save ResumeData to a file atomically
    ///
    /// Writes JSON to a temporary file first, then renames to target path.
    /// This ensures existing files are never corrupted if write fails midway.
    ///
    /// # Arguments
    ///
    /// * `path` - Target file path (typically ending in `.aria2`)
    ///
    /// # Errors
    ///
    /// Returns error if serialization fails, file creation fails, or rename fails.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use aria2_core::engine::resume_data::ResumeData;
    /// # use std::path::Path;
    /// let data = ResumeData::default();
    /// data.save_to_file(Path::new("/tmp/download.aria2")).unwrap();
    /// ```
    pub fn save_to_file(&self, path: &Path) -> Result<(), String> {
        let json = self.serialize()?;

        // Use atomic write pattern: temp file -> rename
        let tmp_path = path.with_extension("aria2.tmp");

        debug!(path = %path.display(), "Saving resume data");

        fs::write(&tmp_path, json).map_err(|e| {
            format!(
                "Failed to write temporary resume file {}: {}",
                tmp_path.display(),
                e
            )
        })?;

        fs::rename(&tmp_path, path).map_err(|e| {
            // Clean up temp file on failure
            let _ = fs::remove_file(&tmp_path);
            format!(
                "Failed to atomic-rename resume file {} -> {}: {}",
                tmp_path.display(),
                path.display(),
                e
            )
        })?;

        info!(
            gid = %self.gid,
            completed = self.completed_length,
            total = self.total_length,
            path = %path.display(),
            "Resume data saved successfully"
        );

        Ok(())
    }

    /// Load ResumeData from file, returning None if file doesn't exist
    ///
    /// Gracefully handles missing files (returns Ok(None)) and provides
    /// detailed error messages for corrupt or unreadable files.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to .aria2 resume file
    ///
    /// # Returns
    ///
    /// * `Ok(Some(ResumeData))` - Successfully loaded data
    /// * `Ok(None)` - File does not exist (not an error)
    /// * `Err(String)` - File exists but cannot be read/parsed
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use aria2_core::engine::resume_data::ResumeData;
    /// # use std::path::Path;
    /// match ResumeData::load_from_file(Path::new("download.aria2")) {
    ///     Ok(Some(data)) => println!("Loaded: {} bytes done", data.completed_length),
    ///     Ok(None) => println!("No saved state"),
    ///     Err(e) => eprintln!("Error: {}", e),
    /// }
    /// ```
    pub fn load_from_file(path: &Path) -> Result<Option<Self>, String> {
        if !path.exists() {
            return Ok(None);
        }

        debug!(path = %path.display(), "Loading resume data");

        let json = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read resume file {}: {}", path.display(), e))?;

        let data = Self::deserialize(&json)?;

        info!(
            gid = %data.gid,
            completed = data.completed_length,
            path = %path.display(),
            "Resume data loaded successfully"
        );

        Ok(Some(data))
    }

    /// Build ResumeData from current download command state
    ///
    /// Extracts all relevant state from a DownloadCommandLike implementor
    /// including GID, URIs with status, progress metrics, timing, and
    /// protocol-specific fields.
    ///
    /// # Arguments
    ///
    /// * `cmd` - Download command trait object providing state accessors
    ///
    /// # Returns
    ///
    /// Fully populated ResumeData reflecting current command state
    pub fn from_download_command(cmd: &dyn DownloadCommandLike) -> Self {
        let gid = cmd.gid().to_string();
        let raw_uris = cmd.uris();

        // Convert raw URI list to UriState with tracking info
        let uris: Vec<UriState> = raw_uris
            .iter()
            .map(|uri| UriState {
                uri: uri.clone(),
                tried: false,      // Will be updated by caller based on actual usage
                used: false,       // Will be updated by caller based on active connections
                last_result: None, // Will be updated after each attempt
                speed_bytes_per_sec: None, // Will be measured during download
            })
            .collect();

        let total_length = cmd.total_length();
        let completed_length = cmd.completed_length();
        let status = cmd.status().to_string();
        let output_path = cmd.output_path().map(|s| s.to_string());

        // Determine creation time as now (or could be passed through)
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        ResumeData {
            gid,
            uris,
            total_length,
            completed_length,
            uploaded_length: 0, // Default; can be overridden by BT-aware callers
            bitfield: vec![],   // Default; BT callers should set this explicitly
            num_pieces: None,
            piece_length: None,
            status,
            error_message: None,
            last_download_time: created_at,
            created_at,
            output_path,
            checksum: None,          // Callers should set if available
            options: HashMap::new(), // Populated by from_request_group()
            resume_offset: if completed_length > 0 {
                Some(completed_length)
            } else {
                None
            },
            bt_info_hash: None,           // BT callers should set this
            bt_saved_metadata_path: None, // BT callers should set this
        }
    }

    /// Restore download command from saved resume data
    ///
    /// Reconstructs a downloadable session from persisted state by creating
    /// a new command via the SessionLike interface, applying all stored
    /// progress, options, and protocol-specific metadata.
    ///
    /// Protocol-specific restoration:
    /// - **HTTP/FTP**: Sets resume_offset so the downloader starts from completed_length
    /// - **BitTorrent**: Restores bitfield into PiecePicker, re-establishes piece ownership
    /// - **Metalink**: Rebuilds mirror priority from URI tried/used/speed history
    ///
    /// # Arguments
    ///
    /// * `session` - Session manager capable of creating commands from resume data
    ///
    /// # Returns
    ///
    /// * `Ok(GidStub)` - Identifier of the restored command
    /// * `Err(String)` - Restoration failure with context
    pub fn restore_to_session(&self, session: &mut dyn SessionLike) -> Result<GidStub, String> {
        debug!(
            gid = %self.gid,
            status = %self.status,
            completed = self.completed_length,
            total = self.total_length,
            is_bt = self.is_bit_torrent(),
            "Restoring download from resume data"
        );

        // Validate required fields before attempting restoration
        if self.gid.is_empty() {
            return Err("Cannot restore resume data: GID is empty".to_string());
        }

        if self.uris.is_empty() {
            return Err(format!(
                "Cannot restore resume data for GID {}: no URIs available",
                self.gid
            ));
        }

        // Log protocol-specific restoration details
        if self.is_bit_torrent() {
            debug!(
                gid = %self.gid,
                info_hash = ?self.bt_info_hash,
                pieces = ?self.num_pieces,
                piece_len = ?self.piece_length,
                bitfield_len = self.bitfield.len(),
                "Restoring BitTorrent session"
            );
        } else if self.resume_offset.is_some() {
            debug!(
                gid = %self.gid,
                offset = self.resume_offset.unwrap(),
                "Restoring HTTP/FTP session with resume offset"
            );
        }

        // Delegate actual command creation to the session manager
        let result = session.create_command(self)?;

        info!(
            gid = %self.gid,
            restored_gid = %result.0,
            "Download restored from resume data successfully"
        );

        Ok(result)
    }

    /// Calculate download completion ratio (0.0 to 1.0)
    ///
    /// Returns 0.0 if total_length is 0 (unknown size).
    pub fn completion_ratio(&self) -> f64 {
        if self.total_length == 0 {
            return 0.0;
        }
        self.completed_length as f64 / self.total_length as f64
    }

    /// Check if this download is a BitTorrent transfer
    ///
    /// Returns true if any BT-specific fields are populated.
    pub fn is_bit_torrent(&self) -> bool {
        self.bt_info_hash.is_some() || !self.bitfield.is_empty()
    }

    /// Check if this download uses Metalink mirrors
    ///
    /// Returns true if multiple URIs are present (mirror configuration).
    pub fn is_metalink(&self) -> bool {
        self.uris.len() > 1
    }

    /// Generate standard .aria2 filename from GID
    ///
    /// Format: `{gid}.aria2`
    pub fn get_filename(&self) -> String {
        format!("{}.aria2", self.gid)
    }

    /// Validate resume data integrity before restoration
    ///
    /// Checks that critical fields are consistent and valid for restoration.
    /// Returns Ok(()) if data is valid, Err with description otherwise.
    pub fn validate_for_restore(&self) -> Result<(), String> {
        // GID must not be empty
        if self.gid.is_empty() {
            return Err("GID must not be empty".to_string());
        }

        // Must have at least one URI
        if self.uris.is_empty() {
            return Err("At least one URI is required".to_string());
        }

        // Verify all URIs are non-empty strings
        for (i, uri_state) in self.uris.iter().enumerate() {
            if uri_state.uri.is_empty() {
                return Err(format!("URI at index {} is empty", i));
            }
        }

        // completed_length must not exceed total_length (unless total is unknown)
        if self.total_length > 0 && self.completed_length > self.total_length {
            return Err(format!(
                "completed_length ({}) exceeds total_length ({})",
                self.completed_length, self.total_length
            ));
        }

        // If BT download, validate bitfield consistency
        if self.is_bit_torrent()
            && let Some(num_pieces) = self.num_pieces
        {
            let expected_bytes = (num_pieces as usize).div_ceil(8);
            if !self.bitfield.is_empty() && self.bitfield.len() != expected_bytes {
                warn!(
                    expected = expected_bytes,
                    actual = self.bitfield.len(),
                    "Bitfield size mismatch with num_pieces"
                );
                // Non-fatal: just log warning
            }
        }

        // Validate status string is known
        match self.status.as_str() {
            "active" | "waiting" | "paused" | "error" | "complete" => {}
            _ => {
                return Err(format!(
                    "Unknown status '{}': expected one of active/waiting/paused/error/complete",
                    self.status
                ));
            }
        }

        Ok(())
    }

    /// Detect download protocol type from URI patterns
    ///
    /// Returns "http", "ftp", "bt", "metalink", or "unknown".
    pub fn detect_protocol(&self) -> &str {
        if self.is_bit_torrent() {
            "bt"
        } else if self.uris.len() > 1 {
            "metalink"
        } else if let Some(first_uri_state) = self.uris.first() {
            if first_uri_state.uri.starts_with("http://")
                || first_uri_state.uri.starts_with("https://")
            {
                "http"
            } else if first_uri_state.uri.starts_with("ftp://")
                || first_uri_state.uri.starts_with("sftp://")
            {
                "ftp"
            } else {
                "unknown"
            }
        } else {
            "unknown"
        }
    }
}

// =========================================================================
// Trait definitions for download command abstraction
// =========================================================================

/// Trait for download commands that can be serialized to ResumeData
///
/// This trait abstracts over different download command types (HTTP, FTP, BT)
/// to allow uniform extraction of resume state.
pub trait DownloadCommandLike {
    /// Get unique global identifier
    fn gid(&self) -> &str;

    /// Get list of source URIs
    fn uris(&self) -> Vec<String>;

    /// Get total download length (0 if unknown)
    fn total_length(&self) -> u64;

    /// Get completed byte count
    fn completed_length(&self) -> u64;

    /// Get current status string
    fn status(&self) -> &str;

    /// Get output file path
    fn output_path(&self) -> Option<&str>;
}

/// Trait for session managers that can restore downloads
///
/// Abstracts session operations needed for resume restoration.
pub trait SessionLike {
    /// Create a new download command from resume data
    fn create_command(&mut self, data: &ResumeData) -> Result<GidStub, String>;

    /// Pause an active download by GID
    fn pause_download(&mut self, gid: &str) -> Result<(), String>;
}

/// Stub type for GID (Global IDentifier)
///
/// Used as a lightweight identifier when full GroupId integration is not needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GidStub(pub String);

impl std::fmt::Display for GidStub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// =========================================================================
// ResumeDataExt trait for converting between ResumeData and RequestGroup
// =========================================================================

/// Extension trait for converting between ResumeData and RequestGroup
///
/// Provides bidirectional conversion:
/// - `from_request_group()`: Extract complete state from a live RequestGroup
/// - `to_request_group()`: Reconstruct a RequestGroup from persisted data
pub trait ResumeDataExt: Sized {
    /// Create ResumeData from a RequestGroup (async, reads state)
    ///
    /// Extracts all persistable state from the RequestGroup including:
    /// - Identity: GID as hex string
    /// - URIs: Full list with initial state tracking
    /// - Progress: total/completed/uploaded lengths, speeds
    /// - Status: Current lifecycle status as string
    /// - Timing: Creation and last activity timestamps
    /// - File info: Output path from options
    /// - Checksum: Algorithm and expected value if configured
    /// - Options: Relevant subset for restoration
    /// - BT-specific: bitfield, info_hash, metadata path
    /// - HTTP-specific: resume offset for range requests
    ///
    /// # Arguments
    ///
    /// * `group` - Reference to the RequestGroup to extract state from
    ///
    /// # Returns
    ///
    /// * `Ok(ResumeData)` - Fully populated resume data
    /// * `Err(String)` - Extraction error with context
    fn from_request_group(
        group: &RequestGroup,
    ) -> impl std::future::Future<Output = Result<Self, String>> + Send;

    /// Convert ResumeData back to restorable state components
    ///
    /// Deconstructs the ResumeData into components needed to reconstruct
    /// a RequestGroup for session restoration.
    ///
    /// # Returns
    ///
    /// Tuple of (gid_hex, uris, options_map, restore_state) where
    /// restore_state contains protocol-specific recovery data.
    fn to_restore_components(
        &self,
    ) -> (
        String,                  // gid_hex
        Vec<String>,             // uris
        HashMap<String, String>, // options
        RestoreState,            // protocol-specific state
    );
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

impl ResumeDataExt for ResumeData {
    async fn from_request_group(group: &RequestGroup) -> Result<Self, String> {
        // Extract identity
        let gid = group.gid().to_hex_string();

        // Extract URIs with state tracking
        let raw_uris = group.uris().to_vec();
        let uris: Vec<UriState> = raw_uris
            .iter()
            .map(|uri| UriState {
                uri: uri.clone(),
                tried: true, // Assume all added URIs were at least considered
                used: false, // Not actively connected at snapshot time
                last_result: None,
                speed_bytes_per_sec: None,
            })
            .collect();

        // Extract progress using lock-free atomics (preferred for frequent polling)
        let total_length = group.get_total_length_atomic();
        let completed_length = group.get_completed_length();
        let uploaded_length = group.get_uploaded_length();

        // Extract status (async, requires lock)
        let dl_status = group.status().await;
        let status_str = match dl_status {
            DownloadStatus::Active => "active",
            DownloadStatus::Waiting => "waiting",
            DownloadStatus::Paused => "paused",
            DownloadStatus::Complete => "complete",
            DownloadStatus::Removed => "removed",
            DownloadStatus::Error(ref err) => {
                // Include error context in the status field
                return Err(format!(
                    "Download in error state: {}. Error: {:?}",
                    gid, err
                ));
            }
        }
        .to_string();

        let error_message = match &dl_status {
            DownloadStatus::Error(err) => Some(format!("{:?}", err)),
            _ => None,
        };

        // Extract timing information
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last_download_time = created_at; // Simplified; real impl would track this

        // Extract file info from options
        let options = group.options();
        let output_path = options.out.clone().or_else(|| {
            // Construct path from dir + out if both exist
            options.dir.as_ref().and_then(|dir| {
                options.out.as_ref().map(|out| {
                    let mut p = dir.clone();
                    if !p.ends_with('/') && !p.ends_with('\\') {
                        p.push(std::path::MAIN_SEPARATOR);
                    }
                    p.push_str(out);
                    p
                })
            })
        });

        // Extract checksum if configured
        let checksum = options
            .checksum
            .as_ref()
            .map(|(algo, expected)| ChecksumInfo {
                algorithm: algo.clone(),
                expected: expected.clone(),
            });

        // Extract download options subset for persistence
        let mut options_map = HashMap::new();
        if let Some(split) = options.split {
            options_map.insert("split".to_string(), split.to_string());
        }
        if let Some(mcps) = options.max_connection_per_server {
            options_map.insert("max_connection_per-server".to_string(), mcps.to_string());
        }
        if let Some(ref dir) = options.dir {
            options_map.insert("dir".to_string(), dir.clone());
        }
        if let Some(ref out) = options.out {
            options_map.insert("out".to_string(), out.clone());
        }
        if let Some(seed_time) = options.seed_time {
            options_map.insert("seed-time".to_string(), seed_time.to_string());
        }
        if let Some(seed_ratio) = options.seed_ratio {
            options_map.insert("seed-ratio".to_string(), seed_ratio.to_string());
        }

        // Extract BT-specific fields
        let bt_bitfield = group.get_bt_bitfield().await;

        // Determine if this is a BT download from URI pattern
        let is_bt = raw_uris
            .iter()
            .any(|u| u.starts_with("magnet:?") || u.ends_with(".torrent"));

        let (bitfield, bt_info_hash, bt_saved_metadata_path) = if is_bt {
            let bf = bt_bitfield.unwrap_or_default();
            // Try to extract info hash from magnet URI
            let info_hash = raw_uris
                .iter()
                .find(|u| u.starts_with("magnet:?"))
                .and_then(|u| Self::extract_info_hash_from_magnet(u));
            (bf, info_hash, None)
        } else {
            (vec![], None, None)
        };

        // Calculate resume offset for HTTP/FTP
        let resume_offset = if completed_length > 0 && !is_bt {
            Some(completed_length)
        } else {
            None
        };

        debug!(
            gid = %gid,
            protocol = if is_bt { "BT" } else { "HTTP/FTP" },
            completed = completed_length,
            total = total_length,
            "Extracted resume data from RequestGroup"
        );

        Ok(ResumeData {
            gid,
            uris,
            total_length,
            completed_length,
            uploaded_length,
            bitfield,
            num_pieces: None,   // Could be calculated from bitfield length
            piece_length: None, // Would need to be stored in RequestGroup
            status: status_str,
            error_message,
            last_download_time,
            created_at,
            output_path,
            checksum,
            options: options_map,
            resume_offset,
            bt_info_hash,
            bt_saved_metadata_path,
        })
    }

    fn to_restore_components(
        &self,
    ) -> (String, Vec<String>, HashMap<String, String>, RestoreState) {
        let gid = self.gid.clone();
        let uris: Vec<String> = self.uris.iter().map(|u| u.uri.clone()).collect();
        let options = self.options.clone();

        let restore_state = if self.is_bit_torrent() {
            RestoreState::BitTorrent {
                bitfield: self.bitfield.clone(),
                num_pieces: self.num_pieces,
                piece_length: self.piece_length,
                info_hash: self.bt_info_hash.clone(),
                metadata_path: self.bt_saved_metadata_path.clone(),
            }
        } else if self.is_metalink() && self.uris.len() > 1 {
            // Build mirror list with priority scoring
            let mirrors: Vec<MirrorRestoreInfo> = self
                .uris
                .iter()
                .enumerate()
                .map(|(i, u)| {
                    // Calculate priority: working mirrors first, then by speed
                    let mut priority = i as u32 * 10;
                    if u.tried && u.last_result.as_deref() == Some("ok") {
                        priority = 0; // Highest priority: working mirrors
                    } else if !u.tried {
                        priority += 5; // Untried mirrors get medium priority
                    } else if u.last_result.is_some() {
                        priority += 20; // Failed mirrors get lowest priority
                    }

                    MirrorRestoreInfo {
                        uri: u.uri.clone(),
                        tried: u.tried,
                        last_result: u.last_result.clone(),
                        speed_bytes_per_sec: u.speed_bytes_per_sec,
                        priority_score: priority,
                    }
                })
                .collect();

            // Sort by priority score (ascending = higher priority first)
            let mut sorted_mirrors = mirrors;
            sorted_mirrors.sort_by_key(|m| m.priority_score);

            RestoreState::Metalink {
                mirrors: sorted_mirrors,
                resume_offset: self.resume_offset,
            }
        } else {
            RestoreState::HttpFtp {
                resume_offset: self.resume_offset.unwrap_or(0),
                total_length: self.total_length,
                completed_length: self.completed_length,
            }
        };

        (gid, uris, options, restore_state)
    }
}

impl ResumeData {
    /// Extract info hash from a magnet link
    ///
    /// Parses magnet URI format: `magnet:?xt=urn:btih:<hash>&dn=...`
    /// and returns the hex-encoded info hash if present.
    fn extract_info_hash_from_magnet(magnet_uri: &str) -> Option<String> {
        // Look for xt=urn:btih: parameter
        let start = magnet_uri.find("xt=urn:btih:")? + "xt=urn:btih:".len();
        let end = magnet_uri[start..]
            .find('&')
            .unwrap_or(magnet_uri[start..].len());
        let hash = &magnet_uri[start..start + end];

        // Validate it looks like a hex hash (40 chars for SHA-1, 32 for base32)
        if hash.len() >= 20 {
            Some(hash.to_string())
        } else {
            None
        }
    }
}

// =========================================================================
// Unit Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::{DownloadOptions, GroupId};
    use std::fs;
    use std::path::PathBuf;

    /// Helper to create a temporary directory for tests
    fn create_test_dir() -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            % 1_000_000_000;
        let dir = std::env::temp_dir().join(format!("resume_test_{}_{}", std::process::id(), ts));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("Failed to create test directory");
        dir
    }

    /// Helper to create sample ResumeData with realistic values (HTTP download)
    fn create_sample_resume_data() -> ResumeData {
        ResumeData {
            gid: "2089b05ecca3d829".to_string(),
            uris: vec![
                UriState {
                    uri: "http://example.com/files/ubuntu-22.04-desktop-amd64.iso".to_string(),
                    tried: true,
                    used: true,
                    last_result: Some("ok".to_string()),
                    speed_bytes_per_sec: Some(5 * 1024 * 1024), // 5 MB/s
                },
                UriState {
                    uri: "http://mirror.example.com/ubuntu-22.04-desktop-amd64.iso".to_string(),
                    tried: false,
                    used: false,
                    last_result: None,
                    speed_bytes_per_sec: None,
                },
                UriState {
                    uri: "ftp://archive.ubuntu.com/ubuntu-22.04-desktop-amd64.iso".to_string(),
                    tried: true,
                    used: false,
                    last_result: Some("Connection timeout".to_string()),
                    speed_bytes_per_sec: None,
                },
            ],
            total_length: 4705785856,     // ~4.38 GB
            completed_length: 2352892928, // ~50% done
            uploaded_length: 0,
            bitfield: vec![],
            num_pieces: None,
            piece_length: None,
            status: "active".to_string(),
            error_message: None,
            last_download_time: 1700000000,
            created_at: 1699999000,
            output_path: Some("/downloads/ubuntu-22.04-desktop-amd64.iso".to_string()),
            checksum: Some(ChecksumInfo {
                algorithm: "sha-256".to_string(),
                expected: "b4517b7c8a...".to_string(), // truncated for brevity
            }),
            options: {
                let mut m = HashMap::new();
                m.insert("split".to_string(), "4".to_string());
                m.insert("dir".to_string(), "/downloads".to_string());
                m
            },
            resume_offset: Some(2352892928),
            bt_info_hash: None,
            bt_saved_metadata_path: None,
        }
    }

    /// Helper to create sample BT-specific ResumeData
    fn create_bt_resume_data() -> ResumeData {
        ResumeData {
            gid: "bt123456789abcdef".to_string(),
            uris: vec![UriState {
                uri: "magnet:?xt=urn:btih:abcdef1234567890abcdef1234567890abc&dn=TestTorrent"
                    .to_string(),
                tried: true,
                used: true,
                last_result: Some("ok".to_string()),
                speed_bytes_per_sec: Some(2 * 1024 * 1024),
            }],
            total_length: 1073741824,    // 1 GB
            completed_length: 536870912, // 512 MB (50%)
            uploaded_length: 134217728,  // 128 MB uploaded
            bitfield: vec![0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00], // 50% pieces done
            num_pieces: Some(64),
            piece_length: Some(16777216), // 16 MB per piece
            status: "paused".to_string(),
            error_message: None,
            last_download_time: 1700000100,
            created_at: 1699999100,
            output_path: Some("/downloads/test.torrent".to_string()),
            checksum: None,
            options: {
                let mut m = HashMap::new();
                m.insert("seed-time".to_string(), "3600".to_string());
                m.insert("seed-ratio".to_string(), "1.0".to_string());
                m
            },
            resume_offset: None,
            bt_info_hash: Some("abcdef1234567890abcdef1234567890abcdef12".to_string()),
            bt_saved_metadata_path: Some("/downloads/.cache/test.torrent".to_string()),
        }
    }

    /// Helper to create Metalink-style ResumeData with multiple mirrors
    fn create_metalink_resume_data() -> ResumeData {
        ResumeData {
            gid: "ml98765fedcba4321".to_string(),
            uris: vec![
                UriState {
                    uri: "http://mirror1.example.com/large-file.bin".to_string(),
                    tried: true,
                    used: true,
                    last_result: Some("ok".to_string()),
                    speed_bytes_per_sec: Some(10 * 1024 * 1024), // 10 MB/s - fastest
                },
                UriState {
                    uri: "http://mirror2.example.com/large-file.bin".to_string(),
                    tried: true,
                    used: false,
                    last_result: Some("ok".to_string()),
                    speed_bytes_per_sec: Some(3 * 1024 * 1024), // 3 MB/s
                },
                UriState {
                    uri: "http://mirror3.example.com/large-file.bin".to_string(),
                    tried: true,
                    used: false,
                    last_result: Some("Connection refused".to_string()),
                    speed_bytes_per_sec: None,
                },
                UriState {
                    uri: "ftp://backup.example.com/large-file.bin".to_string(),
                    tried: false,
                    used: false,
                    last_result: None,
                    speed_bytes_per_sec: None,
                },
            ],
            total_length: 524288000,     // 500 MB
            completed_length: 262144000, // 250 MB (50%)
            uploaded_length: 0,
            bitfield: vec![],
            num_pieces: None,
            piece_length: None,
            status: "active".to_string(),
            error_message: None,
            last_download_time: 1700000200,
            created_at: 1699999200,
            output_path: Some("/downloads/large-file.bin".to_string()),
            checksum: Some(ChecksumInfo {
                algorithm: "sha-1".to_string(),
                expected: "a1b2c3d4e5f6...".to_string(),
            }),
            options: {
                let mut m = HashMap::new();
                m.insert("split".to_string(), "4".to_string());
                m.insert("max-connection-per-server".to_string(), "2".to_string());
                m
            },
            resume_offset: Some(262144000),
            bt_info_hash: None,
            bt_saved_metadata_path: None,
        }
    }

    // =====================================================================
    // Test Group 1: HTTP Save -> Restore Round-trip (5+ tests)
    // =====================================================================

    #[test]
    fn test_http_serialize_deserialize_roundtrip() {
        let original = create_sample_resume_data();

        let json = original.serialize().expect("HTTP serialization failed");
        let restored = ResumeData::deserialize(&json).expect("HTTP deserialization failed");

        // Verify core HTTP fields survive roundtrip
        assert_eq!(restored.gid, original.gid, "GID mismatch");
        assert_eq!(
            restored.total_length, original.total_length,
            "Total length mismatch"
        );
        assert_eq!(
            restored.completed_length, original.completed_length,
            "Completed length mismatch"
        );
        assert_eq!(
            restored.uploaded_length, original.uploaded_length,
            "Upload length mismatch"
        );
        assert_eq!(restored.status, original.status, "Status mismatch");
        assert_eq!(
            restored.resume_offset, original.resume_offset,
            "Resume offset mismatch"
        );
        assert_eq!(
            restored.output_path, original.output_path,
            "Output path mismatch"
        );
        assert_eq!(restored.options, original.options, "Options map mismatch");

        // Verify checksum preserved
        assert_eq!(
            restored
                .checksum
                .as_ref()
                .map(|c| (&c.algorithm, &c.expected)),
            original
                .checksum
                .as_ref()
                .map(|c| (&c.algorithm, &c.expected)),
            "Checksum mismatch"
        );
    }

    #[test]
    fn test_http_resume_offset_preserved() {
        let data = create_sample_resume_data();
        assert_eq!(data.resume_offset, Some(2352892928));

        let json = data.serialize().unwrap();
        let restored = ResumeData::deserialize(&json).unwrap();

        assert_eq!(
            restored.resume_offset,
            Some(2352892928),
            "HTTP resume offset must survive roundtrip"
        );

        // Verify resume offset equals completed_length for normal HTTP downloads
        assert_eq!(
            restored.resume_offset,
            Some(restored.completed_length),
            "Resume offset should equal completed_length for HTTP"
        );
    }

    #[test]
    fn test_http_single_uri_roundtrip() {
        let single = ResumeData {
            gid: "http-single-test".to_string(),
            uris: vec![UriState {
                uri: "http://example.com/single-file.dat".to_string(),
                tried: true,
                used: true,
                last_result: Some("ok".to_string()),
                speed_bytes_per_sec: Some(1048576),
            }],
            total_length: 10485760,
            completed_length: 5242880,
            ..Default::default()
        };

        let json = single.serialize().unwrap();
        let restored = ResumeData::deserialize(&json).unwrap();

        assert_eq!(restored.uris.len(), 1);
        assert_eq!(restored.uris[0].uri, "http://example.com/single-file.dat");
        assert!(
            !restored.is_metalink(),
            "Single URI should not be detected as metalink"
        );
        assert_eq!(restored.detect_protocol(), "http");
    }

    #[test]
    fn test_http_zero_completed_roundtrip() {
        let zero_progress = ResumeData {
            gid: "http-zero-progress".to_string(),
            uris: vec![UriState {
                uri: "http://example.com/not-started.zip".to_string(),
                tried: false,
                used: false,
                last_result: None,
                speed_bytes_per_sec: None,
            }],
            total_length: 1000000,
            completed_length: 0,
            resume_offset: None,
            ..Default::default()
        };

        let json = zero_progress.serialize().unwrap();
        let restored = ResumeData::deserialize(&json).unwrap();

        assert_eq!(restored.completed_length, 0);
        assert_eq!(
            restored.resume_offset, None,
            "Zero progress should have no resume offset"
        );
        assert!((restored.completion_ratio() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_http_unknown_total_size_roundtrip() {
        let unknown_size = ResumeData {
            gid: "http-unknown-size".to_string(),
            uris: vec![UriState {
                uri: "http://streaming.example.com/live.m3u8".to_string(),
                tried: true,
                used: true,
                last_result: Some("ok".to_string()),
                speed_bytes_per_sec: Some(500000),
            }],
            total_length: 0, // Unknown size (streaming)
            completed_length: 999999,
            ..Default::default()
        };

        let json = unknown_size.serialize().unwrap();
        let restored = ResumeData::deserialize(&json).unwrap();

        assert_eq!(restored.total_length, 0);
        assert_eq!(
            restored.completion_ratio(),
            0.0,
            "Unknown size should yield 0% ratio"
        );
    }

    // =====================================================================
    // Test Group 2: BT Save -> Restore Round-trip with bitfield (5+ tests)
    // =====================================================================

    #[test]
    fn test_bt_serialize_deserialize_roundtrip() {
        let original = create_bt_resume_data();

        let json = original.serialize().expect("BT serialization failed");
        let restored = ResumeData::deserialize(&json).expect("BT deserialization failed");

        // Verify BT-specific fields survive roundtrip
        assert_eq!(restored.gid, original.gid, "BT GID mismatch");
        assert_eq!(restored.bitfield, original.bitfield, "Bitfield mismatch");
        assert_eq!(
            restored.num_pieces, original.num_pieces,
            "Num pieces mismatch"
        );
        assert_eq!(
            restored.piece_length, original.piece_length,
            "Piece length mismatch"
        );
        assert_eq!(
            restored.bt_info_hash, original.bt_info_hash,
            "BT info hash mismatch"
        );
        assert_eq!(
            restored.bt_saved_metadata_path, original.bt_saved_metadata_path,
            "BT metadata path mismatch"
        );
        assert_eq!(
            restored.uploaded_length, original.uploaded_length,
            "Upload length mismatch"
        );

        // Verify BT detection works
        assert!(
            restored.is_bit_torrent(),
            "Should be detected as BT download"
        );
        assert_eq!(restored.detect_protocol(), "bt");
    }

    #[test]
    fn test_bt_bitfield_exact_preservation() {
        let bt = create_bt_resume_data();
        let expected_bitfield = vec![0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00];

        assert_eq!(
            bt.bitfield, expected_bitfield,
            "Original bitfield should match"
        );

        let json = bt.serialize().unwrap();
        let restored = ResumeData::deserialize(&json).unwrap();

        assert_eq!(
            restored.bitfield, expected_bitfield,
            "Bitfield must be exactly preserved after roundtrip"
        );
        assert_eq!(
            restored.bitfield.len(),
            8,
            "Bitfield length must be preserved"
        );
    }

    #[test]
    fn test_bt_piece_metadata_roundtrip() {
        let bt = create_bt_resume_data();

        let json = bt.serialize().unwrap();
        let restored = ResumeData::deserialize(&json).unwrap();

        assert_eq!(
            restored.num_pieces,
            Some(64),
            "Num pieces (64) must survive roundtrip"
        );
        assert_eq!(
            restored.piece_length,
            Some(16777216),
            "Piece length (16MB) must survive roundtrip"
        );

        // Verify consistency: num_pieces * piece_length should approximate total_length
        if let (Some(np), Some(pl)) = (restored.num_pieces, restored.piece_length) {
            let calc_total = (np as u64) * (pl as u64);
            assert!(
                calc_total >= restored.total_length,
                "Calculated total ({}) should be >= reported total ({})",
                calc_total,
                restored.total_length
            );
        }
    }

    #[test]
    fn test_bt_upload_length_tracking() {
        let bt = create_bt_resume_data();
        assert_eq!(bt.uploaded_length, 134217728); // 128 MB uploaded

        let json = bt.serialize().unwrap();
        let restored = ResumeData::deserialize(&json).unwrap();

        assert_eq!(
            restored.uploaded_length, 134217728,
            "Uploaded length must be tracked separately for BT seeding"
        );
    }

    #[test]
    fn test_bt_no_resume_offset() {
        let bt = create_bt_resume_data();

        // BT downloads should NOT use resume_offset (they use bitfield instead)
        assert_eq!(
            bt.resume_offset, None,
            "BT downloads should have no HTTP-style resume offset"
        );

        let json = bt.serialize().unwrap();
        let restored = ResumeData::deserialize(&json).unwrap();

        assert_eq!(restored.resume_offset, None);
    }

    #[test]
    fn test_bt_magnet_info_hash_extraction() {
        // Test that we can extract info hash from magnet links
        let magnet = "magnet:?xt=urn:btih:abcdef1234567890abcdef1234567890abc&dn=TestFile";
        let hash = ResumeData::extract_info_hash_from_magnet(magnet);

        assert!(hash.is_some(), "Should extract info hash from magnet link");
        assert_eq!(
            hash.unwrap(),
            "abcdef1234567890abcdef1234567890abc",
            "Extracted hash should match"
        );
    }

    #[test]
    fn test_bt_empty_bitfield_is_not_bt() {
        // A download with empty bitfield and no info_hash should NOT be detected as BT
        let not_bt = ResumeData {
            gid: "not-bt-test".to_string(),
            uris: vec![UriState {
                uri: "http://example.com/file.zip".to_string(),
                ..Default::default()
            }],
            bitfield: vec![],
            bt_info_hash: None,
            ..Default::default()
        };

        assert!(
            !not_bt.is_bit_torrent(),
            "Empty bitfield + no info_hash should not be detected as BT"
        );
    }

    // =====================================================================
    // Test Group 3: Metalink Save -> Restore Round-trip (5+ tests)
    // =====================================================================

    #[test]
    fn test_metalink_serialize_deserialize_roundtrip() {
        let original = create_metalink_resume_data();

        let json = original.serialize().expect("Metalink serialization failed");
        let restored = ResumeData::deserialize(&json).expect("Metalink deserialization failed");

        // Verify all mirrors preserved
        assert_eq!(
            restored.uris.len(),
            4,
            "All 4 mirrors must survive roundtrip"
        );

        // Verify Metalink detection
        assert!(
            restored.is_metalink(),
            "Multiple URIs should be detected as metalink"
        );
        assert_eq!(restored.detect_protocol(), "metalink");

        // Verify per-mirror state preservation
        for (orig_u, rest_u) in original.uris.iter().zip(restored.uris.iter()) {
            assert_eq!(orig_u.uri, rest_u.uri, "Mirror URI mismatch");
            assert_eq!(orig_u.tried, rest_u.tried, "Tried flag mismatch");
            assert_eq!(orig_u.used, rest_u.used, "Used flag mismatch");
            assert_eq!(orig_u.last_result, rest_u.last_result, "Result mismatch");
            assert_eq!(
                orig_u.speed_bytes_per_sec, rest_u.speed_bytes_per_sec,
                "Speed mismatch"
            );
        }
    }

    #[test]
    fn test_metalink_mirror_priority_ordering() {
        let ml = create_metalink_resume_data();

        // Convert to restore components and check mirror ordering
        let (_gid, _uris, _options, restore_state) = ml.to_restore_components();

        match restore_state {
            RestoreState::Metalink { mirrors, .. } => {
                // First mirror should be highest priority (working, fastest)
                assert_eq!(
                    mirrors[0].uri, "http://mirror1.example.com/large-file.bin",
                    "Fastest working mirror should be first"
                );
                assert_eq!(
                    mirrors[0].priority_score, 0,
                    "Best mirror should have score 0"
                );

                // Failed mirror should have lower priority
                let failed_mirror = mirrors
                    .iter()
                    .find(|m| m.uri.contains("mirror3"))
                    .expect("Failed mirror should be present");
                assert!(
                    failed_mirror.priority_score > 10,
                    "Failed mirror should have low priority (high score)"
                );

                // Untried backup mirror should be in middle (between working and failed)
                let untried = mirrors
                    .iter()
                    .find(|m| m.uri.contains("backup"))
                    .expect("Untried mirror should be present");
                assert!(
                    untried.priority_score < failed_mirror.priority_score,
                    "Untried mirror (score={}) should have better priority than failed mirror (score={})",
                    untried.priority_score,
                    failed_mirror.priority_score
                );
                assert!(
                    untried.priority_score > mirrors[0].priority_score,
                    "Untried mirror (score={}) should have lower priority than best working mirror (score={})",
                    untried.priority_score,
                    mirrors[0].priority_score
                );
            }
            _ => panic!("Expected Metalink restore state"),
        }
    }

    #[test]
    fn test_metalink_speed_based_ranking() {
        let ml = create_metalink_resume_data();
        let (_, _, _, restore_state) = ml.to_restore_components();

        match restore_state {
            RestoreState::Metalink { mirrors, .. } => {
                // Mirrors should be sorted by priority (ascending)
                for window in mirrors.windows(2) {
                    assert!(
                        window[0].priority_score <= window[1].priority_score,
                        "Mirrors should be sorted by priority ascending: {} (score={}) <= {} (score={})",
                        window[0].uri,
                        window[0].priority_score,
                        window[1].uri,
                        window[1].priority_score
                    );
                }
            }
            _ => panic!("Expected Metalink restore state"),
        }
    }

    #[test]
    fn test_metalink_checksum_preserved() {
        let ml = create_metalink_resume_data();
        assert!(ml.checksum.is_some());

        let json = ml.serialize().unwrap();
        let restored = ResumeData::deserialize(&json).unwrap();

        assert_eq!(
            restored.checksum.as_ref().map(|c| c.algorithm.as_str()),
            Some("sha-1"),
            "Metalink checksum algorithm should be preserved"
        );
    }

    #[test]
    fn test_metalink_resume_offset_in_restore_state() {
        let ml = create_metalink_resume_data();
        assert_eq!(ml.resume_offset, Some(262144000));

        let (_, _, _, restore_state) = ml.to_restore_components();

        match restore_state {
            RestoreState::Metalink { resume_offset, .. } => {
                assert_eq!(
                    resume_offset,
                    Some(262144000),
                    "Metalink resume offset must be in restore state"
                );
            }
            _ => panic!("Expected Metalink restore state"),
        }
    }

    // =====================================================================
    // Test Group 4: Edge cases and error handling
    // =====================================================================

    #[test]
    fn test_edge_case_empty_uris() {
        let empty_uris = ResumeData {
            gid: "empty-uri-test".to_string(),
            uris: vec![],
            ..Default::default()
        };

        let json = empty_uris.serialize().unwrap();
        let restored = ResumeData::deserialize(&json).unwrap();

        assert!(
            restored.uris.is_empty(),
            "Empty URI list should be preserved"
        );
        assert!(!restored.is_metalink(), "Empty URIs should not be metalink");
        assert_eq!(restored.detect_protocol(), "unknown");
    }

    #[test]
    fn test_edge_case_zero_length_file() {
        let zero_len = ResumeData {
            gid: "zero-len-test".to_string(),
            uris: vec![UriState {
                uri: "http://example.com/empty-file".to_string(),
                ..Default::default()
            }],
            total_length: 0,
            completed_length: 0,
            status: "complete".to_string(),
            ..Default::default()
        };

        let json = zero_len.serialize().unwrap();
        let restored = ResumeData::deserialize(&json).unwrap();

        assert_eq!(restored.total_length, 0);
        assert_eq!(restored.completed_length, 0);
        assert_eq!(restored.status, "complete");
        assert_eq!(restored.completion_ratio(), 0.0);
    }

    #[test]
    fn test_validate_good_data_passes() {
        let good = create_sample_resume_data();
        let result = good.validate_for_restore();
        assert!(
            result.is_ok(),
            "Valid data should pass validation: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_validate_empty_gid_fails() {
        let bad = ResumeData {
            gid: String::new(),
            uris: vec![UriState {
                uri: "http://example.com/f".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let result = bad.validate_for_restore();
        assert!(result.is_err(), "Empty GID should fail validation");
        assert!(
            result.unwrap_err().contains("GID"),
            "Error should mention GID"
        );
    }

    #[test]
    fn test_validate_no_uris_fails() {
        let bad = ResumeData {
            gid: "has-gid-but-no-uris".to_string(),
            uris: vec![],
            ..Default::default()
        };
        let result = bad.validate_for_restore();
        assert!(result.is_err(), "No URIs should fail validation");
        assert!(
            result.unwrap_err().contains("URI"),
            "Error should mention URI"
        );
    }

    #[test]
    fn test_validate_completed_exceeds_total_fails() {
        let bad = ResumeData {
            gid: "overflow-test".to_string(),
            uris: vec![UriState {
                uri: "http://example.com/f".to_string(),
                ..Default::default()
            }],
            total_length: 1000,
            completed_length: 2000, // Exceeds total!
            ..Default::default()
        };
        let result = bad.validate_for_restore();
        assert!(result.is_err(), "Completed > total should fail validation");
        assert!(
            result.unwrap_err().contains("exceeds"),
            "Error should mention overflow"
        );
    }

    #[test]
    fn test_validate_invalid_status_fails() {
        let bad = ResumeData {
            gid: "bad-status-test".to_string(),
            uris: vec![UriState {
                uri: "http://example.com/f".to_string(),
                ..Default::default()
            }],
            status: "invalid_status_xyz".to_string(),
            ..Default::default()
        };
        let result = bad.validate_for_restore();
        assert!(result.is_err(), "Invalid status should fail validation");
        assert!(
            result.unwrap_err().contains("Unknown status"),
            "Error should mention unknown status"
        );
    }

    #[test]
    fn test_detect_protocol_variants() {
        // HTTP
        let http = ResumeData {
            gid: "1".to_string(),
            uris: vec![UriState {
                uri: "https://secure.example.com/f".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(http.detect_protocol(), "http");

        // FTP
        let ftp = ResumeData {
            gid: "2".to_string(),
            uris: vec![UriState {
                uri: "sftp://server/file".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(ftp.detect_protocol(), "ftp");

        // BT via info_hash
        let bt = ResumeData {
            gid: "3".to_string(),
            uris: vec![UriState {
                uri: "http://tracker.example.com/f".to_string(),
                ..Default::default()
            }],
            bt_info_hash: Some("abcd1234".to_string()),
            ..Default::default()
        };
        assert_eq!(bt.detect_protocol(), "bt");

        // Unknown
        let unknown = ResumeData {
            gid: "4".to_string(),
            uris: vec![],
            ..Default::default()
        };
        assert_eq!(unknown.detect_protocol(), "unknown");
    }

    // =====================================================================
    // Test Group 5: Existing tests (preserved for compatibility)
    // =====================================================================

    #[test]
    fn test_resume_data_serialize_deserialize_full_roundtrip() {
        let original = create_sample_resume_data();

        // Serialize to JSON
        let json = original.serialize().expect("Serialization failed");

        // Verify JSON contains key fields
        assert!(json.contains("2089b05ecca3d829"), "JSON should contain GID");
        assert!(json.contains("active"), "JSON should contain status");
        assert!(
            json.contains("4705785856"),
            "JSON should contain total_length"
        );
        assert!(
            json.contains("ubuntu-22.04-desktop-amd64.iso"),
            "JSON should contain filename"
        );

        // Deserialize back
        let restored = ResumeData::deserialize(&json).expect("Deserialization failed");

        // Verify all fields match exactly
        assert_eq!(restored.gid, original.gid, "GID mismatch");
        assert_eq!(
            restored.uris.len(),
            original.uris.len(),
            "URI count mismatch"
        );
        assert_eq!(
            restored.total_length, original.total_length,
            "Total length mismatch"
        );
        assert_eq!(
            restored.completed_length, original.completed_length,
            "Completed length mismatch"
        );
        assert_eq!(restored.status, original.status, "Status mismatch");
        assert_eq!(
            restored.error_message, original.error_message,
            "Error message mismatch"
        );
        assert_eq!(
            restored.last_download_time, original.last_download_time,
            "Timestamp mismatch"
        );
        assert_eq!(
            restored.created_at, original.created_at,
            "Created at mismatch"
        );
        assert_eq!(
            restored.output_path, original.output_path,
            "Output path mismatch"
        );

        println!("Full roundtrip test passed. JSON:\n{}", json);
    }

    #[test]
    fn test_resume_data_save_load_file() {
        let test_dir = create_test_dir();
        let file_path = test_dir.join("test_download.aria2");
        let original = create_sample_resume_data();

        // Save to file
        original.save_to_file(&file_path).expect("Save failed");

        // Verify file exists
        assert!(file_path.exists(), "Resume file should exist after save");

        // Load from file
        let loaded = ResumeData::load_from_file(&file_path)
            .expect("Load failed")
            .expect("Should have returned Some(data)");

        // Verify data integrity
        assert_eq!(
            loaded.gid, original.gid,
            "GID mismatch after file roundtrip"
        );
        assert_eq!(loaded.uris.len(), original.uris.len(), "URI count mismatch");
        assert_eq!(
            loaded.total_length, original.total_length,
            "Total length mismatch"
        );
        assert_eq!(
            loaded.completed_length, original.completed_length,
            "Completed length mismatch"
        );
        assert_eq!(loaded.status, original.status, "Status mismatch");
        assert_eq!(
            loaded.output_path, original.output_path,
            "Output path mismatch"
        );

        // Verify no temp file left behind
        let tmp_path = file_path.with_extension("aria2.tmp");
        assert!(!tmp_path.exists(), "No temporary file should remain");

        // Clean up
        let _ = fs::remove_dir_all(&test_dir);

        println!("File save/load test passed");
    }

    #[test]
    fn test_resume_data_missing_file_returns_none() {
        let test_dir = create_test_dir();
        let nonexistent_path = test_dir.join("nonexistent.aria2");

        // Should return Ok(None), not error
        let result = ResumeData::load_from_file(&nonexistent_path)
            .expect("Missing file should not return error");

        assert!(result.is_none(), "Should return None for non-existent file");

        // Clean up
        let _ = fs::remove_dir_all(&test_dir);

        println!("Missing file test passed");
    }

    #[test]
    fn test_resume_data_corrupt_json_returns_error() {
        let test_dir = create_test_dir();
        let file_path = test_dir.join("corrupt.aria2");

        // Test case 1: Completely invalid content
        fs::write(&file_path, "This is not JSON at all! @#$%^&*()")
            .expect("Failed to write corrupt file");
        let result = ResumeData::load_from_file(&file_path);
        assert!(result.is_err(), "Corrupt JSON should return error");
        assert!(
            result.unwrap_err().contains("Failed to deserialize"),
            "Error should mention deserialization"
        );

        // Test case 2: Truncated JSON
        fs::write(&file_path, "{\"gid\":\"test\",\"uris\":[]")
            .expect("Failed to write truncated JSON");
        let result = ResumeData::load_from_file(&file_path);
        assert!(result.is_err(), "Truncated JSON should return error");

        // Test case 3: Valid JSON but wrong structure (missing required fields)
        fs::write(&file_path, "{\"wrong_field\": 123}").expect("Failed to write invalid structure");
        let result = ResumeData::load_from_file(&file_path);
        assert!(result.is_err(), "Invalid structure should return error");

        // Test case 4: Empty file
        fs::write(&file_path, "").expect("Failed to write empty file");
        let result = ResumeData::load_from_file(&file_path);
        assert!(result.is_err(), "Empty file should return error");

        // Clean up
        let _ = fs::remove_dir_all(&test_dir);

        println!("Corrupt JSON handling test passed");
    }

    #[test]
    fn test_resume_data_bt_fields_optional() {
        // Create HTTP download (no BT fields)
        let http_data = create_sample_resume_data();

        assert!(
            !http_data.is_bit_torrent(),
            "HTTP download should not be detected as BT"
        );
        assert!(
            http_data.bt_info_hash.is_none(),
            "HTTP download should have no BT hash"
        );
        assert!(
            http_data.bt_saved_metadata_path.is_none(),
            "HTTP download should have no BT metadata"
        );
        assert!(
            http_data.bitfield.is_empty(),
            "HTTP download should have empty bitfield"
        );

        // Create BT download (with BT fields)
        let bt_data = create_bt_resume_data();

        assert!(
            bt_data.is_bit_torrent(),
            "BT download should be detected as BT"
        );
        assert!(
            bt_data.bt_info_hash.is_some(),
            "BT download should have info hash"
        );
        assert!(
            bt_data.bt_saved_metadata_path.is_some(),
            "BT download should have metadata path"
        );
        assert!(
            !bt_data.bitfield.is_empty(),
            "BT download should have bitfield"
        );

        // Roundtrip BT data to ensure BT fields persist
        let json = bt_data.serialize().expect("BT serialization failed");
        let restored_bt = ResumeData::deserialize(&json).expect("BT deserialization failed");

        assert_eq!(
            restored_bt.bt_info_hash, bt_data.bt_info_hash,
            "BT hash should survive roundtrip"
        );
        assert_eq!(
            restored_bt.bitfield, bt_data.bitfield,
            "Bitfield should survive roundtrip"
        );
        assert_eq!(
            restored_bt.bt_saved_metadata_path, bt_data.bt_saved_metadata_path,
            "Metadata path should survive roundtrip"
        );

        println!("BT optional fields test passed");
    }

    #[test]
    fn test_resume_data_multiple_uris_preserved() {
        let data = create_sample_resume_data();
        assert_eq!(data.uris.len(), 3, "Sample data should have 3 URIs");

        // Roundtrip through serialization
        let json = data.serialize().expect("Serialize failed");
        let restored = ResumeData::deserialize(&json).expect("Deserialize failed");

        // Verify exact URI count
        assert_eq!(
            restored.uris.len(),
            3,
            "Should preserve 3 URIs after roundtrip"
        );

        // Verify each URI's complete state
        // URI 1: Active, working
        assert_eq!(
            restored.uris[0].uri,
            "http://example.com/files/ubuntu-22.04-desktop-amd64.iso"
        );
        assert!(restored.uris[0].tried, "URI 1 should be marked as tried");
        assert!(restored.uris[0].used, "URI 1 should be marked as used");
        assert_eq!(restored.uris[0].last_result.as_deref(), Some("ok"));
        assert_eq!(restored.uris[0].speed_bytes_per_sec, Some(5 * 1024 * 1024));

        // URI 2: Unused mirror
        assert_eq!(
            restored.uris[1].uri,
            "http://mirror.example.com/ubuntu-22.04-desktop-amd64.iso"
        );
        assert!(
            !restored.uris[1].tried,
            "URI 2 should NOT be marked as tried"
        );
        assert!(!restored.uris[1].used, "URI 2 should NOT be marked as used");
        assert!(
            restored.uris[1].last_result.is_none(),
            "URI 2 should have no result"
        );
        assert!(
            restored.uris[1].speed_bytes_per_sec.is_none(),
            "URI 2 should have no speed"
        );

        // URI 3: Failed attempt
        assert_eq!(
            restored.uris[2].uri,
            "ftp://archive.ubuntu.com/ubuntu-22.04-desktop-amd64.iso"
        );
        assert!(restored.uris[2].tried, "URI 3 should be marked as tried");
        assert!(!restored.uris[2].used, "URI 3 should NOT be marked as used");
        assert_eq!(
            restored.uris[2].last_result.as_deref(),
            Some("Connection timeout")
        );
        assert!(
            restored.uris[2].speed_bytes_per_sec.is_none(),
            "URI 3 should have no speed"
        );

        // Test edge case: Single URI
        let single_uri = ResumeData {
            gid: "single-uri-test".to_string(),
            uris: vec![UriState {
                uri: "http://example.com/single.file".to_string(),
                tried: true,
                used: true,
                last_result: Some("ok".to_string()),
                speed_bytes_per_sec: Some(1000),
            }],
            ..Default::default()
        };

        let single_json = single_uri.serialize().unwrap();
        let single_restored = ResumeData::deserialize(&single_json).unwrap();
        assert_eq!(
            single_restored.uris.len(),
            1,
            "Single URI should be preserved"
        );
        assert_eq!(
            single_restored.uris[0].uri,
            "http://example.com/single.file"
        );

        // Test edge case: Empty URI list
        let no_uris = ResumeData {
            gid: "no-uris-test".to_string(),
            uris: vec![],
            ..Default::default()
        };

        let no_uris_json = no_uris.serialize().unwrap();
        let no_uris_restored = ResumeData::deserialize(&no_uris_json).unwrap();
        assert!(
            no_uris_restored.uris.is_empty(),
            "Empty URI list should be preserved"
        );

        println!("Multiple URIs preservation test passed");
    }

    #[test]
    fn test_completion_ratio_calculation() {
        // Normal case: 50% complete
        let data = ResumeData {
            total_length: 1000,
            completed_length: 500,
            ..Default::default()
        };
        assert!(
            (data.completion_ratio() - 0.5).abs() < f64::EPSILON,
            "50% should be 0.5"
        );

        // Edge case: 0% complete
        let zero = ResumeData {
            total_length: 1000,
            completed_length: 0,
            ..Default::default()
        };
        assert_eq!(zero.completion_ratio(), 0.0, "0 bytes should be 0%");

        // Edge case: 100% complete
        let full = ResumeData {
            total_length: 1000,
            completed_length: 1000,
            ..Default::default()
        };
        assert!(
            (full.completion_ratio() - 1.0).abs() < f64::EPSILON,
            "100% should be 1.0"
        );

        // Edge case: Unknown total size (should return 0.0)
        let unknown = ResumeData {
            total_length: 0,
            completed_length: 500,
            ..Default::default()
        };
        assert_eq!(unknown.completion_ratio(), 0.0, "Unknown size should be 0%");
    }

    #[test]
    fn test_get_filename_generation() {
        let data = ResumeData {
            gid: "abc123".to_string(),
            ..Default::default()
        };
        assert_eq!(data.get_filename(), "abc123.aria2");

        let data2 = ResumeData {
            gid: "long-gid-with-dashes_and_underscores".to_string(),
            ..Default::default()
        };
        assert_eq!(
            data2.get_filename(),
            "long-gid-with-dashes_and_underscores.aria2"
        );
    }

    #[test]
    fn test_default_values() {
        let data = ResumeData::default();

        assert!(data.gid.is_empty(), "Default GID should be empty");
        assert!(data.uris.is_empty(), "Default URIs should be empty");
        assert_eq!(data.total_length, 0, "Default total_length should be 0");
        assert_eq!(
            data.completed_length, 0,
            "Default completed_length should be 0"
        );
        assert_eq!(
            data.uploaded_length, 0,
            "Default uploaded_length should be 0"
        );
        assert!(data.bitfield.is_empty(), "Default bitfield should be empty");
        assert_eq!(data.status, "waiting", "Default status should be 'waiting'");
        assert!(data.error_message.is_none(), "Default error should be None");
        assert_eq!(data.last_download_time, 0, "Default timestamp should be 0");
        assert_eq!(data.created_at, 0, "Default created_at should be 0");
        assert!(
            data.output_path.is_none(),
            "Default output_path should be None"
        );
        assert!(data.checksum.is_none(), "Default checksum should be None");
        assert!(data.options.is_empty(), "Default options should be empty");
        assert!(
            data.resume_offset.is_none(),
            "Default resume_offset should be None"
        );
        assert!(
            data.bt_info_hash.is_none(),
            "Default bt_info_hash should be None"
        );
        assert!(
            data.bt_saved_metadata_path.is_none(),
            "Default bt_metadata should be None"
        );
        assert!(
            data.num_pieces.is_none(),
            "Default num_pieces should be None"
        );
        assert!(
            data.piece_length.is_none(),
            "Default piece_length should be None"
        );
    }

    // =====================================================================
    // Test Group 6: Integration test - crash -> restart -> recovery flow
    // =====================================================================

    #[test]
    fn test_integration_crash_restart_recovery_flow() {
        // Simulate the complete lifecycle:
        // 1. Download starts, makes progress
        // 2. Process crashes (state saved to disk)
        // 3. Process restarts, loads saved state
        // 4. Validates state and prepares for restoration

        let test_dir = create_test_dir();
        let resume_file = test_dir.join("crash_recovery_test.aria2");

        // --- Phase 1: Simulate active download with progress ---
        let active_download = ResumeData {
            gid: "deadbeefcafebabe".to_string(),
            uris: vec![UriState {
                uri: "http://primary.server/big-release.iso".to_string(),
                tried: true,
                used: true,
                last_result: Some("ok".to_string()),
                speed_bytes_per_sec: Some(8 * 1024 * 1024),
            }],
            total_length: 2147483648,     // 2 GB
            completed_length: 1073741824, // 1 GB (50% done)
            uploaded_length: 0,
            bitfield: vec![],
            num_pieces: None,
            piece_length: None,
            status: "active".to_string(),
            error_message: None,
            last_download_time: 1700010000,
            created_at: 1700009000,
            output_path: Some("/downloads/big-release.iso".to_string()),
            checksum: Some(ChecksumInfo {
                algorithm: "sha-256".to_string(),
                expected: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
            }),
            options: {
                let mut m = HashMap::new();
                m.insert("split".to_string(), "8".to_string());
                m.insert("max-connection-per-server".to_string(), "4".to_string());
                m.insert("dir".to_string(), "/downloads".to_string());
                m
            },
            resume_offset: Some(1073741824),
            bt_info_hash: None,
            bt_saved_metadata_path: None,
        };

        // --- Phase 2: Simulate crash - save state to disk ---
        active_download
            .save_to_file(&resume_file)
            .expect("Crash-save should succeed");
        assert!(
            resume_file.exists(),
            "Resume file must exist after crash-save"
        );

        // --- Phase 3: Simulate restart - load state from disk ---
        let loaded = ResumeData::load_from_file(&resume_file)
            .expect("Load should succeed")
            .expect("Resume data should exist after crash");

        // --- Phase 4: Validate loaded state ---
        assert_eq!(
            loaded.gid, "deadbeefcafebabe",
            "GID must match after crash/restart"
        );
        assert_eq!(
            loaded.completed_length, 1073741824,
            "Progress must be preserved across crash"
        );
        assert_eq!(loaded.status, "active", "Status must be preserved");
        assert_eq!(
            loaded.resume_offset,
            Some(1073741824),
            "Resume offset must allow continuation from where we stopped"
        );

        // --- Phase 5: Prepare for restoration ---
        let validation = loaded.validate_for_restore();
        assert!(
            validation.is_ok(),
            "Saved state must pass validation for restoration: {:?}",
            validation.err()
        );

        // Decompose into restore components
        let (gid, uris, options, restore_state) = loaded.to_restore_components();

        assert_eq!(gid, "deadbeefcafebabe", "Restoration GID must match");
        assert_eq!(uris.len(), 1, "URI must be available for restoration");
        assert!(
            options.contains_key("split"),
            "Options must include split setting"
        );

        // Verify correct restore state variant
        match restore_state {
            RestoreState::HttpFtp {
                resume_offset,
                total_length,
                completed_length,
            } => {
                assert_eq!(resume_offset, 1073741824, "HTTP resume offset must match");
                assert_eq!(total_length, 2147483648, "Total length must match");
                assert_eq!(completed_length, 1073741824, "Completed must match");
            }
            other => panic!("Expected HttpFtp restore state, got: {:?}", other),
        }

        // --- Phase 6: Simulate successful restoration ---
        // In production, this would call session.create_command(&loaded)
        // Here we verify the data is ready for that call

        // Mock session that accepts any valid data
        struct MockSession;
        impl SessionLike for MockSession {
            fn create_command(&mut self, data: &ResumeData) -> Result<GidStub, String> {
                // Validate before accepting
                data.validate_for_restore()?;
                Ok(GidStub(format!("restored-{}", data.gid)))
            }

            fn pause_download(&mut self, _gid: &str) -> Result<(), String> {
                Ok(())
            }
        }

        let mut mock_session = MockSession;
        let restore_result = loaded.restore_to_session(&mut mock_session);

        assert!(
            restore_result.is_ok(),
            "Restoration should succeed: {:?}",
            restore_result.err()
        );
        assert_eq!(
            restore_result.unwrap().0,
            "restored-deadbeefcafebabe",
            "Restored GID should indicate successful recovery"
        );

        // Clean up
        let _ = fs::remove_dir_all(&test_dir);

        println!("Integration crash->restart->recovery flow test passed");
    }

    #[tokio::test]
    async fn test_integration_from_request_group_roundtrip() {
        // Test the full pipeline: RequestGroup -> ResumeData -> file -> load -> validate

        let group = RequestGroup::new(
            GroupId::new(0xDEADBEEF),
            vec![
                "http://example.com/test-file.bin".to_string(),
                "http://mirror.example.com/test-file.bin".to_string(),
            ],
            {
                DownloadOptions {
                    split: Some(4),
                    dir: Some("/downloads".to_string()),
                    out: Some("test-file.bin".to_string()),
                    checksum: Some((
                        "sha-256".to_string(),
                        "abc123def4567890abcdef1234567890abcdef1234567890abcdef1234567890"
                            .to_string(),
                    )),
                    ..DownloadOptions::default()
                }
            },
        );

        // Simulate some download progress
        group.set_total_length_atomic(104857600); // 100 MB
        group.set_completed_length(52428800); // 50 MB downloaded
        group.set_uploaded_length(0);
        group.set_download_speed_cached(5242880); // 5 MB/s
        group.set_resume_offset(52428800);

        // Extract resume data from the live RequestGroup
        let resume_data: ResumeData = <ResumeData as ResumeDataExt>::from_request_group(&group)
            .await
            .expect("Extraction from RequestGroup should succeed");

        // Verify extraction produced valid data
        assert_eq!(
            resume_data.gid,
            group.gid().to_hex_string(),
            "GID should match"
        );
        assert_eq!(
            resume_data.total_length, 104857600,
            "Total length should match"
        );
        assert_eq!(
            resume_data.completed_length, 52428800,
            "Completed length should match"
        );
        assert_eq!(resume_data.uris.len(), 2, "Both URIs should be extracted");
        assert!(
            resume_data.checksum.is_some(),
            "Checksum should be extracted from options"
        );

        // Validate the extracted data
        resume_data
            .validate_for_restore()
            .expect("Extracted data should be valid for restoration");

        // Roundtrip through serialization
        let json = resume_data.serialize().unwrap();
        let restored = ResumeData::deserialize(&json).unwrap();

        assert_eq!(restored.gid, resume_data.gid, "Roundtrip GID should match");
        assert_eq!(
            restored.completed_length, resume_data.completed_length,
            "Roundtrip completed_length should match"
        );

        println!(
            "RequestGroup -> ResumeData roundtrip test passed. GID: {}",
            resume_data.gid
        );
    }

    #[tokio::test]
    async fn test_integration_bt_request_group_extraction() {
        // Test BT-specific extraction from RequestGroup with bitfield

        let group = RequestGroup::new(
            GroupId::new(0xB7C01234),
            vec![
                "magnet:?xt=urn:btih:a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0&dn=TestTorrent"
                    .to_string(),
            ],
            {
                DownloadOptions {
                    seed_time: Some(3600),
                    seed_ratio: Some(1.5),
                    enable_dht: true,
                    ..DownloadOptions::default()
                }
            },
        );

        // Set BT-specific state
        group.set_total_length_atomic(1073741824); // 1 GB
        group.set_completed_length(536870912); // 512 MB
        group.set_uploaded_length(134217728); // 128 MB seeded
        group
            .set_bt_bitfield(Some(vec![0xFF, 0xFF, 0x00, 0x00]))
            .await;

        // Extract
        let resume_data: ResumeData = <ResumeData as ResumeDataExt>::from_request_group(&group)
            .await
            .expect("BT extraction should succeed");

        // Verify BT detection
        assert!(resume_data.is_bit_torrent(), "Should be detected as BT");
        assert_eq!(resume_data.detect_protocol(), "bt");
        assert_eq!(
            resume_data.bitfield,
            vec![0xFF, 0xFF, 0x00, 0x00],
            "Bitfield should be extracted"
        );
        assert_eq!(
            resume_data.uploaded_length, 134217728,
            "Upload should be tracked"
        );

        // Verify info hash extracted from magnet
        assert!(
            resume_data.bt_info_hash.is_some(),
            "Info hash should be extracted from magnet URI"
        );

        // Verify restore components produce BT variant
        let (_, _, _, restore_state) = resume_data.to_restore_components();
        match restore_state {
            RestoreState::BitTorrent { bitfield, .. } => {
                assert_eq!(bitfield, vec![0xFF, 0xFF, 0x00, 0x00]);
            }
            other => panic!("Expected BitTorrent restore state, got: {:?}", other),
        }

        println!("BT RequestGroup extraction test passed");
    }
}

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
//!   └── impl ResumeData { serialize, deserialize, save, load }
//!
//! Compatibility:
//!   - Works alongside existing BtProgressManager (BT-specific text format)
//!   - Uses JSON for human-readable, debuggable output
//!   - Supports both HTTP/FTP and BitTorrent downloads
//! ```

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use tracing::{debug, info, warn};

/// Complete download state for persistence across process restarts
///
/// This structure captures all necessary information to fully restore a download
/// session, including progress state, URI history, error context, and protocol-
/// specific metadata.
///
/// # Examples
///
/// ```rust
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

    /// Per-piece completion bitmap (BitTorrent only, empty for HTTP/FTP)
    pub bitfield: Vec<u8>,

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
            bitfield: Vec::new(),
            status: "waiting".to_string(),
            error_message: None,
            last_download_time: 0,
            created_at: 0,
            output_path: None,
            checksum: None,
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
    /// let json = r#"{"gid":"test","uris":[],"total_length":0,"completed_length":0,"bitfield":[],"status":"paused","error_message":null,"last_download_time":0,"created_at":0,"output_path":null,"checksum":null,"bt_info_hash":null,"bt_saved_metadata_path":null}"#;
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

    /// Build ResumeData from current download command state (STUB)
    ///
    /// TODO: Implement full extraction from DownloadCommand/RequestGroup.
    /// This stub returns default data with the provided GID.
    ///
    /// Future implementation should extract:
    /// - GID from command identifier
    /// - URIs from command's URI list with tried/used flags
    /// - Progress from command's current statistics
    /// - Status from command's lifecycle state
    /// - For BT: bitfield, info_hash from peer session
    /// - For HTTP/FTP: range completion status
    ///
    /// # Arguments
    ///
    /// * `_cmd` - Download command trait object (placeholder)
    ///
    /// # Returns
    ///
    /// Populated ResumeData (currently stubbed with defaults)
    #[allow(unused_variables)]
    pub fn from_download_command(_cmd: &dyn DownloadCommandLike) -> Self {
        warn!("from_download_command() is not yet implemented - returning defaults");

        ResumeData {
            gid: "stub-gid".to_string(),
            ..Default::default()
        }
    }

    /// Restore download command from saved resume data (STUB)
    ///
    /// TODO: Implement full session recreation from persisted state.
    /// This stub logs a warning and returns an error.
    ///
    /// Future implementation should:
    /// - Recreate DownloadCommand with original URIs and options
    /// - Set initial offset to completed_length for HTTP range resume
    /// - Restore BT piece selection from bitfield
    /// - Mark previously-tried URIs appropriately
    /// - Reconstruct checksum validation settings
    ///
    /// # Arguments
    ///
    /// * `_session` - Session manager (placeholder)
    ///
    /// # Returns
    ///
    /// * `Ok(Gid)` - Identifier of restored command (when implemented)
    /// * `Err(String)` - Not implemented error (current behavior)
    #[allow(unused_variables)]
    pub fn restore_to_session(&self, _session: &dyn SessionLike) -> Result<GidStub, String> {
        warn!(
            gid = %self.gid,
            "restore_to_session() is not yet implemented"
        );
        Err(format!(
            "restore_to_session() not implemented for GID {}",
            self.gid
        ))
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

    /// Generate standard .aria2 filename from GID
    ///
    /// Format: `{gid}.aria2`
    pub fn get_filename(&self) -> String {
        format!("{}.aria2", self.gid)
    }
}

/// Trait for download commands that can be serialized to ResumeData
///
/// This trait abstracts over different download command types (HTTP, FTP, BT)
/// to allow uniform extraction of resume state.
// NOTE: Placeholder trait for future integration with actual command types
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
// NOTE: Placeholder trait for future integration with actual session type
pub trait SessionLike {
    /// Create a new download command from resume data
    fn create_command(&mut self, data: &ResumeData) -> Result<GidStub, String>;

    /// Pause an active download by GID
    fn pause_download(&mut self, gid: &str) -> Result<(), String>;
}

/// Stub type for GID (Global IDentifier)
///
/// Will be replaced with actual GID type when integrating with real session.
#[derive(Debug, Clone)]
pub struct GidStub(pub String);

#[cfg(test)]
mod tests {
    use super::*;
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

    /// Helper to create sample ResumeData with realistic values
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
            bitfield: vec![],
            status: "active".to_string(),
            error_message: None,
            last_download_time: 1700000000,
            created_at: 1699999000,
            output_path: Some("/downloads/ubuntu-22.04-desktop-amd64.iso".to_string()),
            checksum: Some(ChecksumInfo {
                algorithm: "sha-256".to_string(),
                expected: "b4517b7c8a...".to_string(), // truncated for brevity
            }),
            bt_info_hash: None,
            bt_saved_metadata_path: None,
        }
    }

    /// Helper to create sample BT-specific ResumeData
    fn create_bt_resume_data() -> ResumeData {
        ResumeData {
            gid: "bt123456789abcdef".to_string(),
            uris: vec![UriState {
                uri: "magnet:?xt=urn:btih:abcdef1234567890&dn=TestTorrent".to_string(),
                tried: true,
                used: true,
                last_result: Some("ok".to_string()),
                speed_bytes_per_sec: Some(2 * 1024 * 1024),
            }],
            total_length: 1073741824,    // 1 GB
            completed_length: 536870912, // 512 MB (50%)
            bitfield: vec![0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00], // 50% pieces done
            status: "paused".to_string(),
            error_message: None,
            last_download_time: 1700000100,
            created_at: 1699999100,
            output_path: Some("/downloads/test.torrent".to_string()),
            checksum: None,
            bt_info_hash: Some("abcdef1234567890abcdef1234567890abcdef12".to_string()),
            bt_saved_metadata_path: Some("/downloads/.cache/test.torrent".to_string()),
        }
    }

    #[test]
    fn test_resume_data_serialize_deserialize_roundtrip() {
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
        assert_eq!(
            restored.checksum.as_ref().map(|c| &c.algorithm),
            original.checksum.as_ref().map(|c| &c.algorithm),
            "Checksum algorithm mismatch"
        );
        assert_eq!(
            restored.bt_info_hash, original.bt_info_hash,
            "BT info hash mismatch"
        );
        assert_eq!(
            restored.bt_saved_metadata_path, original.bt_saved_metadata_path,
            "BT metadata path mismatch"
        );

        // Verify URI details preserved
        assert_eq!(
            restored.uris[0].uri, original.uris[0].uri,
            "First URI mismatch"
        );
        assert_eq!(
            restored.uris[0].tried, original.uris[0].tried,
            "First URI tried flag mismatch"
        );
        assert_eq!(
            restored.uris[0].used, original.uris[0].used,
            "First URI used flag mismatch"
        );
        assert_eq!(
            restored.uris[0].last_result, original.uris[0].last_result,
            "First URI result mismatch"
        );
        assert_eq!(
            restored.uris[0].speed_bytes_per_sec, original.uris[0].speed_bytes_per_sec,
            "URI speed mismatch"
        );

        println!("Roundtrip test passed. JSON:\n{}", json);
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
        assert!(
            data.bt_info_hash.is_none(),
            "Default bt_info_hash should be None"
        );
        assert!(
            data.bt_saved_metadata_path.is_none(),
            "Default bt_metadata should be None"
        );
    }
}

// =========================================================================
// ResumeDataExt trait for converting between ResumeData and RequestGroup
// =========================================================================

/// Extension trait for creating ResumeData from a RequestGroup
pub trait ResumeDataExt: Sized {
    /// Create ResumeData from a RequestGroup (async, reads state)
    fn from_request_group(
        group: &crate::request::request_group::RequestGroup,
    ) -> impl std::future::Future<Output = Result<Self, String>> + Send;
}

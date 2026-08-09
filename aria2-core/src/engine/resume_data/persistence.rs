//! ResumeData persistence methods
//!
//! Contains all inherent `impl ResumeData` methods for serialization,
//! file I/O, protocol detection, validation, and private helpers.

use super::types::ResumeData;
use std::fs;
use std::path::Path;
use tracing::{debug, info, warn};

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
// Private helper methods
// =========================================================================

impl ResumeData {
    /// Extract info hash from a magnet link
    ///
    /// Parses magnet URI format: `magnet:?xt=urn:btih:<hash>&dn=...`
    /// and returns the hex-encoded info hash if present.
    pub(crate) fn extract_info_hash_from_magnet(magnet_uri: &str) -> Option<String> {
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

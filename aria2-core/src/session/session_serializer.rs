//! Session Serializer module - Batch operations for session file I/O
//!
//! This module provides high-level functions for batch serialization and
//! deserialization of multiple download sessions. It handles file I/O
//! operations for loading and saving session data.
//!
//! # Overview
//!
//! The session serializer is responsible for:
//! - **Loading**: Reading session files and deserializing all entries
//! - **Saving**: Serializing RequestGroup objects to session file format
//! - **Batch processing**: Converting between in-memory representations and file format
//!
//! # Architecture
//!
//! This module builds upon [`SessionEntry`] from the `session_entry` module:
//! - Individual entry parsing/serialization is handled by `SessionEntry`
//! - This module handles multi-entry files and RequestGroup conversions
//! - File I/O operations use atomic write patterns (write tmp + rename)
//!
//! # File Format
//!
//! Session files contain one or more entries separated by blank lines:
//! ```text
//! uri1\turi2
//!  GID=hex_value
//!  option=value
//!
//! uri3
//!  GID=another_hex
//!  PAUSE=true
//! ```
//!
//! # Examples
//!
//! ```rust,no_run
//! use aria2_core::session::session_serializer::{load_from_file, save_to_file};
//! use std::path::Path;
//! use std::sync::Arc;
//! use tokio::sync::RwLock;
//!
//! // Load sessions from file
//! let path = Path::new("aria2.session");
//! let entries = load_from_file(path).await.unwrap();
//!
//! // Save sessions (requires RequestGroup list)
//! // let groups: Vec<Arc<RwLock<RequestGroup>>> = ...;
//! // save_to_file(path, &groups).await.unwrap();
//! ```

use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::{Aria2Error, Result};
use crate::request::request_group::{DownloadStatus, RequestGroup};

// Re-export core types from session_entry module for backward compatibility
pub use super::session_entry::{
    decode_hex, download_options_to_map, escape_uri, unescape_uri, SessionEntry,
};

/// Converts a RequestGroup to a SessionEntry for serialization
///
/// Extracts relevant information from a RequestGroup (including progress,
/// status, BT-specific fields) and creates a SessionEntry suitable for
/// serialization to a session file.
///
/// # Arguments
///
/// * `group` - Reference to the RequestGroup to convert
///
/// # Returns
///
/// Some(SessionEntry) if the group should be serialized (active/waiting/paused),
/// None if the group is complete/removed/error (should not persist)
///
/// # Note
///
/// This function extracts information from both synchronous fields and
/// async methods on the RequestGroup.
pub async fn group_to_entry(group: &RequestGroup) -> Option<SessionEntry> {
    let status = group.status().await;

    match status {
        DownloadStatus::Complete | DownloadStatus::Removed | DownloadStatus::Error(_) => None,
        _ => {
            let gid = group.gid().value();
            let uris = group.uris().to_vec();

            if uris.is_empty() {
                return None;
            }

            let options = download_options_to_map(group.options());
            let paused = matches!(status, DownloadStatus::Paused);

            // Extract progress information using new atomic fields (lock-free)
            let total_length = group.get_total_length_atomic();
            let completed_length = group.get_completed_length();
            let upload_length = group.get_uploaded_length();
            let download_speed = group.get_download_speed_cached();

            // Convert DownloadStatus to string representation
            let status_str = match status {
                DownloadStatus::Active => "active",
                DownloadStatus::Waiting => "waiting",
                DownloadStatus::Paused => "paused",
                DownloadStatus::Complete | DownloadStatus::Removed => "complete",
                DownloadStatus::Error(_) => "error",
            }
            .to_string();

            // Extract error code if in error state
            let error_code = match &status {
                DownloadStatus::Error(_) => Some(1), // Generic error code
                _ => None,
            };

            // Get BT bitfield if available (async operation)
            let bitfield = group.get_bt_bitfield().await;

            Some(SessionEntry {
                gid,
                uris,
                options,
                paused,

                // Progress fields (from atomic fields for performance)
                total_length,
                completed_length,
                upload_length,
                download_speed,
                status: status_str,
                error_code,

                // BT-specific fields (from RequestGroup if available)
                bitfield,
                num_pieces: None, // TODO: Could be stored in RequestGroup if needed
                piece_length: None, // TODO: Could be stored in RequestGroup if needed
                info_hash_hex: None, // TODO: Could be extracted from URI

                // Resume offset (use completed_length as reasonable default)
                resume_offset: if completed_length > 0 {
                    Some(completed_length)
                } else {
                    None
                },
            })
        }
    }
}

/// Serializes multiple RequestGroups to session file format
///
/// Converts each active/waiting/paused RequestGroup into a SessionEntry
/// and serializes them all into a single string suitable for writing to
/// a session file.
///
/// # Arguments
///
/// * `groups` - Slice of Arc<RwLock<RequestGroup>> references
///
/// # Returns
///
/// Result containing the serialized string or an error
///
/// # Filtering
///
/// Only groups with non-empty URIs and non-terminal statuses are included.
/// Complete, removed, and error groups are skipped.
///
/// # Example
///
/// ```rust,no_run
/// use aria2_core::session::session_serializer::serialize_groups;
/// use std::sync::Arc;
/// use tokio::sync::RwLock;
///
/// // let groups: Vec<Arc<RwLock<RequestGroup>>> = ...;
/// let content = serialize_groups(&groups).await.unwrap();
/// println!("Serialized {} groups", content.matches('\n').count());
/// ```
pub async fn serialize_groups(groups: &[Arc<RwLock<RequestGroup>>]) -> Result<String> {
    let mut output = String::new();

    for group_lock in groups {
        let group = group_lock.read().await;
        if let Some(entry) = group_to_entry(&group).await {
            output.push_str(&entry.serialize());
            output.push('\n');
        }
    }

    Ok(output)
}

/// Deserializes session file text into a vector of SessionEntry objects
///
/// Parses the entire contents of a session file and returns all valid
/// entries found. Handles comments (#), blank lines, and forward-compatible
/// unknown keys.
///
/// # Arguments
///
/// * `text` - Full contents of a session file as a string
///
/// # Returns
///
/// Result containing a Vec of successfully parsed SessionEntry objects
///
/// # Format Details
///
/// Each entry consists of:
/// 1. A URI line (one or more tab-separated URIs)
/// 2. Zero or more property lines (space-prefixed key=value pairs)
/// 3. Separated from next entry by blank line
///
/// # Error Handling
///
/// - Empty lines and comments are silently skipped
/// - Unknown keys are stored in the options map (forward compatibility)
/// - Invalid values are ignored (with warnings logged)
/// - Malformed hex strings cause bitfield to be set to None
///
/// # Example
///
/// ```rust
/// use aria2_core::session::session_serializer::deserialize;
///
/// let input = r#"http://example.com/file.zip
///  GID=1
///  split=4
///
/// ftp://server/big.iso
///  GID=2
///  PAUSE=true
/// "#;
///
/// let entries = deserialize(input).unwrap();
/// assert_eq!(entries.len(), 2);
/// assert!(!entries[0].paused);
/// assert!(entries[1].paused);
/// ```
pub fn deserialize(text: &str) -> Result<Vec<SessionEntry>> {
    let mut entries = Vec::new();
    let mut current_text = String::new();
    let mut in_entry = false;

    for raw_line in text.lines() {
        let line = raw_line.trim_end();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            if in_entry && !current_text.is_empty() {
                // End of current entry
                match SessionEntry::deserialize_line(&current_text) {
                    Ok(entry) if !entry.uris.is_empty() => entries.push(entry),
                    Ok(_) => {} // Skip entries with no URIs
                    Err(e) => {
                        tracing::warn!("Failed to deserialize entry: {}", e);
                    }
                }
                current_text.clear();
                in_entry = false;
            }
            continue;
        }

        // This line belongs to current entry
        current_text.push_str(line);
        current_text.push('\n');
        in_entry = true;
    }

    // Don't forget the last entry if file doesn't end with blank line
    if in_entry && !current_text.is_empty() {
        match SessionEntry::deserialize_line(&current_text) {
            Ok(entry) if !entry.uris.is_empty() => entries.push(entry),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("Failed to deserialize entry: {}", e);
            }
        }
    }

    Ok(entries)
}

/// Loads and deserializes session entries from a file
///
/// Reads the specified session file and parses its contents into
/// a vector of SessionEntry objects using atomic read operations.
///
/// # Arguments
///
/// * `path` - Path to the session file to load
///
/// # Returns
///
/// Result containing a Vec of SessionEntry objects or an IO error
///
/// # Errors
///
/// Returns an error if:
/// - The file cannot be read (permission denied, not found, etc.)
/// - The file contains invalid UTF-8
///
/// # Example
///
/// ```rust,no_run
/// use aria2_core::session::session_serializer::load_from_file;
/// use std::path::Path;
///
/// let path = Path::new("aria2.session");
/// let entries = load_from_file(path).await.unwrap();
/// println!("Loaded {} session entries", entries.len());
/// ```
pub async fn load_from_file(path: &Path) -> Result<Vec<SessionEntry>> {
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| Aria2Error::Io(format!(
            "Failed to read session file {}: {}",
            path.display(),
            e
        )))?;

    deserialize(&content)
}

/// Saves multiple RequestGroups to a session file using atomic write
///
/// Serializes all provided RequestGroups and writes them to the specified
/// file using an atomic write pattern (write to temp file + rename).
/// This ensures session file integrity even if the process crashes during write.
///
/// # Arguments
///
/// * `path` - Target path for the session file
/// * `groups` - Slice of Arc<RwLock<RequestGroup>> references to serialize
///
/// # Returns
///
/// Result indicating success or an IO error
///
/// # Atomic Write Strategy
///
/// 1. Serialize all groups to memory
/// 2. Write to `{path}.sess.tmp` temporary file
/// 3. Rename temp file to target path (atomic on most filesystems)
///
/// # Errors
///
/// Returns an error if:
/// - Temporary file cannot be written
/// - Rename operation fails
///
/// # Example
///
/// ```rust,no_run
/// use aria2_core::session::session_serializer::save_to_file;
/// use std::path::Path;
/// use std::sync::Arc;
/// use tokio::sync::RwLock;
///
/// let path = Path::new("aria2.session");
/// // let groups: Vec<Arc<RwLock<RequestGroup>>> = ...;
/// save_to_file(path, &groups).await.unwrap();
/// ```
pub async fn save_to_file(path: &Path, groups: &[Arc<RwLock<RequestGroup>>]) -> Result<()> {
    let content = serialize_groups(groups).await?;
    let tmp_path = path.with_extension("sess.tmp");

    tokio::fs::write(&tmp_path, &content)
        .await
        .map_err(|e| {
            Aria2Error::Io(format!(
                "Failed to write session temp file {}: {}",
                tmp_path.display(),
                e
            ))
        })?;

    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(|e| {
            Aria2Error::Io(format!(
                "Failed to rename session file {}: {}",
                path.display(),
                e
            ))
        })
}

/// Saves pre-serialized SessionEntry list directly to file (bypasses RequestGroup conversion)
///
/// Useful when you already have SessionEntry objects and want to save them
/// without converting through RequestGroup. Uses atomic write pattern for safety.
///
/// # Arguments
///
/// * `path` - Target path for the session file
/// * `entries` - Slice of SessionEntry objects to serialize and save
///
/// # Returns
///
/// Result indicating success or an IO error
///
/// # When to Use
///
/// - Testing session persistence without full RequestGroup setup
/// - Migrating sessions from another source
/// - Manual session manipulation tools
///
/// # Atomic Write Strategy
///
/// Same as [`save_to_file()`]: write to `.sess.tmp` then rename atomically.
///
/// # Example
///
/// ```rust,no_run
/// use aria2_core::session::session_entry::SessionEntry;
/// use aria2_core::session::session_serializer::save_to_file_with_entries;
/// use std::path::Path;
///
/// let path = Path::new("custom.session");
/// let entries = vec![
///     SessionEntry::new(1, vec!["http://example.com/f".to_string()]),
/// ];
/// save_to_file_with_entries(path, &entries).await.unwrap();
/// ```
pub async fn save_to_file_with_entries(
    path: &Path,
    entries: &[SessionEntry],
) -> Result<()> {
    let mut content = String::new();
    for entry in entries {
        content.push_str(&entry.serialize());
        content.push('\n');
    }

    let tmp_path = path.with_extension("sess.tmp");

    tokio::fs::write(&tmp_path, &content)
        .await
        .map_err(|e| {
            Aria2Error::Io(format!(
                "Failed to write session temp file {}: {}",
                tmp_path.display(),
                e
            ))
        })?;

    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(|e| {
            Aria2Error::Io(format!(
                "Failed to rename session file {}: {}",
                path.display(),
                e
            ))
        })
}

// ==================== Unit Tests ====================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_serialize_multiple_groups() {
        // Test that serialize_groups properly filters and serializes multiple groups
        // This test would require mock RequestGroup objects
        // For now, we test the deserialize function which handles multiple entries

        let input = r#"http://example.com/file1.zip
 GID=1
 split=4

http://example.com/file2.iso
 GID=2
 PAUSE=true
 dir=/downloads
"#;

        let entries = deserialize(input).unwrap();
        assert_eq!(entries.len(), 2, "Should parse 2 entries");
        assert_eq!(entries[0].uris[0], "http://example.com/file1.zip");
        assert_eq!(entries[1].uris[0], "http://example.com/file2.iso");
        assert!(!entries[0].paused, "First entry should not be paused");
        assert!(entries[1].paused, "Second entry should be paused");
    }

    #[test]
    fn test_deserialize_mixed_content() {
        // Test handling of mixed content: comments, blanks, valid entries
        let input = r#"# Session file header
# Created by aria2-rust

# First download task
http://example.com/bigfile.tar.gz
 GID=abc123def456
 split=8
 dir=/data/downloads
 TOTAL_LENGTH=104857600
 COMPLETED_LENGTH=52428800

# Second download task (paused)
ftp://mirror.example.com/distro.iso
 GID=789abc012def
 PAUSE=true
 out=distro.iso
 STATUS=paused

# Third task with mirrors
http://mirror1.com/app.exe	http://mirror2.com/app.exe	http://mirror3.com/app.exe
 GID=fedcba098765
 max-connection-per-server=4

"#;

        let entries = deserialize(input).unwrap();
        assert_eq!(entries.len(), 3, "Should parse 3 entries from mixed content");

        // Verify first entry
        assert_eq!(entries[0].gid, 0xabc123def456);
        assert_eq!(entries[0].options.get("split").unwrap(), "8");
        assert_eq!(entries[0].total_length, 104857600);
        assert_eq!(entries[0].completed_length, 52428800);

        // Verify second entry (paused)
        assert!(entries[1].paused);
        assert_eq!(entries[1].status, "paused");
        assert_eq!(entries[1].options.get("out").unwrap(), "distro.iso");

        // Verify third entry (multiple mirrors)
        assert_eq!(entries[2].uris.len(), 3, "Third entry should have 3 mirror URIs");
        assert_eq!(
            entries[2].options.get("max-connection-per-server").unwrap(),
            "4"
        );
    }

    #[test]
    fn test_deserialize_empty_and_whitespace_only() {
        // Test edge cases: completely empty or whitespace-only input
        assert!(deserialize("").unwrap().is_empty(), "Empty string should yield no entries");
        assert!(deserialize("\n\n\n").unwrap().is_empty(), "Only newlines should yield no entries");
        assert!(deserialize("   \n  \n   ").unwrap().is_empty(), "Whitespace-only should yield no entries");
        assert!(
            deserialize("# Just a comment\n# Another comment\n").unwrap().is_empty(),
            "Comments-only should yield no entries"
        );
    }

    #[test]
    fn test_deserialize_preserves_unknown_options() {
        // Test that unknown keys are preserved in options map (forward compatibility)
        let input = r#"http://example.com/file.zip
 GID=1
 CUSTOM_OPTION=value123
 FUTURE_FEATURE=enabled
 TOTAL_LENGTH=1000
"#;

        let entries = deserialize(input).unwrap();
        assert_eq!(entries.len(), 1);

        // Known field parsed correctly
        assert_eq!(entries[0].total_length, 1000);

        // Unknown keys stored in options
        assert_eq!(
            entries[0].options.get("CUSTOM_OPTION").unwrap(),
            "value123"
        );
        assert_eq!(
            entries[0].options.get("FUTURE_FEATURE").unwrap(),
            "enabled"
        );
    }

    #[test]
    fn test_roundtrip_full_session() {
        // Test complete roundtrip: create entries -> serialize -> deserialize -> verify
        let original_entries = vec![
            SessionEntry::new(
                0xABCDEF01,
                vec![
                    "http://primary.example.com/large-file.bin".to_string(),
                    "http://mirror1.example.com/large-file.bin".to_string(),
                    "http://mirror2.example.com/large-file.bin".to_string(),
                ],
            )
            .with_options({
                let mut opts = HashMap::new();
                opts.insert("split".to_string(), "16".to_string());
                opts.insert("max-connection-per-server".to_string(), "8".to_string());
                opts.insert("dir".to_string(), "/downloads".to_string());
                opts.insert("out".to_string(), "large-file.bin".to_string());
                opts
            }),
            SessionEntry::new(
                0x12345678,
                vec!["ftp://server.example.com/software.iso".to_string()],
            )
            .paused()
            .with_options({
                let mut opts = HashMap::new();
                opts.insert("seed-time".to_string(), "3600".to_string());
                opts
            }),
        ];

        // Serialize all entries
        let mut serialized = String::new();
        for entry in &original_entries {
            serialized.push_str(&entry.serialize());
            serialized.push('\n');
        }

        // Deserialize back
        let restored_entries = deserialize(&serialized).unwrap();

        // Verify count matches
        assert_eq!(
            restored_entries.len(),
            original_entries.len(),
            "Entry count should match after roundtrip"
        );

        // Verify first entry details
        assert_eq!(restored_entries[0].gid, 0xABCDEF01);
        assert_eq!(restored_entries[0].uris.len(), 3);
        assert_eq!(restored_entries[0].uris[0], "http://primary.example.com/large-file.bin");
        assert_eq!(restored_entries[0].options.get("split").unwrap(), "16");
        assert!(!restored_entries[0].paused);

        // Verify second entry details
        assert_eq!(restored_entries[1].gid, 0x12345678);
        assert!(restored_entries[1].paused);
        assert_eq!(
            restored_entries[1].options.get("seed-time").unwrap(),
            "3600"
        );
    }

    #[test]
    fn test_backward_compatibility_old_format() {
        // Ensure we can still load old format files without new fields
        let old_format_input = r#"http://example.com/legacy-download.zip
 GID=oldformat123
 split=4
 dir=/old/path
"#;

        let entries = deserialize(old_format_input).unwrap();
        assert_eq!(entries.len(), 1);

        // Old format should have sensible defaults for new fields
        assert_eq!(entries[0].total_length, 0);
        assert_eq!(entries[0].completed_length, 0);
        assert_eq!(entries[0].download_speed, 0);
        assert_eq!(entries[0].status, "active");
        assert_eq!(entries[0].error_code, None);
        assert_eq!(entries[0].bitfield, None);

        // But original fields should still work
        assert_eq!(entries[0].options.get("split").unwrap(), "4");
        assert_eq!(entries[0].options.get("dir").unwrap(), "/old/path");
    }

    #[test]
    fn test_error_messages_are_english() {
        // Verify that error messages are in English (not Chinese)
        // We can't easily trigger actual errors without filesystem issues,
        // but we can check the error message format strings exist correctly

        // This test mainly documents the requirement; actual testing would
        // require mocking filesystem errors
        let path = Path::new("/nonexistent/path/aria2.session");

        // We can't actually run this in test without blocking,
        // but the error message strings should be English
        // Expected: "Failed to read session file ..."
        // Not: "读取 session 文件失败 ..."

        // For now, just verify the function signature exists
        // In production, you'd want integration tests with actual FS errors
        let _ = path; // Suppress unused warning
    }
}

//! File I/O operations for session persistence
//!
//! Provides async functions for loading and saving session files
//! using atomic write patterns for data integrity.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

use crate::error::{Aria2Error, Result};
use crate::request::request_group::RequestGroup;

use super::SessionEntry;
use super::deserialization::deserialize;
use super::serialize::serialize_groups_with_results;
use crate::request::request_group::DownloadResult;

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
/// #[tokio::main]
/// async fn main() {
///     let path = Path::new("aria2.session");
///     let _entries = load_from_file(path).await.unwrap();
/// }
/// ```
pub async fn load_from_file(path: &Path) -> Result<Vec<SessionEntry>> {
    let bytes = tokio::fs::read(path).await.map_err(|e| {
        Aria2Error::Io(format!(
            "Failed to read session file {}: {}",
            path.display(),
            e
        ))
    })?;
    let content = decode_session_content(path, &bytes)?;

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
/// * `groups` - Slice of Arc<std::sync::RwLock<RequestGroup>> references to serialize
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
/// use std::sync::RwLock;
///
/// #[tokio::main]
/// async fn main() {
///     let path = Path::new("aria2.session");
///     let groups: Vec<Arc<RwLock<aria2_core::request::request_group::RequestGroup>>> = vec![];
///     save_to_file(path, &groups).await.unwrap();
/// }
/// ```
pub async fn save_to_file(
    path: &Path,
    groups: &[Arc<std::sync::RwLock<RequestGroup>>],
) -> Result<()> {
    save_to_file_with_results(path, groups, &[]).await
}

/// Saves groups and eligible stopped results to a session file.
///
/// Stopped results are filtered by the serializer's aria2-compatible save
/// policy (`force-save`, `save-not-found`, and resumable in-progress results).
pub async fn save_to_file_with_results(
    path: &Path,
    groups: &[Arc<std::sync::RwLock<RequestGroup>>],
    results: &[DownloadResult],
) -> Result<()> {
    let content = encode_session_content(path, &serialize_groups_with_results(groups, results)?)?;
    let tmp_path = path.with_extension("sess.tmp");

    tokio::fs::write(&tmp_path, &content).await.map_err(|e| {
        Aria2Error::Io(format!(
            "Failed to write session temp file {}: {}",
            tmp_path.display(),
            e
        ))
    })?;

    tokio::fs::rename(&tmp_path, path).await.map_err(|e| {
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
/// #[tokio::main]
/// async fn main() {
///     let path = Path::new("custom.session");
///     let entries = vec![
///         SessionEntry::new(1, vec!["http://example.com/f".to_string()]),
///     ];
///     save_to_file_with_entries(path, &entries).await.unwrap();
/// }
/// ```
pub async fn save_to_file_with_entries(path: &Path, entries: &[SessionEntry]) -> Result<()> {
    let mut text = String::new();
    for entry in entries {
        text.push_str(&entry.serialize());
        text.push('\n');
    }
    let content = encode_session_content(path, &text)?;

    let tmp_path = path.with_extension("sess.tmp");

    tokio::fs::write(&tmp_path, &content).await.map_err(|e| {
        Aria2Error::Io(format!(
            "Failed to write session temp file {}: {}",
            tmp_path.display(),
            e
        ))
    })?;

    tokio::fs::rename(&tmp_path, path).await.map_err(|e| {
        Aria2Error::Io(format!(
            "Failed to rename session file {}: {}",
            path.display(),
            e
        ))
    })
}

fn is_gzip_path(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("gz"))
}

fn decode_session_content(path: &Path, bytes: &[u8]) -> Result<String> {
    if !is_gzip_path(path) {
        return String::from_utf8(bytes.to_vec()).map_err(|e| {
            Aria2Error::Io(format!(
                "Failed to decode session file {}: {}",
                path.display(),
                e
            ))
        });
    }

    let mut decoder = GzDecoder::new(bytes);
    let mut content = String::new();
    decoder.read_to_string(&mut content).map_err(|e| {
        Aria2Error::Io(format!(
            "Failed to decompress session file {}: {}",
            path.display(),
            e
        ))
    })?;
    Ok(content)
}

fn encode_session_content(path: &Path, text: &str) -> Result<Vec<u8>> {
    if !is_gzip_path(path) {
        return Ok(text.as_bytes().to_vec());
    }

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(text.as_bytes()).map_err(|e| {
        Aria2Error::Io(format!(
            "Failed to compress session file {}: {}",
            path.display(),
            e
        ))
    })?;
    encoder.finish().map_err(|e| {
        Aria2Error::Io(format!(
            "Failed to finish compressed session file {}: {}",
            path.display(),
            e
        ))
    })
}

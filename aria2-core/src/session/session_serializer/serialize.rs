//! Serialization logic for converting RequestGroups to session file format

use std::sync::Arc;

use crate::error::Result;
use crate::request::request_group::{DownloadStatus, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

use super::SessionEntry;
use super::download_options_to_map;

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
/// This function extracts information from synchronous fields and methods on the RequestGroup.
pub fn group_to_entry(group: &RequestGroup) -> Option<SessionEntry> {
    let status = group.status();

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

            // Get BT bitfield if available
            let bitfield = group.get_bt_bitfield();

            // Get BT metadata fields (Task 5: session persistence enhancement)
            let num_pieces = group.get_bt_num_pieces();
            let piece_length = group.get_bt_piece_length();
            let info_hash_hex = group.get_bt_info_hash_hex();

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

                // BT-specific fields (from RequestGroup)
                bitfield,
                num_pieces: if num_pieces > 0 {
                    Some(num_pieces)
                } else {
                    None
                },
                piece_length: if piece_length > 0 {
                    Some(piece_length)
                } else {
                    None
                },
                info_hash_hex,

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
/// * `groups` - Slice of Arc<std::sync::RwLock<RequestGroup>> references
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
/// use std::sync::RwLock;
///
/// #[tokio::main]
/// async fn main() {
///     let groups: Vec<Arc<RwLock<aria2_core::request::request_group::RequestGroup>>> = vec![];
///     let _content = serialize_groups(&groups).unwrap();
/// }
/// ```
pub fn serialize_groups(groups: &[Arc<std::sync::RwLock<RequestGroup>>]) -> Result<String> {
    let mut output = String::new();

    for group_lock in groups {
        let group = group_lock.recover();
        if let Some(entry) = group_to_entry(&group) {
            output.push_str(&entry.serialize());
            output.push('\n');
        }
    }

    Ok(output)
}

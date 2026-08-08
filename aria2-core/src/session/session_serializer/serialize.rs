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
    // aria2 does not persist generated child groups. They are recreated by
    // the parent metadata handler after restart, so persisting them would
    // duplicate work and break the parent-child lifecycle.
    if group.belongs_to_gid().is_some() {
        return None;
    }

    let status = group.status();

    match status {
        DownloadStatus::Complete | DownloadStatus::Removed | DownloadStatus::Error(_) => None,
        _ => {
            let mut gid = group.gid().value();
            let mut uris = group.uris().to_vec();

            if uris.is_empty() {
                return None;
            }

            let mut options = download_options_to_map(group.options());

            #[cfg(feature = "bittorrent")]
            let bt_dependency = group.bt_dependency_descriptor();

            // C++ SessionSerializer persists the metadata URI/GID for a
            // generated download, not the synthetic payload URI. Preserve
            // that wire-compatible identity and carry the Rust graph details
            // in namespaced options that older aria2 versions ignore.
            if let Some(info) = graph_metadata_info(
                group.metadata_info(),
                #[cfg(feature = "bittorrent")]
                bt_dependency.is_some(),
            ) && let Some(metadata_gid) = info.gid()
                && !info.uri().is_empty()
            {
                options.insert(
                    "aria2-rust-payload-gid".to_string(),
                    group.gid().to_hex_string(),
                );
                options.insert(
                    "aria2-rust-metadata-uri".to_string(),
                    info.uri().to_string(),
                );
                if let Some(path) = info.metadata_path() {
                    options.insert("aria2-rust-metadata-path".to_string(), path.to_string());
                }
                gid = metadata_gid.value();
                uris = vec![info.uri().to_string()];
            }
            if let Some(output_name) = group.output_name() {
                options.insert("aria2-rust-output-name".to_string(), output_name);
            }

            #[cfg(feature = "bittorrent")]
            if let Some((memory_source, fallback_uris, file_mappings)) = bt_dependency {
                options.insert(
                    "aria2-rust-metadata-memory".to_string(),
                    memory_source.to_string(),
                );
                if let Ok(encoded) = encode_descriptor(&fallback_uris) {
                    options.insert("aria2-rust-fallback-uris".to_string(), encoded);
                }
                if let Ok(encoded) = encode_descriptor(&file_mappings) {
                    options.insert("aria2-rust-file-mappings".to_string(), encoded);
                }
            }

            #[cfg(feature = "bittorrent")]
            if let Some(data) = group.bt_metadata_data()
                && let Ok(encoded) = encode_descriptor(&data)
            {
                options.insert("aria2-rust-bt-metadata-data".to_string(), encoded);
            }
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

fn graph_metadata_info(
    info: Option<crate::request::request_group::MetadataInfo>,
    #[cfg(feature = "bittorrent")] has_bt_dependency: bool,
) -> Option<crate::request::request_group::MetadataInfo> {
    #[cfg(feature = "bittorrent")]
    if !has_bt_dependency {
        return None;
    }

    info
}

#[cfg(feature = "bittorrent")]
fn encode_descriptor<T: serde::Serialize>(value: &T) -> std::result::Result<String, String> {
    use base64::Engine;

    let json = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    Ok(base64::engine::general_purpose::STANDARD.encode(json))
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
/// Complete, removed, error, and generated child groups are skipped. Parent
/// groups remain eligible so their persisted GID can be used to recreate the
/// metadata workflow after restart.
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

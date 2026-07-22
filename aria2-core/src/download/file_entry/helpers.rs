//! Free functions for file entry management.

use std::sync::Arc;

use super::entry::FileEntry;

// ---------------------------------------------------------------------------
// Internal helpers (used across sub-modules)
// ---------------------------------------------------------------------------

/// Validate a URI string by attempting to parse it.
pub(super) fn is_valid_uri(uri: &str) -> bool {
    url::Url::parse(uri).is_ok()
}

/// Extract the hostname from a URI string.
///
/// Returns `None` if the URI cannot be parsed.
pub(super) fn extract_host(uri: &str) -> Option<String> {
    extract_host_and_protocol(uri).map(|(h, _)| h)
}

/// Extract both hostname and protocol from a URI string.
///
/// Handles `scheme://host:port/path` format. Returns `None` if the URI
/// cannot be parsed.
pub(super) fn extract_host_and_protocol(uri: &str) -> Option<(String, String)> {
    crate::selector::feedback_uri_selector::extract_host_and_protocol(uri)
}

// ---------------------------------------------------------------------------
// Public free functions
// ---------------------------------------------------------------------------

/// Return the first `FileEntry` in the slice that `is_requested()`.
pub fn get_first_requested_file_entry(entries: &[Arc<FileEntry>]) -> Option<&Arc<FileEntry>> {
    entries.iter().find(|e| e.is_requested())
}

/// Count the number of requested file entries in the slice.
pub fn count_requested_file_entry(entries: &[Arc<FileEntry>]) -> usize {
    entries.iter().filter(|e| e.is_requested()).count()
}

/// Return `true` if at least one requested `FileEntry` has remaining URIs.
pub fn is_uri_supplied_for_requested_file_entry(entries: &[Arc<FileEntry>]) -> bool {
    entries
        .iter()
        .any(|e| e.is_requested() && !e.remaining_uris().is_empty())
}

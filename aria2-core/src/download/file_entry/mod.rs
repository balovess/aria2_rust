//! Per-file tracking object within a multi-source/multi-file download.
//!
//! Equivalent to the C++ aria2 `FileEntry` class. Each `FileEntry` represents
//! one file in a multi-file torrent/metalink download or the single file in a
//! normal download. It tracks:
//!
//! - File metadata (path, length, offset within container)
//! - URI state machine: `remaining_uris` → `spent_uris` → `uri_results`
//! - Request state machine: `request_pool` → `in_flight_requests` → discarded
//! - Connection control (max connections per server)
//!
//! # Thread Safety
//!
//! `FileEntry` is **not** `Sync` — it is meant to be owned by a single
//! download task. If sharing is needed, wrap in `Arc<Mutex<FileEntry>>`.

pub mod entry;
pub mod helpers;
pub mod request_ops;
pub mod tests;
pub mod types;
pub mod uri_ops;

// Re-export public API to preserve the original import paths.
pub use entry::FileEntry;
pub use helpers::{
    count_requested_file_entry, get_first_requested_file_entry,
    is_uri_supplied_for_requested_file_entry,
};
pub use types::UriResult;

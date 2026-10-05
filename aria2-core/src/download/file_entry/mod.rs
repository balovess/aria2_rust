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
//! The URI lifecycle queues use a narrow per-entry lock so a live request
//! group can update them while protocol sessions hold a shared `DownloadContext`.
//! Request-pool and file metadata operations still require exclusive access.

pub mod entry;
pub mod helpers;
pub mod request_ops;
pub mod tests;
pub mod types;
pub mod uri_ops;

// Expose the file-entry interface at its owning module boundary.
pub use entry::FileEntry;
pub use helpers::{
    count_requested_file_entry, get_first_requested_file_entry,
    is_uri_supplied_for_requested_file_entry,
};
pub use types::UriResult;

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
//!
//! ```text
//! uri1    uri2
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
//!
//! #[tokio::main]
//! async fn main() {
//!     let path = Path::new("aria2.session");
//!     let _entries = load_from_file(path).await.unwrap();
//! }
//! ```

// Re-export core types from session_entry module
pub use super::session_entry::{
    SessionEntry, decode_hex, download_options_to_map, escape_uri, unescape_uri,
};

mod deserialization;
mod file_io;
mod serialize;
#[cfg(test)]
mod tests;

// Re-export all public functions from sub-modules
pub use deserialization::deserialize;
pub use file_io::{
    load_from_file, save_to_file, save_to_file_with_entries, save_to_file_with_results,
};
pub(crate) use serialize::should_save_download_result;
pub use serialize::{
    download_result_to_entry, group_to_entry, serialize_groups, serialize_groups_with_results,
};

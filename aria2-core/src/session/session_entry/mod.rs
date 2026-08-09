//! Session Entry module - Core data structure for session serialization
//!
//! This module provides the `SessionEntry` struct which represents a single download
//! task's state that can be serialized to and deserialized from session files.
//!
//! # Overview
//!
//! A `SessionEntry` captures all necessary information about an active or paused download:
//! - **URIs**: One or more source URLs (mirrors) for the download
//! - **GID**: Unique identifier for the download task
//! - **Options**: Download configuration options (split, dir, out, etc.)
//! - **Progress**: Current download/upload statistics
//! - **Status**: Active state of the download (active, waiting, paused, error)
//! - **BT-specific**: Bitfield and piece information for BitTorrent downloads
//!
//! # Architecture
//!
//! ```text
//! session_entry/mod.rs (this file)
//!   ├── Re-exports for convenience
//!   └── Sub-module declarations
//!
//! session_entry/struct_def.rs
//!   ├── SessionEntry struct definition
//!   └── Builder pattern methods (new, with_options, paused)
//!
//! session_entry/uri_utils.rs
//!   └── escape_uri(), unescape_uri(), decode_hex()
//!
//! session_entry/options.rs
//!   └── download_options_to_map()
//!
//! session_entry/tests.rs
//!   └── Unit tests
//! ```

mod options;
mod struct_def;
#[cfg(test)]
mod tests;
mod uri_utils;

pub use options::download_options_to_map;
pub use struct_def::SessionEntry;
pub use uri_utils::{decode_hex, escape_uri, unescape_uri};

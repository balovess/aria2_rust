//! DownloadContext — split into sub-modules for maintainability.
//!
//! - `struct_def`  — struct definition, Debug impl, constructors, Default impl
//! - `file_ops`    — file entry accessors, lookup, filtering, path management
//! - `hash_ops`    — piece hash and whole-file checksum operations
//! - `context_ops` — attributes, timing, Metalink, BT info hash, signature, owner, network stats

mod context_ops;
mod file_ops;
mod hash_ops;
mod struct_def;

// Re-export all public items so external code sees the same API.
pub use struct_def::DownloadContext;

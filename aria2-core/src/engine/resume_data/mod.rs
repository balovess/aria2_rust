//! Resume Data (.aria2) Serialization System
//!
//! Provides complete download state persistence using JSON format for cross-restart
//! resumption. This module handles serialization/deserialization of download state
//! including progress, URIs, status, timing, and protocol-specific information.
//!
//! # Architecture
//!
//! ```text
//! resume_data/
//!   ├── mod.rs         - Module declarations and re-exports
//!   ├── types.rs       - ResumeData, UriState, ChecksumInfo, RestoreState, MirrorRestoreInfo
//!   ├── persistence.rs - ResumeData inherent impl (serialize, deserialize, save, load, etc.)
//!   ├── ext_trait.rs   - ResumeDataExt trait definition
//!   ├── ext_impl.rs    - ResumeDataExt impl for ResumeData
//!   └── tests.rs       - Unit and integration tests
//!
//! Integration:
//!   - Works alongside existing BtProgressManager (BT-specific text format)
//!   - Uses JSON for human-readable, debuggable output
//!   - Supports both HTTP/FTP and BitTorrent downloads
//! ```

mod ext_impl;
mod ext_trait;
mod persistence;
#[cfg(test)]
mod tests;
mod types;

// Public re-exports — preserve the same API as the original single-file module
pub use ext_trait::ResumeDataExt;
pub use types::{ChecksumInfo, MirrorRestoreInfo, RestoreState, ResumeData, UriState};

//! ResumeDataExt trait definition
//!
//! Provides the extension trait for bidirectional conversion between
//! ResumeData and RequestGroup.

use super::types::RestoreState;
use crate::request::request_group::RequestGroup;
use std::collections::HashMap;

/// Extension trait for converting between ResumeData and RequestGroup
///
/// Provides bidirectional conversion:
/// - `from_request_group()`: Extract complete state from a live RequestGroup
/// - `to_request_group()`: Reconstruct a RequestGroup from persisted data
pub trait ResumeDataExt: Sized {
    /// Create ResumeData from a RequestGroup (async, reads state)
    ///
    /// Extracts all persistable state from the RequestGroup including:
    /// - Identity: GID as hex string
    /// - URIs: Full list with initial state tracking
    /// - Progress: total/completed/uploaded lengths, speeds
    /// - Status: Current lifecycle status as string
    /// - Timing: Creation and last activity timestamps
    /// - File info: Output path from options
    /// - Checksum: Algorithm and expected value if configured
    /// - Options: Relevant subset for restoration
    /// - BT-specific: bitfield, info_hash, metadata path
    /// - HTTP-specific: resume offset for range requests
    ///
    /// # Arguments
    ///
    /// * `group` - Reference to the RequestGroup to extract state from
    ///
    /// # Returns
    ///
    /// * `Ok(ResumeData)` - Fully populated resume data
    /// * `Err(String)` - Extraction error with context
    fn from_request_group(
        group: &RequestGroup,
    ) -> Result<Self, String>;

    /// Convert ResumeData back to restorable state components
    ///
    /// Deconstructs the ResumeData into components needed to reconstruct
    /// a RequestGroup for session restoration.
    ///
    /// # Returns
    ///
    /// Tuple of (gid_hex, uris, options_map, restore_state) where
    /// restore_state contains protocol-specific recovery data.
    fn to_restore_components(
        &self,
    ) -> (
        String,                  // gid_hex
        Vec<String>,             // uris
        HashMap<String, String>, // options
        RestoreState,            // protocol-specific state
    );
}

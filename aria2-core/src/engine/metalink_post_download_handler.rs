//! Metalink Post-Download Handler
//!
//! After a download completes, this handler checks if the downloaded content
//! is actually a Metalink document and, if so, creates additional download
//! commands for each file referenced within it.
//!
//! This implements the "transparent Metalink" feature where a URL that
//! appears to be a regular download is actually a Metalink document.
//! The handler is triggered based on:
//! 1. Content-Type header matching known Metalink MIME types
//! 2. File extension matching (.meta4, .metalink)
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `MetalinkPostDownloadHandler` | `MetalinkPostDownloadHandler` |
//! | `can_handle()` | `PostDownloadHandler::criteria()` |
//! | `get_next_request_groups()` | `getNextRequestGroups()` |

use crate::engine::metalink_download_command::MetalinkDownloadCommand;
use crate::error::{Aria2Error, Result};
use crate::request::request_group::DownloadOptions;

/// Known Metalink MIME content types.
///
/// Mirrors C++ `DownloadHandlerConstants::getMetalinkContentTypes()`.
const METALINK_CONTENT_TYPES: &[&str] = &[
    "application/metalink4+xml",
    "application/metalink+xml",
    "application/x-metalink",
];

/// Known Metalink file extensions.
///
/// Mirrors C++ `DownloadHandlerConstants::getMetalinkExtensions()`.
const METALINK_EXTENSIONS: &[&str] = &[".meta4", ".metalink", ".metalink3"];

/// Post-download handler for Metalink files.
///
/// After the Metalink XML/JSON file is downloaded, this handler parses
/// it and generates download commands for each file referenced
/// within the Metalink document.
///
/// This handler also prevents infinite loops by clearing the
/// `accept-metalink` flag on generated request groups, mirroring
/// C++ `dctx->setAcceptMetalink(false)`.
pub struct MetalinkPostDownloadHandler {
    /// Whether to pause newly created download groups
    pause_requested: bool,
}

impl MetalinkPostDownloadHandler {
    /// Create a new Metalink post-download handler.
    pub fn new() -> Self {
        Self {
            pause_requested: false,
        }
    }

    /// Set whether newly created groups should be paused.
    pub fn with_pause_requested(mut self, pause: bool) -> Self {
        self.pause_requested = pause;
        self
    }

    /// Check if the given content type or file path indicates a Metalink
    /// document that should be handled by this post-download handler.
    ///
    /// Mirrors C++ `ContentTypeRequestGroupCriteria::matchRequest()`.
    pub fn can_handle(content_type: Option<&str>, file_path: Option<&str>) -> bool {
        // Check Content-Type header
        if let Some(ct) = content_type {
            let ct_lower = ct.to_lowercase();
            // Strip parameters (e.g. "application/metalink4+xml; charset=utf-8")
            let ct_base = ct_lower.split(';').next().unwrap_or("").trim();
            if METALINK_CONTENT_TYPES.contains(&ct_base) {
                return true;
            }
        }

        // Check file extension
        if let Some(path) = file_path {
            let path_lower = path.to_lowercase();
            if METALINK_EXTENSIONS
                .iter()
                .any(|ext| path_lower.ends_with(ext))
            {
                return true;
            }
        }

        false
    }

    /// Parse the downloaded Metalink file and generate download commands
    /// for each file entry.
    ///
    /// Mirrors C++ `MetalinkPostDownloadHandler::getNextRequestGroups()`.
    ///
    /// # Arguments
    /// * `metalink_data` - The raw Metalink XML file content
    /// * `base_uri` - Optional base URI for resolving relative URLs
    /// * `options` - Download options for the generated commands
    ///
    /// # Returns
    /// A vector of `MetalinkDownloadCommand`, one per file in the Metalink.
    pub fn get_next_request_groups(
        &self,
        metalink_data: &[u8],
        base_uri: Option<&str>,
        options: &DownloadOptions,
    ) -> Result<Vec<MetalinkDownloadCommand>> {
        use aria2_protocol::metalink::parser::MetalinkDocument;

        // Quick sanity check: must look like XML
        let trimmed = metalink_data
            .iter()
            .skip_while(|&&b| b.is_ascii_whitespace())
            .take(5)
            .copied()
            .collect::<Vec<u8>>();
        if !trimmed.starts_with(b"<?xml") && !trimmed.starts_with(b"<met") {
            return Err(Aria2Error::Parse(
                "Downloaded content does not appear to be a Metalink document".to_string(),
            ));
        }

        let doc = MetalinkDocument::parse(metalink_data, None).map_err(Aria2Error::Parse)?;

        if doc.files.is_empty() {
            tracing::info!("Metalink document contains no downloadable files");
            return Ok(Vec::new());
        }

        tracing::info!(
            version = %doc.version.as_str(),
            files = doc.files.len(),
            "Metalink post-download handler: creating request groups"
        );

        let file_infos = MetalinkDownloadCommand::create_multi_file(
            metalink_data,
            options,
            base_uri,
            1, // GID start
        )?;

        let commands: Vec<MetalinkDownloadCommand> =
            file_infos.into_iter().map(|fi| fi.command).collect();

        tracing::info!(
            count = commands.len(),
            "Metalink post-download handler: generated download commands"
        );

        Ok(commands)
    }

    /// Return the list of Metalink content types.
    pub fn content_types() -> &'static [&'static str] {
        METALINK_CONTENT_TYPES
    }

    /// Return the list of Metalink file extensions.
    pub fn extensions() -> &'static [&'static str] {
        METALINK_EXTENSIONS
    }
}

impl Default for MetalinkPostDownloadHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_handle_content_type() {
        assert!(MetalinkPostDownloadHandler::can_handle(
            Some("application/metalink4+xml"),
            None
        ));
        assert!(MetalinkPostDownloadHandler::can_handle(
            Some("application/metalink+xml"),
            None
        ));
        assert!(MetalinkPostDownloadHandler::can_handle(
            Some("application/x-metalink"),
            None
        ));
        assert!(!MetalinkPostDownloadHandler::can_handle(
            Some("application/octet-stream"),
            None
        ));
        assert!(!MetalinkPostDownloadHandler::can_handle(None, None));
    }

    #[test]
    fn test_can_handle_file_extension() {
        assert!(MetalinkPostDownloadHandler::can_handle(
            None,
            Some("download.meta4")
        ));
        assert!(MetalinkPostDownloadHandler::can_handle(
            None,
            Some("download.metalink")
        ));
        assert!(MetalinkPostDownloadHandler::can_handle(
            None,
            Some("download.METALINK3")
        )); // case-insensitive
        assert!(!MetalinkPostDownloadHandler::can_handle(
            None,
            Some("download.iso")
        ));
    }

    #[test]
    fn test_can_handle_content_type_with_params() {
        assert!(MetalinkPostDownloadHandler::can_handle(
            Some("application/metalink4+xml; charset=utf-8"),
            None
        ));
    }

    #[test]
    fn test_can_handle_both() {
        // Content-Type match takes priority
        assert!(MetalinkPostDownloadHandler::can_handle(
            Some("application/metalink4+xml"),
            Some("download.iso")
        ));
        // Extension match fallback
        assert!(MetalinkPostDownloadHandler::can_handle(
            Some("application/octet-stream"),
            Some("download.meta4")
        ));
    }
}

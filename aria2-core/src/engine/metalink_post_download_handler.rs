//! Metalink Post-Download Handler
//!
//! After a download completes, this handler checks if the downloaded content
//! is actually a Metalink document and, if so, creates additional download
//! groups for each file referenced within it.
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
//! | `create_child_groups()` | `getNextRequestGroups()` |

use std::sync::Arc;
use tracing::info;

use crate::engine::metalink_download_command::MetalinkDownloadCommand;
use crate::engine::post_download_handler::{CompletedDownloadInfo, PostDownloadHandler};
use crate::error::{Aria2Error, Result};
use crate::request::request_group::{DownloadOptions, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

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
/// it and generates child request groups for each file referenced
/// within the Metalink document.
///
/// This handler also prevents infinite loops by clearing the
/// `follow_metalink` flag on generated request groups, mirroring
/// C++ `dctx->setAcceptMetalink(false)`.
#[derive(Debug)]
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
    pub fn can_handle_static(content_type: Option<&str>, file_path: Option<&str>) -> bool {
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

    /// Read the Metalink data from the completed download.
    ///
    /// C++: `diskAdaptor->openExistingFile()` then `util::toString(diskAdaptor)`.
    fn read_metalink_data(info: &CompletedDownloadInfo) -> Result<Vec<u8>, String> {
        if info.in_memory_download {
            info.in_memory_data
                .clone()
                .ok_or_else(|| "In-memory download has no data".to_string())
        } else {
            let path = info
                .file_path
                .as_ref()
                .ok_or_else(|| "File-based Metalink download has no file path".to_string())?;

            std::fs::read(path).map_err(|e| {
                format!("Failed to read Metalink file '{}': {}", path, e)
            })
        }
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

impl PostDownloadHandler for MetalinkPostDownloadHandler {
    fn can_handle(&self, info: &CompletedDownloadInfo) -> bool {
        Self::can_handle_static(
            info.content_type.as_deref(),
            info.file_path.as_deref(),
        )
    }

    fn create_child_groups(
        &self,
        info: &CompletedDownloadInfo,
    ) -> Result<Vec<Arc<std::sync::RwLock<RequestGroup>>>, String> {
        // Read the Metalink data from the completed download
        let metalink_data = Self::read_metalink_data(info)?;

        if metalink_data.is_empty() {
            return Err("Metalink file is empty".to_string());
        }

        // Parse and create MetalinkDownloadCommands
        let mut child_options = (*info.options).clone();

        // Prevent infinite loops: child groups don't re-trigger
        // the Metalink handler. C++: dctx->setAcceptMetalink(false)
        child_options.follow_metalink = Some(false);

        let commands = self
            .get_next_request_groups(
                &metalink_data,
                info.base_uri.as_deref(),
                &child_options,
            )
            .map_err(|e| format!("Metalink processing failed: {}", e))?;

        // Extract RequestGroup from each MetalinkDownloadCommand.
        // C++: `groups.insert(groups.end(), newRgs.begin(), newRgs.end())`
        let mut child_groups = Vec::with_capacity(commands.len());
        for cmd in commands {
            let group = cmd.into_group();

            // If pause requested (PREF_PAUSE_METADATA), mark the child group.
            // C++: `rg->setPauseRequested(true)` when keepRunning && pause_metadata
            if self.pause_requested {
                group.recover().control_flags.request_pause();
            }

            child_groups.push(group);
        }

        info!(
            parent_gid = info.gid.value(),
            children = child_groups.len(),
            "MetalinkPostDownloadHandler: created child request groups"
        );

        Ok(child_groups)
    }

    fn name(&self) -> &'static str {
        "MetalinkPostDownloadHandler"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_handle_content_type() {
        assert!(MetalinkPostDownloadHandler::can_handle_static(
            Some("application/metalink4+xml"),
            None
        ));
        assert!(MetalinkPostDownloadHandler::can_handle_static(
            Some("application/metalink+xml"),
            None
        ));
        assert!(MetalinkPostDownloadHandler::can_handle_static(
            Some("application/x-metalink"),
            None
        ));
        assert!(!MetalinkPostDownloadHandler::can_handle_static(
            Some("application/octet-stream"),
            None
        ));
        assert!(!MetalinkPostDownloadHandler::can_handle_static(None, None));
    }

    #[test]
    fn test_can_handle_file_extension() {
        assert!(MetalinkPostDownloadHandler::can_handle_static(
            None,
            Some("download.meta4")
        ));
        assert!(MetalinkPostDownloadHandler::can_handle_static(
            None,
            Some("download.metalink")
        ));
        assert!(MetalinkPostDownloadHandler::can_handle_static(
            None,
            Some("download.METALINK3")
        )); // case-insensitive
        assert!(!MetalinkPostDownloadHandler::can_handle_static(
            None,
            Some("download.iso")
        ));
    }

    #[test]
    fn test_can_handle_content_type_with_params() {
        assert!(MetalinkPostDownloadHandler::can_handle_static(
            Some("application/metalink4+xml; charset=utf-8"),
            None
        ));
    }

    #[test]
    fn test_can_handle_both() {
        // Content-Type match takes priority
        assert!(MetalinkPostDownloadHandler::can_handle_static(
            Some("application/metalink4+xml"),
            Some("download.iso")
        ));
        // Extension match fallback
        assert!(MetalinkPostDownloadHandler::can_handle_static(
            Some("application/octet-stream"),
            Some("download.meta4")
        ));
    }
}

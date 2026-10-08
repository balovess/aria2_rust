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

use crate::engine::metalink::to_request_group::MetalinkToRequestGroup;
use crate::engine::post_download_handler::{
    CompletedDownloadInfo, PostDownloadHandler, path_has_extension,
};
use crate::error::Aria2Error;
use crate::request::request_group::{FollowMode, GroupId, RequestGroup};
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
    fn can_handle_static(content_type: Option<&str>, file_path: Option<&str>) -> bool {
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

    fn can_handle_with_source_uri(
        content_type: Option<&str>,
        file_path: Option<&str>,
        source_uri: Option<&str>,
    ) -> bool {
        Self::can_handle_static(content_type, file_path)
            || source_uri.is_some_and(|uri| path_has_extension(uri, METALINK_EXTENSIONS))
    }

    /// Read the Metalink data from the completed download.
    ///
    /// C++: `diskAdaptor->openExistingFile()` then `util::toString(diskAdaptor)`.
    fn read_metalink_data(
        info: &CompletedDownloadInfo,
    ) -> std::result::Result<Vec<u8>, Aria2Error> {
        if info.in_memory_download {
            info.in_memory_data
                .clone()
                .ok_or_else(|| Aria2Error::Parse("In-memory download has no data".to_string()))
        } else {
            let path = info.file_path.as_ref().ok_or_else(|| {
                Aria2Error::Io("File-based Metalink download has no file path".to_string())
            })?;

            std::fs::read(path).map_err(|e| {
                Aria2Error::Io(format!("Failed to read Metalink file '{}': {}", path, e))
            })
        }
    }
}

impl Default for MetalinkPostDownloadHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl PostDownloadHandler for MetalinkPostDownloadHandler {
    fn can_handle(&self, info: &CompletedDownloadInfo) -> bool {
        Self::can_handle_with_source_uri(
            info.content_type.as_deref(),
            info.file_path.as_deref(),
            info.base_uri.as_deref(),
        )
    }

    fn create_child_groups(
        &self,
        info: &CompletedDownloadInfo,
    ) -> std::result::Result<Vec<Arc<std::sync::RwLock<RequestGroup>>>, Aria2Error> {
        let mut next_gid = 1;
        let mut allocate_gid = || {
            let gid = GroupId::new(next_gid);
            next_gid = next_gid.saturating_add(1);
            gid
        };
        self.create_child_groups_with_allocator(info, &mut allocate_gid)
    }

    fn create_child_groups_with_allocator(
        &self,
        info: &CompletedDownloadInfo,
        allocate_gid: &mut dyn FnMut() -> GroupId,
    ) -> std::result::Result<Vec<Arc<std::sync::RwLock<RequestGroup>>>, Aria2Error> {
        // Read the Metalink data from the completed download
        let metalink_data = Self::read_metalink_data(info)?;

        if metalink_data.is_empty() {
            return Err(Aria2Error::Parse("Metalink file is empty".to_string()));
        }

        // Parse and create MetalinkDownloadCommands
        let mut child_options = (*info.options).clone();

        // Prevent infinite loops: child groups don't re-trigger
        // the Metalink handler. C++: dctx->setAcceptMetalink(false)
        child_options.follow_metalink = Some(FollowMode::Disabled);

        // Manager-owned resource groups preserve the parsed file index and
        // base URI for command construction. Torrent-only entries must use a
        // metadata/payload graph; flattening them into a payload with no URI
        // loses the torrent prerequisite and can never be spawned.
        let mut converter = MetalinkToRequestGroup::new();
        if let Some(base_uri) = info.base_uri.as_deref() {
            converter = converter.with_base_uri(base_uri);
        }
        let mut gids = std::iter::from_fn(|| Some(allocate_gid()));
        let expansion =
            converter.create_groups_from_bytes(&metalink_data, &child_options, &mut gids)?;
        #[cfg(feature = "bittorrent")]
        let mut child_groups = expansion.resource_groups;
        #[cfg(not(feature = "bittorrent"))]
        let child_groups = expansion.resource_groups;

        #[cfg(feature = "bittorrent")]
        for graph in expansion.torrent_graphs {
            child_groups.push(graph.metadata);
            child_groups.push(graph.payload);
        }

        for group in &child_groups {
            // If pause requested (PREF_PAUSE_METADATA), mark the child group.
            // C++: `rg->setPauseRequested(true)` when keepRunning && pause_metadata
            if self.pause_requested {
                group.recover().request_pause();
            }
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
    #[cfg(feature = "bittorrent")]
    use crate::request::request_group::DownloadOptions;

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

    #[test]
    fn test_can_handle_source_uri_extension() {
        assert!(MetalinkPostDownloadHandler::can_handle_with_source_uri(
            Some("application/octet-stream"),
            None,
            Some("https://example.test/source.meta4?download=1"),
        ));
        assert!(!MetalinkPostDownloadHandler::can_handle_with_source_uri(
            Some("application/octet-stream"),
            None,
            Some("https://example.test/source.xml"),
        ));
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn torrent_only_post_download_preserves_metadata_payload_graph() {
        let path = std::env::temp_dir().join(format!(
            "aria2-rust-metalink-post-{}.meta4",
            std::process::id()
        ));
        std::fs::write(
            &path,
            br#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><metaurl mediatype="application/x-bittorrent">https://example.test/payload.torrent</metaurl></file></metalink>"#,
        )
        .expect("write Metalink fixture");

        let info = CompletedDownloadInfo {
            content_type: Some("application/metalink4+xml".to_string()),
            file_path: Some(path.to_string_lossy().into_owned()),
            options: Arc::new(DownloadOptions::default()),
            gid: GroupId::new(90),
            in_memory_download: false,
            in_memory_data: None,
            base_uri: Some("https://example.test/releases/index.meta4".to_string()),
        };
        let handler = MetalinkPostDownloadHandler::new();
        let mut next_gid = 100;
        let mut allocate_gid = || {
            let gid = GroupId::new(next_gid);
            next_gid += 1;
            gid
        };
        let groups = handler
            .create_child_groups_with_allocator(&info, &mut allocate_gid)
            .expect("torrent-only Metalink should create a graph");

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].recover().gid(), GroupId::new(100));
        assert_eq!(groups[1].recover().gid(), GroupId::new(101));
        assert_eq!(
            groups[0]
                .recover()
                .uris()
                .iter()
                .map(|uri| uri.as_ref())
                .collect::<Vec<_>>(),
            ["https://example.test/payload.torrent"]
        );
        assert_eq!(
            groups[1]
                .recover()
                .uris()
                .iter()
                .map(|uri| uri.as_ref())
                .collect::<Vec<_>>(),
            ["bt://0000000000000064"]
        );
        assert!(!groups[1].recover().is_dependency_resolved());

        let _ = std::fs::remove_file(path);
    }
}

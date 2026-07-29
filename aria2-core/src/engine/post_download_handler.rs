//! Post-download handler trait and processing chain.
//!
//! Mirrors C++ `PostDownloadHandler` base class and the
//! `RequestGroup::postDownloadProcessing()` flow. When a download completes,
//! the engine checks if the downloaded content matches a known type
//! (BitTorrent metainfo, Metalink document) and, if so, creates child
//! request groups for the actual content.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `PostDownloadHandler` trait | `PostDownloadHandler` base class |
//! | `run_post_download_processing()` | `RequestGroup::postDownloadProcessing()` |
//! | `BtTorrentPostDownloadHandler` | `BtPostDownloadHandler` |
//! | `MetalinkPostDownloadHandler` | `MetalinkPostDownloadHandler` |
//!
//! # Flow
//!
//! 1. Download completes → demotion path detects `Complete` status
//! 2. Before releasing resources, extract content_type + file_path
//! 3. Call `run_post_download_processing()` with handler chain
//! 4. Each handler's `can_handle()` is checked; first match wins
//! 5. Handler's `create_child_groups()` creates child `RequestGroup`s
//! 6. Parent-child relationships are linked via `following_gid`/`followed_by_gids`
//! 7. Child groups are inserted at front of reserved queue

use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::error::Aria2Error;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

/// Metadata extracted from a completed download for handler matching.
///
/// Populated before the download context is released, so handlers can
/// inspect content type and file path to decide if they should process
/// the download.
pub struct CompletedDownloadInfo {
    /// Content-Type header from the HTTP response (if available).
    pub content_type: Option<String>,
    /// Local file path of the downloaded content.
    pub file_path: Option<String>,
    /// The download options from the completed group.
    pub options: Arc<DownloadOptions>,
    /// GID of the completed group.
    pub gid: GroupId,
    /// Whether the group was an in-memory download (e.g. magnet metadata).
    pub in_memory_download: bool,
    /// Raw file bytes (for in-memory downloads like magnet metadata).
    /// `None` for file-based downloads where the handler reads from disk.
    pub in_memory_data: Option<Vec<u8>>,
    /// Base URI from the first file entry's spent URIs (for Metalink).
    pub base_uri: Option<String>,
}

/// Trait for post-download handlers that create child request groups.
///
/// Mirrors C++ `PostDownloadHandler` with `canHandle()` / `criteria()`
/// and `getNextRequestGroups()`. Each handler checks if it can process
/// the completed download based on content type / file extension, and
/// if so, creates child `RequestGroup` instances.
pub trait PostDownloadHandler: Send + Sync + std::fmt::Debug {
    /// Check if this handler can process the completed download.
    ///
    /// Mirrors C++ `ContentTypeRequestGroupCriteria::matchRequest()`.
    /// The first handler whose `can_handle()` returns `true` wins.
    fn can_handle(&self, info: &CompletedDownloadInfo) -> bool;

    /// Create child request groups from the completed download.
    ///
    /// Mirrors C++ `PostDownloadHandler::getNextRequestGroups()`.
    /// The returned groups should have their `following_gid` set to
    /// the parent's GID. The caller is responsible for setting
    /// `followed_by_gids` on the parent.
    fn create_child_groups(
        &self,
        info: &CompletedDownloadInfo,
    ) -> std::result::Result<Vec<Arc<std::sync::RwLock<RequestGroup>>>, Aria2Error>;

    /// Handler name for logging.
    fn name(&self) -> &'static str;
}

/// Run the post-download processing chain on a completed download.
///
/// Mirrors C++ `RequestGroup::postDownloadProcessing()`. Iterates the
/// registered handlers and calls the first one that matches. If a handler
/// creates child groups, they are linked to the parent via
/// `following_gid`/`followed_by_gids` and returned so the caller can
/// insert them into the reserved queue.
///
/// # Arguments
/// * `info` - Metadata about the completed download
/// * `handlers` - Ordered list of post-download handlers to try
///
/// # Returns
/// A vector of child `RequestGroup`s wrapped in `Arc<RwLock>` ready for
/// insertion into the reserved queue. Empty if no handler matched.
pub fn run_post_download_processing(
    info: &CompletedDownloadInfo,
    handlers: &[&dyn PostDownloadHandler],
) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
    debug!(
        gid = info.gid.value(),
        content_type = ?info.content_type,
        file_path = ?info.file_path,
        "Running post-download processing chain"
    );

    for handler in handlers {
        if handler.can_handle(info) {
            info!(
                handler = handler.name(),
                gid = info.gid.value(),
                "Post-download handler matched"
            );

            match handler.create_child_groups(info) {
                Ok(groups) => {
                    if groups.is_empty() {
                        debug!(
                            handler = handler.name(),
                            "Handler matched but created no child groups"
                        );
                        return Vec::new();
                    }

                    // Link parent-child relationships.
                    // C++: `requestGroup->followedBy(newRgs)` and `rg->following(gid)`.
                    let child_gids: Vec<GroupId> = groups
                        .iter()
                        .map(|g| g.recover().gid())
                        .collect();

                    for child in &groups {
                        child.recover().set_following_gid(info.gid);
                    }

                    info!(
                        handler = handler.name(),
                        parent_gid = info.gid.value(),
                        children = child_gids.len(),
                        "Post-download handler created child groups"
                    );

                    // Note: The caller must set followed_by_gids on the parent
                    // group and insert the children into the reserved queue.
                    // This is done by the demotion path which has mutable
                    // access to the parent group.

                    return groups;
                }
                Err(e) => {
                    warn!(
                        handler = handler.name(),
                        error = %e,
                        "Post-download handler failed to create child groups"
                    );
                    // Continue to next handler on error, matching C++ behavior
                    // which catches exceptions in postDownloadProcessing().
                }
            }
        }
    }

    debug!(
        gid = info.gid.value(),
        "No post-download handler matched"
    );
    Vec::new()
}

/// Build the default post-download handler chain.
///
/// Mirrors C++ `RequestGroup::initializePostDownloadHandler()` which
/// checks `PREF_FOLLOW_TORRENT` and `PREF_FOLLOW_METALINK` options.
/// Returns a vector of handler instances ready for use.
///
/// # Arguments
/// * `options` - Download options that control which handlers are enabled
pub fn build_handler_chain(
    options: &DownloadOptions,
) -> Vec<Box<dyn PostDownloadHandler>> {
    #[allow(unused_mut)]
    let mut handlers: Vec<Box<dyn PostDownloadHandler>> = Vec::new();

    // BitTorrent handler: enabled when follow-torrent is true
    // C++: if(option_->getAsBool(PREF_FOLLOW_TORRENT) ||
    //          option_->get(PREF_FOLLOW_TORRENT) == V_MEM)
    if options.follow_torrent.unwrap_or(true) {
        #[cfg(feature = "bittorrent")]
        handlers.push(Box::new(
            super::bt_torrent_post_download_handler::BtTorrentPostDownloadHandler::new(),
        ));
    }

    // Metalink handler: enabled when follow-metalink is true
    // C++: if(option_->getAsBool(PREF_FOLLOW_METALINK) ||
    //          option_->get(PREF_FOLLOW_METALINK) == V_MEM)
    if options.follow_metalink.unwrap_or(true) {
        #[cfg(feature = "metalink")]
        handlers.push(Box::new(
            super::metalink_post_download_handler::MetalinkPostDownloadHandler::new(),
        ));
    }

    handlers
}

/// Convenience function: extract CompletedDownloadInfo from a RequestGroup.
///
/// Reads content-type, file path, and other metadata from the group
/// before the download context is released. This should be called
/// BEFORE `demote_group()` clears the context.
pub fn extract_download_info(group: &RequestGroup) -> CompletedDownloadInfo {
    let gid = group.gid();
    let options = group.options_arc();

    let (content_type, file_path, base_uri, in_memory_download, in_memory_data) =
        if let Some(dctx) = group.download_context.recover().as_ref() {
            let fp = dctx.first_file_path().map(|s| s.to_string());

            // Get base URI from the first file entry's spent URIs.
            // C++: getBaseUri() checks spentUris.back() then remainingUris.front().
            let base_uri = dctx.first_file_entry().and_then(|entry| {
                entry
                    .spent_uris()
                    .back()
                    .cloned()
                    .or_else(|| entry.remaining_uris().front().cloned())
            });

            // In-memory download detection: check if the BT attribute is set
            // and the base_path is empty (magnet metadata download).
            // C++: `requestGroup->inMemoryDownload()` checks a flag set
            // during metadata exchange.
            let in_mem = dctx.first_file_path().is_none()
                || dctx.get_base_path().is_empty();

            let data = None; // In-memory data extraction is deferred to the handler

            (None, fp, base_uri, in_mem, data)
        } else {
            (None, None, None, false, None)
        };

    CompletedDownloadInfo {
        content_type,
        file_path,
        options,
        gid,
        in_memory_download,
        in_memory_data,
        base_uri,
    }
}

use crate::util::rwlock_ext::RwLockRecover;

#[cfg(test)]
mod tests {
    use super::*;

    struct MockHandler {
        name_str: &'static str,
        can_handle_result: bool,
        groups: Vec<Arc<std::sync::RwLock<RequestGroup>>>,
    }

    impl std::fmt::Debug for MockHandler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MockHandler")
                .field("name_str", &self.name_str)
                .field("can_handle_result", &self.can_handle_result)
                .finish()
        }
    }

    impl PostDownloadHandler for MockHandler {
        fn can_handle(&self, _info: &CompletedDownloadInfo) -> bool {
            self.can_handle_result
        }

        fn create_child_groups(
            &self,
            _info: &CompletedDownloadInfo,
        ) -> std::result::Result<Vec<Arc<std::sync::RwLock<RequestGroup>>>, Aria2Error> {
            Ok(self.groups.clone())
        }

        fn name(&self) -> &'static str {
            self.name_str
        }
    }

    fn make_info() -> CompletedDownloadInfo {
        CompletedDownloadInfo {
            content_type: None,
            file_path: None,
            options: Arc::new(DownloadOptions::default()),
            gid: GroupId::new(1),
            in_memory_download: false,
            in_memory_data: None,
            base_uri: None,
        }
    }

    #[test]
    fn test_no_handler_matches() {
        let handlers: Vec<&dyn PostDownloadHandler> = vec![];
        let info = make_info();
        let result = run_post_download_processing(&info, &handlers);
        assert!(result.is_empty());
    }

    #[test]
    fn test_handler_matches_and_creates_groups() {
        let child = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(100),
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )));
        let handler = MockHandler {
            name_str: "mock",
            can_handle_result: true,
            groups: vec![child],
        };
        let info = make_info();
        let handlers: Vec<&dyn PostDownloadHandler> = vec![&handler];
        let result = run_post_download_processing(&info, &handlers);
        assert_eq!(result.len(), 1);
        // Child should have following_gid set to parent's GID
        assert_eq!(result[0].recover().following_gid(), Some(GroupId::new(1)));
    }

    #[test]
    fn test_handler_error_continues() {
        #[derive(Debug)]
        struct FailHandler;
        impl PostDownloadHandler for FailHandler {
            fn can_handle(&self, _info: &CompletedDownloadInfo) -> bool {
                true
            }
            fn create_child_groups(
                &self,
                _info: &CompletedDownloadInfo,
            ) -> std::result::Result<Vec<Arc<std::sync::RwLock<RequestGroup>>>, Aria2Error> {
                Err(Aria2Error::Parse("parse error".to_string()))
            }
            fn name(&self) -> &'static str {
                "fail"
            }
        }

        let fail = FailHandler;
        let info = make_info();
        let handlers: Vec<&dyn PostDownloadHandler> = vec![&fail];
        let result = run_post_download_processing(&info, &handlers);
        assert!(result.is_empty());
    }
}

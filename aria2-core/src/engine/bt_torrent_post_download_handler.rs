//! BitTorrent torrent file post-download handler.
//!
//! When a downloaded file is detected as a BitTorrent metainfo file
//! (based on content type or file extension), this handler parses the
//! torrent and creates child `RequestGroup` instances for the actual
//! BitTorrent download.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `BtTorrentPostDownloadHandler` | `BtPostDownloadHandler` |
//! | `can_handle()` | `ContentTypeRequestGroupCriteria::matchRequest()` |
//! | `create_child_groups()` | `BtPostDownloadHandler::getNextRequestGroups()` |

use std::sync::Arc;
use tracing::{debug, info};

use crate::engine::post_download_handler::{CompletedDownloadInfo, PostDownloadHandler};
use crate::error::Aria2Error;
use crate::request::request_group::{DownloadOptions, FollowMode, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

/// Known BitTorrent content types.
///
/// Mirrors C++ `DownloadHandlerConstants::getBtContentTypes()`.
const BT_CONTENT_TYPES: &[&str] = &["application/x-bittorrent"];

/// Known BitTorrent file extensions.
///
/// Mirrors C++ `DownloadHandlerConstants::getBtExtensions()`.
const BT_EXTENSIONS: &[&str] = &[".torrent"];

/// Post-download handler for BitTorrent metainfo files.
///
/// When a download completes and the content is a .torrent file,
/// this handler parses the bencode metainfo and creates a new
/// `RequestGroup` for the actual BitTorrent download.
///
/// This implements the "transparent torrent follow" feature where
/// a URL that appears to be a regular download is actually a
/// torrent file. The handler is triggered based on:
/// 1. Content-Type header matching `application/x-bittorrent`
/// 2. File extension matching `.torrent`
///
/// This handler also prevents infinite loops by clearing the
/// `follow_torrent` flag on generated request groups, mirroring
/// C++ behavior where child groups don't re-trigger the handler.
#[derive(Debug)]
pub struct BtTorrentPostDownloadHandler {
    /// Whether to pause newly created groups (mirrors C++ PREF_PAUSE_METADATA).
    pause_requested: bool,
}

impl BtTorrentPostDownloadHandler {
    /// Create a new BitTorrent post-download handler.
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

    /// Check if the given content type or file path indicates a BitTorrent
    /// metainfo file.
    ///
    /// Mirrors C++ `ContentTypeRequestGroupCriteria::matchRequest()`.
    pub fn can_handle_static(content_type: Option<&str>, file_path: Option<&str>) -> bool {
        if let Some(ct) = content_type {
            let ct_lower = ct.to_lowercase();
            let ct_base = ct_lower.split(';').next().unwrap_or("").trim();
            if BT_CONTENT_TYPES.contains(&ct_base) {
                return true;
            }
        }

        if let Some(path) = file_path {
            let path_lower = path.to_lowercase();
            if BT_EXTENSIONS.iter().any(|ext| path_lower.ends_with(ext)) {
                return true;
            }
        }

        false
    }

    /// Parse a torrent file and create a request group for it.
    ///
    /// Mirrors C++ `BtPostDownloadHandler::getNextRequestGroups()`:
    /// 1. Read the torrent data (from in-memory or disk)
    /// 2. Parse bencode metainfo via `TorrentMeta::parse()`
    /// 3. Create a new `RequestGroup` with tracker URIs
    /// 4. Set parent-child relationships
    ///
    /// # Arguments
    /// * `torrent_data` - Raw bencoded torrent file content
    /// * `parent_gid` - GID of the parent download (for GID generation)
    /// * `options` - Download options for the new group
    fn create_request_group_from_torrent(
        &self,
        torrent_data: &[u8],
        parent_gid: GroupId,
        options: &DownloadOptions,
    ) -> std::result::Result<Vec<Arc<std::sync::RwLock<RequestGroup>>>, Aria2Error> {
        let child_gid = GroupId::new(parent_gid.value().saturating_mul(1000).saturating_add(1));
        self.create_request_group_from_torrent_with_gid(
            torrent_data,
            parent_gid,
            child_gid,
            options,
        )
    }

    fn create_request_group_from_torrent_with_gid(
        &self,
        torrent_data: &[u8],
        parent_gid: GroupId,
        child_gid: GroupId,
        options: &DownloadOptions,
    ) -> std::result::Result<Vec<Arc<std::sync::RwLock<RequestGroup>>>, Aria2Error> {
        use aria2_protocol::bittorrent::torrent::parser::TorrentMeta;

        // Parse the bencode metainfo.
        // C++: `bittorrent::ValueBaseBencodeParser().parseFinal(content, size, error)`
        let meta = TorrentMeta::parse(torrent_data).map_err(|e| {
            Aria2Error::BittorrentParse(format!("Could not parse BitTorrent metainfo: {}", e))
        })?;

        let info_hash_hex = meta.info_hash.as_hex();

        info!(
            info_hash = %info_hash_hex,
            name = %meta.info.name,
            pieces = meta.info.pieces.len(),
            "BtTorrentPostDownloadHandler: parsed torrent metainfo"
        );

        // Create the download URIs from the tracker list + DHT.
        // C++: `createRequestGroupForBitTorrent(newRgs, option, args, "", torrent.get())`
        let mut uris = Vec::new();

        // Add tracker announce URLs (primary + tiers)
        if !meta.announce.is_empty() {
            uris.push(meta.announce.clone());
        }
        for tier in &meta.announce_list {
            for tracker in tier {
                if !uris.contains(tracker) {
                    uris.push(tracker.clone());
                }
            }
        }

        // Add web seed URLs from url-list (BEP 19)
        for web_seed in &meta.web_seeds {
            if !uris.contains(web_seed) {
                uris.push(web_seed.clone());
            }
        }

        // If no trackers, use magnet URI as fallback
        if uris.is_empty() {
            let magnet = format!("magnet:?xt=urn:btih:{}", info_hash_hex);
            uris.push(magnet);
        }

        // Create a new RequestGroup for the torrent download. The engine path
        // supplies a manager-owned GID; the legacy helper above retains a
        // deterministic fallback for standalone callers.
        let mut child_options = options.clone();

        // Prevent infinite loops: child torrent groups don't re-trigger
        // the BT post-download handler. C++: child groups don't get
        // postDownloadHandlers added.
        child_options.follow_torrent = Some(FollowMode::Disabled);

        let child_group = RequestGroup::new(child_gid, uris, child_options);

        // Set BitTorrent-specific metadata on the child group.
        // This data will be used by BtDownloadCommand when the group
        // is promoted to active.
        {
            child_group.bt_num_pieces.store(
                meta.info.pieces.len() as u32,
                std::sync::atomic::Ordering::Relaxed,
            );
            child_group
                .bt_piece_length
                .store(meta.info.piece_length, std::sync::atomic::Ordering::Relaxed);
            *child_group.bt_info_hash_hex.recover_mut() = Some(info_hash_hex);
        }

        // If pause requested (PREF_PAUSE_METADATA), mark the child group.
        // C++: `rg->setPauseRequested(true)` when keepRunning && pause_metadata
        if self.pause_requested {
            child_group.control_flags.request_pause();
        }

        let child = Arc::new(std::sync::RwLock::new(child_group));

        debug!(
            parent_gid = parent_gid.value(),
            child_gid = child_gid.value(),
            "BtTorrentPostDownloadHandler: created child request group"
        );

        Ok(vec![child])
    }
}

impl Default for BtTorrentPostDownloadHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl PostDownloadHandler for BtTorrentPostDownloadHandler {
    fn can_handle(&self, info: &CompletedDownloadInfo) -> bool {
        Self::can_handle_static(info.content_type.as_deref(), info.file_path.as_deref())
    }

    fn create_child_groups(
        &self,
        info: &CompletedDownloadInfo,
    ) -> std::result::Result<Vec<Arc<std::sync::RwLock<RequestGroup>>>, Aria2Error> {
        // Get torrent data: either from in-memory (magnet metadata) or disk
        let torrent_data = if info.in_memory_download {
            // C++: reads from BencodeDiskWriter for in-memory downloads
            info.in_memory_data
                .as_ref()
                .ok_or_else(|| Aria2Error::Parse("In-memory download has no data".to_string()))?
                .clone()
        } else {
            // C++: `diskAdaptor->openExistingFile()` then `util::toString()`
            let path = info.file_path.as_ref().ok_or_else(|| {
                Aria2Error::Io("File-based torrent download has no file path".to_string())
            })?;

            std::fs::read(path).map_err(|e| {
                Aria2Error::Io(format!("Failed to read torrent file '{}': {}", path, e))
            })?
        };

        if torrent_data.is_empty() {
            return Err(Aria2Error::Parse("Torrent file is empty".to_string()));
        }

        self.create_request_group_from_torrent(&torrent_data, info.gid, &info.options)
    }

    fn create_child_groups_with_allocator(
        &self,
        info: &CompletedDownloadInfo,
        allocate_gid: &mut dyn FnMut() -> GroupId,
    ) -> std::result::Result<Vec<Arc<std::sync::RwLock<RequestGroup>>>, Aria2Error> {
        let torrent_data = if info.in_memory_download {
            info.in_memory_data
                .as_ref()
                .ok_or_else(|| Aria2Error::Parse("In-memory download has no data".to_string()))?
                .clone()
        } else {
            let path = info.file_path.as_ref().ok_or_else(|| {
                Aria2Error::Io("File-based torrent download has no file path".to_string())
            })?;
            std::fs::read(path).map_err(|e| {
                Aria2Error::Io(format!("Failed to read torrent file '{}': {}", path, e))
            })?
        };

        if torrent_data.is_empty() {
            return Err(Aria2Error::Parse("Torrent file is empty".to_string()));
        }

        self.create_request_group_from_torrent_with_gid(
            &torrent_data,
            info.gid,
            allocate_gid(),
            &info.options,
        )
    }

    fn name(&self) -> &'static str {
        "BtTorrentPostDownloadHandler"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_handle_content_type() {
        assert!(BtTorrentPostDownloadHandler::can_handle_static(
            Some("application/x-bittorrent"),
            None
        ));
        assert!(!BtTorrentPostDownloadHandler::can_handle_static(
            Some("application/octet-stream"),
            None
        ));
    }

    #[test]
    fn test_can_handle_file_extension() {
        assert!(BtTorrentPostDownloadHandler::can_handle_static(
            None,
            Some("download.torrent")
        ));
        assert!(BtTorrentPostDownloadHandler::can_handle_static(
            None,
            Some("DOWNLOAD.TORRENT")
        )); // case-insensitive
        assert!(!BtTorrentPostDownloadHandler::can_handle_static(
            None,
            Some("download.iso")
        ));
    }

    #[test]
    fn test_can_handle_no_match() {
        assert!(!BtTorrentPostDownloadHandler::can_handle_static(None, None));
        assert!(!BtTorrentPostDownloadHandler::can_handle_static(
            Some("text/html"),
            Some("index.html")
        ));
    }
}

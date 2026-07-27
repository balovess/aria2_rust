//! Rich download result for RPC consumers.
//!
//! Mirrors C++ `DownloadResult` which carries the full download snapshot
//! for `aria2.tellStatus`, `aria2.tellStopped`, `aria2.getDownloadResult`
//! RPC methods. Contains GID, progress stats, file entries, BT info hash,
//! and relationship GIDs (followedBy / following / belongsTo).

use serde::{Deserialize, Serialize};

use super::GroupId;
use super::result_code::DownloadResultCode;
use super::status::DownloadStatus;

/// File entry within a download result.
///
/// Mirrors C++ `FileData` / `FileEntry` information exposed by RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// File index (1-based, matching C++ convention).
    pub index: usize,
    /// File path relative to download directory.
    pub path: String,
    /// Total file length in bytes.
    pub length: u64,
    /// Completed bytes for this file.
    pub completed_length: u64,
    /// Whether this file is selected for download.
    pub selected: bool,
    /// URIs associated with this file.
    pub uris: Vec<UriEntry>,
}

/// URI entry within a file entry.
///
/// Mirrors C++ `URIResult` exposed by RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UriEntry {
    /// The URI string.
    pub uri: String,
    /// Current status of this URI ("used", "waiting", "spent").
    pub status: String,
}

/// Rich download result for RPC consumers.
///
/// Mirrors C++ `DownloadResult` with all fields needed for
/// `aria2.tellStatus`, `aria2.tellStopped`, and `aria2.getDownloadResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadResult {
    // ── Identity ────────────────────────────────────────────────────────
    /// GID of the download this result refers to.
    pub gid: GroupId,
    /// Download status at the time of result creation.
    pub status: DownloadStatus,
    /// Structured result code.
    pub code: DownloadResultCode,
    /// Human-readable error / status message.
    pub message: String,

    // ── Progress ────────────────────────────────────────────────────────
    /// Total length of the download in bytes.
    pub total_length: u64,
    /// Completed length in bytes.
    pub completed_length: u64,
    /// Total uploaded bytes (BT only, 0 for non-BT).
    pub upload_length: u64,
    /// Download speed in bytes/sec at the time of snapshot.
    pub download_speed: u64,
    /// Upload speed in bytes/sec at the time of snapshot (BT only).
    pub upload_speed: u64,
    /// Total number of pieces.
    pub num_pieces: u32,
    /// Piece length in bytes.
    pub piece_length: u32,
    /// Bitfield representing completed pieces (hex string for RPC).
    /// Empty string if not applicable.
    pub bitfield: String,

    // ── Relationships ──────────────────────────────────────────────────
    /// GIDs of downloads that were spawned by this one
    /// (e.g. Metalink → child downloads, torrent → magnet).
    pub followed_by: Vec<GroupId>,
    /// GID of the parent download that spawned this one.
    pub following: Option<GroupId>,
    /// GID of the download this one belongs to (e.g. BT parent).
    pub belongs_to: Option<GroupId>,

    // ── File info ──────────────────────────────────────────────────────
    /// Download directory.
    pub dir: String,
    /// File entries for multi-file downloads.
    pub files: Vec<FileEntry>,
    /// BT info hash (empty string for non-BT).
    pub info_hash: String,

    // ── Metadata ───────────────────────────────────────────────────────
    /// Download context attributes (e.g. CTX_ATTR_ED2K for aria2-next).
    pub attrs: std::collections::HashMap<String, String>,
    /// Whether this was an in-memory download (metadata exchange only).
    pub in_memory_download: bool,
    /// Session download length (bytes downloaded since session start).
    pub session_download_length: u64,
    /// Session time (seconds since session start).
    pub session_time: u64,
}

impl DownloadResult {
    /// Create a new download result with identity fields and defaults.
    pub fn new(gid: GroupId, status: DownloadStatus, code: DownloadResultCode) -> Self {
        let message = match code {
            DownloadResultCode::Finished => "OK".to_string(),
            DownloadResultCode::Removed => "Download removed by user".to_string(),
            DownloadResultCode::InProgress => "Download interrupted by shutdown".to_string(),
            DownloadResultCode::Paused => "Download paused".to_string(),
            _ => format!("{}", code),
        };

        Self {
            gid,
            status,
            code,
            message,
            total_length: 0,
            completed_length: 0,
            upload_length: 0,
            download_speed: 0,
            upload_speed: 0,
            num_pieces: 0,
            piece_length: 0,
            bitfield: String::new(),
            followed_by: Vec::new(),
            following: None,
            belongs_to: None,
            dir: String::new(),
            files: Vec::new(),
            info_hash: String::new(),
            attrs: std::collections::HashMap::new(),
            in_memory_download: false,
            session_download_length: 0,
            session_time: 0,
        }
    }

    /// Create a successful result (convenience for tests).
    pub fn finished() -> Self {
        Self {
            gid: GroupId(0),
            status: DownloadStatus::Complete,
            code: DownloadResultCode::Finished,
            message: String::from("OK"),
            total_length: 0,
            completed_length: 0,
            upload_length: 0,
            download_speed: 0,
            upload_speed: 0,
            num_pieces: 0,
            piece_length: 0,
            bitfield: String::new(),
            followed_by: Vec::new(),
            following: None,
            belongs_to: None,
            dir: String::new(),
            files: Vec::new(),
            info_hash: String::new(),
            attrs: std::collections::HashMap::new(),
            in_memory_download: false,
            session_download_length: 0,
            session_time: 0,
        }
    }

    /// Create a result for a user-removed download.
    pub fn removed() -> Self {
        Self {
            gid: GroupId(0),
            status: DownloadStatus::Removed,
            code: DownloadResultCode::Removed,
            message: String::from("Download removed by user"),
            ..Self::finished()
        }
    }

    /// Create a result for an interrupted (shutdown) download.
    pub fn in_progress() -> Self {
        Self {
            gid: GroupId(0),
            status: DownloadStatus::Active,
            code: DownloadResultCode::InProgress,
            message: String::from("Download interrupted by shutdown"),
            ..Self::finished()
        }
    }

    /// Create a result for a paused download.
    pub fn paused() -> Self {
        Self {
            gid: GroupId(0),
            status: DownloadStatus::Paused,
            code: DownloadResultCode::Paused,
            message: String::from("Download paused"),
            ..Self::finished()
        }
    }

    /// Create an error result with a specific code and message.
    pub fn error(code: DownloadResultCode, message: impl Into<String>) -> Self {
        Self {
            gid: GroupId(0),
            status: DownloadStatus::Error(String::new()),
            code,
            message: message.into(),
            ..Self::finished()
        }
    }

    /// Get the GID as a hex string (for RPC compatibility).
    pub fn gid_hex(&self) -> String {
        self.gid.to_hex_string()
    }

    /// Fill in progress stats from the given `RequestGroup`.
    ///
    /// Reads `total_length`, `completed_length`, `upload_length`,
    /// `download_speed`, `upload_speed`, `dir`, and `info_hash`
    /// from the group's `AtomicProgress` and options.
    pub fn fill_from_group(&mut self, group: &super::RequestGroup) {
        self.total_length = group.total_length();
        self.completed_length = group.completed_length();
        self.upload_length = group.upload_length();
        self.download_speed = group.download_speed();
        self.upload_speed = group.upload_speed();
        self.dir = group.options().dir.clone().unwrap_or_default();
        self.info_hash = group.info_hash_hex().unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_result_code_roundtrip() {
        for code in [
            DownloadResultCode::Finished,
            DownloadResultCode::TimeOut,
            DownloadResultCode::Removed,
            DownloadResultCode::InProgress,
            DownloadResultCode::ChecksumError,
        ] {
            assert_eq!(DownloadResultCode::from_code(code.as_code()), Some(code));
        }
    }

    #[test]
    fn test_is_success() {
        assert!(DownloadResultCode::Finished.is_success());
        assert!(!DownloadResultCode::TimeOut.is_success());
    }

    #[test]
    fn test_is_resumable() {
        assert!(DownloadResultCode::InProgress.is_resumable());
        assert!(DownloadResultCode::Paused.is_resumable());
        assert!(!DownloadResultCode::TimeOut.is_resumable());
    }

    #[test]
    fn test_is_user_stopped() {
        assert!(DownloadResultCode::Removed.is_user_stopped());
        assert!(DownloadResultCode::Paused.is_user_stopped());
        assert!(!DownloadResultCode::InProgress.is_user_stopped());
    }

    #[test]
    fn test_download_result_finished() {
        let r = DownloadResult::finished();
        assert_eq!(r.code, DownloadResultCode::Finished);
        assert_eq!(r.total_length, 0);
        assert!(r.followed_by.is_empty());
    }

    #[test]
    fn test_download_result_removed() {
        let r = DownloadResult::removed();
        assert_eq!(r.code, DownloadResultCode::Removed);
    }

    #[test]
    fn test_download_result_in_progress() {
        let r = DownloadResult::in_progress();
        assert_eq!(r.code, DownloadResultCode::InProgress);
    }

    #[test]
    fn test_download_result_has_all_rpc_fields() {
        let r = DownloadResult::finished();
        // Verify all fields that RPC consumers expect are present.
        assert_eq!(r.gid_hex(), "0000000000000000");
        assert_eq!(r.total_length, 0);
        assert_eq!(r.completed_length, 0);
        assert_eq!(r.upload_length, 0);
        assert_eq!(r.download_speed, 0);
        assert_eq!(r.upload_speed, 0);
        assert_eq!(r.num_pieces, 0);
        assert_eq!(r.piece_length, 0);
        assert!(r.bitfield.is_empty());
        assert!(r.followed_by.is_empty());
        assert!(r.following.is_none());
        assert!(r.belongs_to.is_none());
        assert!(r.dir.is_empty());
        assert!(r.files.is_empty());
        assert!(r.info_hash.is_empty());
        assert!(r.attrs.is_empty());
        assert!(!r.in_memory_download);
        assert_eq!(r.session_download_length, 0);
        assert_eq!(r.session_time, 0);
    }
}

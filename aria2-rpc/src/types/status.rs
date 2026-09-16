//! Download status and file metadata returned by RPC methods.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::transfer::UriEntry;
use crate::wire;

/// Public download lifecycle state used by RPC DTOs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DownloadStatus {
    #[default]
    Waiting,
    Active,
    Paused,
    Error(String),
    Complete,
    Removed,
}

impl Serialize for DownloadStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Waiting => "waiting",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Error(_) => "error",
            Self::Complete => "complete",
            Self::Removed => "removed",
        })
    }
}

impl<'de> Deserialize<'de> for DownloadStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "waiting" => Ok(Self::Waiting),
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "error" => Ok(Self::Error(String::new())),
            "complete" => Ok(Self::Complete),
            "removed" => Ok(Self::Removed),
            value => Err(serde::de::Error::unknown_variant(
                value,
                &[
                    "waiting", "active", "paused", "error", "complete", "removed",
                ],
            )),
        }
    }
}

impl DownloadStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Error(_) => "error",
            Self::Complete => "complete",
            Self::Removed => "removed",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active | Self::Waiting)
    }

    pub fn is_stopped(&self) -> bool {
        !self.is_active()
    }
}

// =========================================================================
// BitTorrent Metadata Types
// =========================================================================

/// BitTorrent metadata for tellStatus response.
///
/// Matches original aria2 `gatherBitTorrentMetadata` output structure:
/// ```json
/// {
///   "announceList": [["udp://tracker:80"]],
///   "comment": "a comment",
///   "creationDate": 1234567890,
///   "mode": "single",
///   "info": {"name": "filename"}
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BittorrentInfo {
    /// Announce URIs grouped by tier
    pub announce_list: Vec<Vec<String>>,
    /// Torrent comment (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Creation date as Unix timestamp (Integer in original JSON)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<i64>,
    /// Torrent mode: "single" or "multi"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Torrent info containing the name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<BittorrentMetaInfo>,
}

/// Inner info dict of bittorrent metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BittorrentMetaInfo {
    /// Torrent name
    pub name: String,
}

// =========================================================================
// Download Status Types
// =========================================================================

/// Detailed status information for a download task.
///
/// Returned by `aria2.tellStatus`, `aria2.tellActive`, `aria2.tellWaiting`,
/// and `aria2.tellStopped`. Contains both static metadata (GID, directory)
/// and dynamic progress fields (speeds, lengths, connections).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusInfo {
    pub gid: String,
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub total_length: Option<u64>,
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub completed_length: Option<u64>,
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub upload_length: Option<u64>,
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub download_speed: Option<u64>,
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub upload_speed: Option<u64>,
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub connections: Option<u16>,
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub error_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub status: DownloadStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileInfo>>,
    /// BitTorrent metadata (matches original nested `bittorrent` object)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bittorrent: Option<BittorrentInfo>,
    /// Following GID (single string, matches original aria2 behavior)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub following: Option<String>,
    /// Whether this download is seeding (BitTorrent only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seeder: Option<String>,
    /// Hex-encoded piece bitfield (BitTorrent only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitfield: Option<String>,
    /// Piece length in bytes (BitTorrent only)
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub piece_length: Option<u64>,
    /// Number of pieces (BitTorrent only)
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub num_pieces: Option<u32>,
    /// Number of locally verified pieces (BitTorrent only).
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub completed_pieces: Option<u32>,
    /// Number of pieces still missing locally (BitTorrent only).
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub missing_pieces: Option<u32>,
    /// List of GIDs that follow (chained downloads)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub followed_by: Option<Vec<String>>,
    /// Parent GID this download belongs to (chained downloads)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub belongs_to: Option<String>,
    /// BitTorrent info hash (hex string)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info_hash: Option<String>,
    /// Number of seeders (BitTorrent only)
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub num_seeders: Option<u32>,
    /// Verified bytes length (when --check-integrity is active)
    #[serde(
        default,
        serialize_with = "wire::serialize_option_display_as_string",
        deserialize_with = "wire::deserialize_option_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub verified_length: Option<u64>,
    /// Whether integrity verification is pending ("true"/"false" string)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_integrity_pending: Option<String>,
}

impl Default for StatusInfo {
    fn default() -> Self {
        Self {
            gid: String::new(),
            total_length: None,
            completed_length: None,
            upload_length: None,
            download_speed: None,
            upload_speed: None,
            connections: None,
            error_code: None,
            error_message: None,
            status: DownloadStatus::Active,
            dir: None,
            files: None,
            bittorrent: None,
            following: None,
            seeder: None,
            bitfield: None,
            piece_length: None,
            num_pieces: None,
            completed_pieces: None,
            missing_pieces: None,
            followed_by: None,
            belongs_to: None,
            info_hash: None,
            num_seeders: None,
            verified_length: None,
            verify_integrity_pending: None,
        }
    }
}

impl StatusInfo {
    pub fn new(gid: impl Into<String>) -> Self {
        Self {
            gid: gid.into(),
            ..Default::default()
        }
    }

    pub fn with_total_length(mut self, v: u64) -> Self {
        self.total_length = Some(v);
        self
    }
    pub fn with_completed_length(mut self, v: u64) -> Self {
        self.completed_length = Some(v);
        self
    }
    pub fn with_download_speed(mut self, v: u64) -> Self {
        self.download_speed = Some(v);
        self
    }
    pub fn with_status(mut self, s: DownloadStatus) -> Self {
        self.status = s;
        self
    }
    pub fn with_dir(mut self, d: impl Into<String>) -> Self {
        self.dir = Some(d.into());
        self
    }
    pub fn with_files(mut self, f: Vec<FileInfo>) -> Self {
        self.files = Some(f);
        self
    }
    pub fn with_bittorrent(mut self, v: BittorrentInfo) -> Self {
        self.bittorrent = Some(v);
        self
    }
    pub fn with_following(mut self, v: impl Into<String>) -> Self {
        self.following = Some(v.into());
        self
    }
    pub fn with_error_code(mut self, c: i32) -> Self {
        self.error_code = Some(c);
        self
    }
    pub fn with_error_message(mut self, m: impl Into<String>) -> Self {
        self.error_message = Some(m.into());
        self
    }
    pub fn with_connections(mut self, c: u16) -> Self {
        self.connections = Some(c);
        self
    }
    pub fn with_upload_length(mut self, v: u64) -> Self {
        self.upload_length = Some(v);
        self
    }
    pub fn with_upload_speed(mut self, v: u64) -> Self {
        self.upload_speed = Some(v);
        self
    }
    pub fn with_seeder(mut self, v: impl Into<String>) -> Self {
        self.seeder = Some(v.into());
        self
    }
    pub fn with_bitfield(mut self, v: impl Into<String>) -> Self {
        self.bitfield = Some(v.into());
        self
    }
    pub fn with_piece_length(mut self, v: u64) -> Self {
        self.piece_length = Some(v);
        self
    }
    pub fn with_num_pieces(mut self, v: u32) -> Self {
        self.num_pieces = Some(v);
        self
    }
    pub fn with_completed_pieces(mut self, v: u32) -> Self {
        self.completed_pieces = Some(v);
        self
    }
    pub fn with_missing_pieces(mut self, v: u32) -> Self {
        self.missing_pieces = Some(v);
        self
    }
    pub fn with_followed_by(mut self, v: Vec<String>) -> Self {
        self.followed_by = Some(v);
        self
    }
    pub fn with_belongs_to(mut self, v: impl Into<String>) -> Self {
        self.belongs_to = Some(v.into());
        self
    }
    pub fn with_info_hash(mut self, v: impl Into<String>) -> Self {
        self.info_hash = Some(v.into());
        self
    }
    pub fn with_num_seeders(mut self, v: u32) -> Self {
        self.num_seeders = Some(v);
        self
    }
    pub fn with_verified_length(mut self, v: u64) -> Self {
        self.verified_length = Some(v);
        self
    }
    pub fn with_verify_integrity_pending(mut self, v: impl Into<String>) -> Self {
        self.verify_integrity_pending = Some(v.into());
        self
    }

    pub fn progress_percent(&self) -> f64 {
        match (self.total_length, self.completed_length) {
            (Some(total), Some(done)) if total > 0 => (done as f64 / total as f64) * 100.0,
            _ => 0.0,
        }
    }
}

// =========================================================================
// File and URI Types
// =========================================================================

/// File information for a download entry.
///
/// Returned by `aria2.getFiles`. Contains file path, size, progress,
/// selection state, and associated URIs. All numeric fields are serialized
/// as strings matching original aria2c wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub index: usize,
    pub path: String,
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub length: u64,
    #[serde(
        serialize_with = "wire::serialize_display_as_string",
        deserialize_with = "wire::deserialize_string_or_number"
    )]
    pub completed_length: u64,
    /// Whether this file is selected for download.
    /// Original aria2c serializes as "true"/"false" string.
    #[serde(
        serialize_with = "wire::serialize_bool_as_string",
        deserialize_with = "wire::deserialize_bool_from_string_or_bool"
    )]
    pub selected: bool,
    pub uris: Vec<UriEntry>,
}

impl Default for FileInfo {
    fn default() -> Self {
        Self {
            index: 1,
            path: String::new(),
            length: 0,
            completed_length: 0,
            selected: true,
            uris: vec![],
        }
    }
}

impl FileInfo {
    pub fn new(path: impl Into<String>, length: u64) -> Self {
        Self {
            path: path.into(),
            length,
            ..Default::default()
        }
    }

    pub fn with_uris(mut self, uris: Vec<UriEntry>) -> Self {
        self.uris = uris;
        self
    }
    pub fn with_completed(mut self, v: u64) -> Self {
        self.completed_length = v;
        self
    }
    pub fn with_index(mut self, v: usize) -> Self {
        self.index = v;
        self
    }
}

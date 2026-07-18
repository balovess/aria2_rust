//! RPC data model types.
//!
//! Contains all data structures used in RPC request/response payloads,
//! including download status, file info, server info, and session info.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use aria2_core::TorrentFileEntry;

// Re-export DownloadStatus from aria2-core as the canonical definition
pub use aria2_core::DownloadStatus;

/// Type alias for global configuration options shared across all downloads.
pub type GlobalOptions = Arc<RwLock<HashMap<String, serde_json::Value>>>;

/// Type alias for per-task configuration options keyed by GID.
pub type TaskOptions = Arc<RwLock<HashMap<String, HashMap<String, serde_json::Value>>>>;

// =========================================================================
// Download Status Types
// =========================================================================

/// Detailed status information for a download task.
///
/// Returned by `aria2.tellStatus`, `aria2.tellActive`, `aria2.tellWaiting`,
/// and `aria2.tellStopped`. Contains both static metadata (GID, directory)
/// and dynamic progress fields (speeds, lengths, connections).
///
/// **Field value types**: All numeric fields are stored and serialized as
/// JSON strings (e.g. `"totalLength": "5242880"`) to match the original
/// aria2 RPC protocol, which uses `util::itos()` to convert all numbers to
/// strings. This ensures maximum compatibility with strict parsers like
/// the original aria2c CLI and various third-party UIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusInfo {
    pub gid: String,
    pub status: DownloadStatus,
    /// Total file size in bytes (string per aria2 protocol)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_length: Option<String>,
    /// Completed bytes (string per aria2 protocol)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_length: Option<String>,
    /// Uploaded bytes (string per aria2 protocol)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_length: Option<String>,
    /// Current download speed in bytes/sec (string per aria2 protocol)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_speed: Option<String>,
    /// Current upload speed in bytes/sec (string per aria2 protocol)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_speed: Option<String>,
    /// Number of active connections (string per aria2 protocol)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connections: Option<String>,
    /// Hex-encoded bitfield of completed pieces (BT only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitfield: Option<String>,
    /// Piece length in bytes (string per aria2 protocol)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub piece_length: Option<String>,
    /// Total number of pieces (string per aria2 protocol)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_pieces: Option<String>,
    /// GIDs of downloads following this one (e.g. BT follow-ups)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub followed_by: Option<Vec<String>>,
    /// GID of the download this one is following
    #[serde(skip_serializing_if = "Option::is_none")]
    pub following: Option<String>,
    /// GID of the parent download this one belongs to
    #[serde(skip_serializing_if = "Option::is_none")]
    pub belongs_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Numeric error code (string per aria2 protocol)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// BitTorrent info hash (hex string, BT only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info_hash: Option<String>,
    /// Nested BitTorrent metadata (BT only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bittorrent: Option<BitTorrentInfo>,
    /// Number of seeders (string per aria2 protocol, BT only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_seeders: Option<String>,
    /// "true" or "false" — whether this client is seeding (BT only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeder: Option<String>,
    /// Verified length in bytes (string per aria2 protocol)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_length: Option<String>,
    /// "true" or "false" — whether integrity verification is pending
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_integrity_pending: Option<String>,
    /// Internal-only: torrent file entries. NEVER serialized to JSON-RPC
    /// output (original aria2 uses the `files` array for BT entries).
    #[serde(skip)]
    pub torrent_files: Option<Vec<TorrentFileEntry>>,
}

impl Default for StatusInfo {
    fn default() -> Self {
        Self {
            gid: String::new(),
            status: DownloadStatus::Active,
            total_length: None,
            completed_length: None,
            upload_length: None,
            download_speed: None,
            upload_speed: None,
            connections: None,
            bitfield: None,
            piece_length: None,
            num_pieces: None,
            followed_by: None,
            following: None,
            belongs_to: None,
            files: None,
            dir: None,
            error_code: None,
            error_message: None,
            info_hash: None,
            bittorrent: None,
            num_seeders: None,
            seeder: None,
            verified_length: None,
            verify_integrity_pending: None,
            torrent_files: None,
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

    /// Set total length from a numeric value (converted to string for protocol compat).
    pub fn with_total_length(mut self, v: u64) -> Self {
        self.total_length = Some(v.to_string());
        self
    }
    pub fn with_completed_length(mut self, v: u64) -> Self {
        self.completed_length = Some(v.to_string());
        self
    }
    pub fn with_download_speed(mut self, v: u64) -> Self {
        self.download_speed = Some(v.to_string());
        self
    }
    pub fn with_upload_speed(mut self, v: u64) -> Self {
        self.upload_speed = Some(v.to_string());
        self
    }
    pub fn with_upload_length(mut self, v: u64) -> Self {
        self.upload_length = Some(v.to_string());
        self
    }
    pub fn with_connections(mut self, c: u16) -> Self {
        self.connections = Some(c.to_string());
        self
    }
    pub fn with_error_code(mut self, c: i32) -> Self {
        self.error_code = Some(c.to_string());
        self
    }
    pub fn with_error_message(mut self, m: impl Into<String>) -> Self {
        self.error_message = Some(m.into());
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
    pub fn with_torrent_files(mut self, files: Vec<TorrentFileEntry>) -> Self {
        self.torrent_files = Some(files);
        self
    }
    /// Set piece length in bytes (BT only).
    pub fn with_piece_length(mut self, v: u64) -> Self {
        self.piece_length = Some(v.to_string());
        self
    }
    /// Set total number of pieces (BT only).
    pub fn with_num_pieces(mut self, v: u64) -> Self {
        self.num_pieces = Some(v.to_string());
        self
    }
    /// Set hex-encoded bitfield of completed pieces (BT only).
    pub fn with_bitfield(mut self, b: impl Into<String>) -> Self {
        self.bitfield = Some(b.into());
        self
    }
    /// Set info hash hex string (BT only).
    pub fn with_info_hash(mut self, h: impl Into<String>) -> Self {
        self.info_hash = Some(h.into());
        self
    }
    /// Set BitTorrent metadata nested object (BT only).
    pub fn with_bittorrent(mut self, bt: BitTorrentInfo) -> Self {
        self.bittorrent = Some(bt);
        self
    }
    /// Set number of seeders (BT only).
    pub fn with_num_seeders(mut self, n: u64) -> Self {
        self.num_seeders = Some(n.to_string());
        self
    }
    /// Set seeder flag ("true"/"false") — BT only.
    pub fn with_seeder(mut self, is_seeder: bool) -> Self {
        self.seeder = Some(is_seeder.to_string());
        self
    }
    /// Set verified length in bytes.
    pub fn with_verified_length(mut self, v: u64) -> Self {
        self.verified_length = Some(v.to_string());
        self
    }
    /// Set verify integrity pending flag.
    pub fn with_verify_integrity_pending(mut self, pending: bool) -> Self {
        self.verify_integrity_pending = Some(pending.to_string());
        self
    }
    /// Set list of GIDs following this download.
    pub fn with_followed_by(mut self, gids: Vec<String>) -> Self {
        self.followed_by = Some(gids);
        self
    }
    /// Set the GID this download is following.
    pub fn with_following(mut self, gid: impl Into<String>) -> Self {
        self.following = Some(gid.into());
        self
    }
    /// Set the parent GID this download belongs to.
    pub fn with_belongs_to(mut self, gid: impl Into<String>) -> Self {
        self.belongs_to = Some(gid.into());
        self
    }

    /// Calculate download progress percentage by parsing string fields.
    ///
    /// Returns 0.0 if either length is missing or unparseable.
    pub fn progress_percent(&self) -> f64 {
        let total: u64 = self
            .total_length
            .as_ref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let done: u64 = self
            .completed_length
            .as_ref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if total > 0 {
            (done as f64 / total as f64) * 100.0
        } else {
            0.0
        }
    }
}

/// BitTorrent metadata nested object returned by `aria2.tellStatus` for BT downloads.
///
/// Mirrors the original aria2 `bittorrent` field structure:
/// - `announceList`: list of tracker tiers (each tier is a list of URLs)
/// - `comment`: optional torrent comment
/// - `creationDate`: optional Unix timestamp
/// - `mode`: "single" or "multi"
/// - `info.name`: torrent name from info dictionary
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BitTorrentInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announce_list: Option<Vec<Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<BitTorrentInfoMeta>,
}

impl BitTorrentInfo {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_announce_list(mut self, list: Vec<Vec<String>>) -> Self {
        self.announce_list = Some(list);
        self
    }
    pub fn with_comment(mut self, c: impl Into<String>) -> Self {
        self.comment = Some(c.into());
        self
    }
    pub fn with_creation_date(mut self, d: i64) -> Self {
        self.creation_date = Some(d);
        self
    }
    pub fn with_mode(mut self, m: impl Into<String>) -> Self {
        self.mode = Some(m.into());
        self
    }
    pub fn with_info_name(mut self, name: impl Into<String>) -> Self {
        self.info = Some(BitTorrentInfoMeta {
            name: Some(name.into()),
        });
        self
    }
}

/// Inner `info` object of `BitTorrentInfo`, containing the torrent name.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct BitTorrentInfoMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

// =========================================================================
// File and URI Types
// =========================================================================

/// File information for a download entry.
///
/// Returned by `aria2.getFiles`. Contains file path, size, progress,
/// selection state, and associated URIs.
///
/// # Wire format compatibility
///
/// Matches original aria2 `createFileEntry` (RpcMethodImpl.cc:558-580),
/// which emits EVERY scalar field as a JSON string:
/// - `index` via `util::uitos(index)` (1-based)
/// - `length` / `completedLength` via `util::itos(...)`
/// - `selected` as `VLB_TRUE` / `VLB_FALSE` ("true" / "false")
///
/// Plugins (AriaNg, YAAM) parse these as strings; emitting numbers silently
/// breaks them. We therefore store each scalar as a `String` — consistent
/// with `PeerInfo`, `GlobalStat`, and `StatusInfo`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub index: String,
    pub path: String,
    pub length: String,
    pub completed_length: String,
    pub selected: String,
    pub uris: Vec<UriEntry>,
}

impl Default for FileInfo {
    fn default() -> Self {
        Self {
            index: "1".to_string(),
            path: String::new(),
            length: "0".to_string(),
            completed_length: "0".to_string(),
            selected: "true".to_string(),
            uris: vec![],
        }
    }
}

impl FileInfo {
    /// Build a `FileInfo` with the given path and length.
    ///
    /// `length` accepts any integer type — it is converted to its decimal
    /// string representation (matching `util::itos()`). The `index` defaults
    /// to `"1"` (1-based, matching original aria2) and `selected` defaults to
    /// `"true"`; use [`FileInfo::with_index`] and [`FileInfo::with_selected`]
    /// to override.
    pub fn new(path: impl Into<String>, length: impl ToString) -> Self {
        Self {
            path: path.into(),
            length: length.to_string(),
            ..Default::default()
        }
    }

    pub fn with_uris(mut self, uris: Vec<UriEntry>) -> Self {
        self.uris = uris;
        self
    }
    pub fn with_completed(mut self, v: impl ToString) -> Self {
        self.completed_length = v.to_string();
        self
    }
    /// Set the 1-based file index (matches original aria2 `util::uitos(index)`).
    pub fn with_index(mut self, idx: impl ToString) -> Self {
        self.index = idx.to_string();
        self
    }
    /// Set the `selected` flag from a Rust bool — serialized as `"true"`/`"false"`
    /// (matches original aria2 `VLB_TRUE`/`VLB_FALSE`).
    pub fn with_selected(mut self, selected: bool) -> Self {
        self.selected = bool_to_str(selected).to_string();
        self
    }
}

/// URI entry with status tracking.
///
/// Used in `FileInfo.uris` and returned by `aria2.getUris`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UriEntry {
    pub uri: String,
    pub status: UriStatus,
}

impl UriEntry {
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            status: UriStatus::Waiting,
        }
    }
    pub fn used(mut self) -> Self {
        self.status = UriStatus::Used;
        self
    }
    pub fn waiting(mut self) -> Self {
        self.status = UriStatus::Waiting;
        self
    }
}

/// URI status indicating whether a URI is currently being used or waiting.
///
/// # Wire format compatibility
///
/// Serializes to lowercase `"used"` / `"waiting"` to match the original
/// aria2 `VLB_USED` / `VLB_WAITING` constants (`VARIABLE_LOCALBUNDLE` in
/// `RpcMethodImpl.cc`). Without `rename_all = "lowercase"` the enum variants
/// would serialize as `"Used"` / `"Waiting"` and break strict parsers like
/// AriaNg / YAAM.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum UriStatus {
    Used,
    #[default]
    Waiting,
}

/// URI information returned by `aria2.getUris`.
///
/// Type alias for [`UriEntry`] for API compatibility.
pub type UriInfo = UriEntry;

// =========================================================================
// Server and Peer Types
// =========================================================================

/// Server connection information for a specific file index.
///
/// Returned by `aria2.getServers`, grouped by file index.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfoIndex {
    /// File index (0-based)
    pub index: usize,
    /// List of active server connections for this file
    pub servers: Vec<ServerInfo>,
}

/// Individual server connection details.
///
/// Contains URI, current active URI (after redirects), and download speed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// Original server URI
    pub uri: String,
    /// Current active URI (may differ from original after redirects)
    pub current_uri: String,
    /// Current download speed from this server (bytes/sec)
    pub download_speed: u64,
}

impl ServerInfo {
    /// Create a new ServerInfo instance.
    pub fn new(uri: impl Into<String>) -> Self {
        let uri_str = uri.into();
        Self {
            current_uri: uri_str.clone(),
            uri: uri_str,
            download_speed: 0,
        }
    }

    /// Set the current (possibly redirected) URI.
    pub fn with_current_uri(mut self, uri: impl Into<String>) -> Self {
        self.current_uri = uri.into();
        self
    }

    /// Set the download speed.
    pub fn with_download_speed(mut self, speed: u64) -> Self {
        self.download_speed = speed;
        self
    }
}

/// Information about a single BitTorrent peer.
///
/// Matches the wire format produced by `gatherPeerEntry` in the original
/// aria2 `RpcMethodImpl.cc`: every field is serialized as a JSON string
/// (numeric values via `util::itos()`, booleans as `"true"`/`"false"`).
/// `bitfield` is emitted as a lowercase hex string; `seeder` is omitted
/// when the peer has not yet announced as a seeder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub peer_id: String,
    pub ip: String,
    pub port: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitfield: Option<String>,
    pub am_choking: String,
    pub peer_choking: String,
    pub download_speed: String,
    pub upload_speed: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeder: Option<String>,
}

impl PeerInfo {
    /// Build a minimal PeerInfo with the two required identity fields.
    ///
    /// Speeds default to `"0"`, choking flags default to `"false"`, and
    /// `bitfield`/`seeder` default to `None` (omitted from JSON output).
    pub fn new(peer_id: impl Into<String>, ip: impl Into<String>) -> Self {
        Self {
            peer_id: peer_id.into(),
            ip: ip.into(),
            port: "0".to_string(),
            bitfield: None,
            am_choking: "false".to_string(),
            peer_choking: "false".to_string(),
            download_speed: "0".to_string(),
            upload_speed: "0".to_string(),
            seeder: None,
        }
    }

    /// Set the peer's remote port (any integer-encoded form is accepted).
    pub fn with_port(mut self, port: impl ToString) -> Self {
        self.port = port.to_string();
        self
    }

    /// Set the bitfield as a hex string (the original aria2 emits lowercase
    /// hex via `util::toHex`).
    pub fn with_bitfield(mut self, bitfield: impl Into<String>) -> Self {
        self.bitfield = Some(bitfield.into());
        self
    }

    /// Set the am-choking flag from a Rust bool; serialized as `"true"`/`"false"`.
    pub fn with_am_choking(mut self, am_choking: bool) -> Self {
        self.am_choking = bool_to_str(am_choking).to_string();
        self
    }

    /// Set the peer-choking flag from a Rust bool; serialized as `"true"`/`"false"`.
    pub fn with_peer_choking(mut self, peer_choking: bool) -> Self {
        self.peer_choking = bool_to_str(peer_choking).to_string();
        self
    }

    /// Set the download speed (any integer).
    pub fn with_download_speed(mut self, speed: impl ToString) -> Self {
        self.download_speed = speed.to_string();
        self
    }

    /// Set the upload speed (any integer).
    pub fn with_upload_speed(mut self, speed: impl ToString) -> Self {
        self.upload_speed = speed.to_string();
        self
    }

    /// Mark this peer as a seeder (`true`) or leecher (`false`).
    pub fn with_seeder(mut self, seeder: bool) -> Self {
        self.seeder = Some(bool_to_str(seeder).to_string());
        self
    }
}

/// Convert a Rust bool to the original aria2 wire-format string.
///
/// Returns `"true"` / `"false"` matching aria2 `VLB_TRUE` / `VLB_FALSE`.
/// `pub(crate)` so that handlers (e.g. `bittorrent.rs`) can emit wire-format
/// strings when constructing `FileInfo` entries from raw state.
pub(crate) fn bool_to_str(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

// =========================================================================
// Global Statistics
// =========================================================================

/// Global download statistics.
///
/// Returned by `aria2.getGlobalStat`. Contains aggregate numbers for
/// active, waiting, and stopped downloads, plus total transfer speeds.
///
/// # Wire format compatibility
///
/// Matches original aria2 `GetGlobalStatRpcMethod::process`
/// (RpcMethodImpl.cc:1382-1394), which emits EVERY field as a JSON string
/// via `util::itos()` / `util::uitos()`. Plugins (AriaNg, YAAM) parse these
/// fields as strings; emitting numbers silently breaks them. We therefore
/// store each value as a `String` — consistent with `PeerInfo` and
/// `tellStatus`'s `downloadSpeed` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalStat {
    #[serde(rename = "downloadSpeed")]
    pub download_speed: String,
    #[serde(rename = "uploadSpeed")]
    pub upload_speed: String,
    #[serde(rename = "numActive")]
    pub num_active: String,
    #[serde(rename = "numWaiting")]
    pub num_waiting: String,
    #[serde(rename = "numStopped")]
    pub num_stopped: String,
    #[serde(rename = "numStoppedTotal")]
    pub num_stopped_total: String,
}

impl GlobalStat {
    /// Build a `GlobalStat` from raw numeric values, converting each to its
    /// decimal string representation (matches `util::itos()` / `util::uitos()`).
    ///
    /// This is the primary constructor used by `handle_global_stat`; direct
    /// field assignment is discouraged because it bypasses the string
    /// conversion that the wire format requires.
    pub fn from_numbers(
        download_speed: u64,
        upload_speed: u64,
        num_active: usize,
        num_waiting: usize,
        num_stopped: usize,
        num_stopped_total: usize,
    ) -> Self {
        Self {
            download_speed: download_speed.to_string(),
            upload_speed: upload_speed.to_string(),
            num_active: num_active.to_string(),
            num_waiting: num_waiting.to_string(),
            num_stopped: num_stopped.to_string(),
            num_stopped_total: num_stopped_total.to_string(),
        }
    }

    /// Convert to JSON-RPC response value.
    ///
    /// Fields are already strings, so the derived `Serialize` impl produces
    /// the same output; this method exists for the handler's convenience
    /// and to make the camelCase key names explicit at the call site.
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "downloadSpeed": self.download_speed,
            "uploadSpeed": self.upload_speed,
            "numActive": self.num_active,
            "numWaiting": self.num_waiting,
            "numStopped": self.num_stopped,
            "numStoppedTotal": self.num_stopped_total
        })
    }
}

impl Default for GlobalStat {
    /// Default to `"0"` for every field — matches the original aria2's
    /// zero-state wire output (`util::itos(0)` produces `"0"`).
    fn default() -> Self {
        Self {
            download_speed: "0".to_string(),
            upload_speed: "0".to_string(),
            num_active: "0".to_string(),
            num_waiting: "0".to_string(),
            num_stopped: "0".to_string(),
            num_stopped_total: "0".to_string(),
        }
    }
}

// =========================================================================
// Version and Session Types
// =========================================================================

/// Version information returned by `aria2.getVersion`.
///
/// Contains the aria2 version string and list of enabled features.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    /// Version string (e.g., "1.37.0-Rust")
    pub version: String,
    /// List of enabled feature names (serialized as "enabledFeatures" in JSON)
    #[serde(rename = "enabledFeatures")]
    pub enabled_features: Vec<String>,
}

impl VersionInfo {
    /// Create VersionInfo from environment or defaults.
    ///
    /// The `enabled_features` list mirrors the original aria2
    /// `strSupportedFeature()` names exactly (see `FeatureConfig.cc:115-189`),
    /// reporting only features genuinely implemented in the Rust port:
    ///
    /// | Feature | Original flag | Rust implementation |
    /// |---------|---------------|---------------------|
    /// | `BitTorrent` | `ENABLE_BITTORRENT` | `addTorrent` handler + BT engine |
    /// | `GZip` | `HAVE_ZLIB` | `aria2_core::http::stream_filter::GZipDecoder` (flate2) |
    /// | `HTTPS` | `ENABLE_SSL` | `tokio-rustls` TLS in `server.rs` |
    /// | `Message Digest` | always enabled | sha1/md5 via `aria2_core` |
    /// | `Metalink` | `ENABLE_METALINK` | `addMetalink` handler + quick-xml |
    /// | `XML-RPC` | `ENABLE_XML_RPC` | `aria2_rpc::xml_rpc` module |
    ///
    /// Features NOT reported (not implemented): `Async DNS` (c-ares),
    /// `Firefox3 Cookie` (sqlite3), `SFTP` (libssh2).
    pub fn from_env() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            enabled_features: vec![
                "BitTorrent".to_string(),
                "GZip".to_string(),
                "HTTPS".to_string(),
                "Message Digest".to_string(),
                "Metalink".to_string(),
                "XML-RPC".to_string(),
            ],
        }
    }

    /// Convert to JSON-RPC response value (camelCase keys).
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "enabledFeatures": self.enabled_features,
            "version": self.version
        })
    }
}

/// Session information returned by `aria2.getSessionInfo`.
///
/// # Wire format compatibility
///
/// Matches original aria2 `GetSessionInfoRpcMethod::process`
/// (RpcMethodImpl.cc:1254-1260), which emits ONLY `sessionId`. The
/// `session_start_time` field is kept for internal diagnostics but is
/// `#[serde(skip)]`-ed so the derived `Serialize` impl is consistent with
/// `to_json_value()` — preventing `sessionStartTime` from ever leaking into
/// the wire response and breaking plugin compatibility (AriaNg asserts the
/// response contains only `sessionId`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Unique session identifier (the only field emitted on the wire).
    pub session_id: String,
    /// Session start time as Unix timestamp (seconds since epoch).
    ///
    /// Internal-only — never serialized. Used for diagnostics and session
    /// timeout calculations.
    #[serde(skip)]
    pub session_start_time: u64,
}

impl SessionInfo {
    /// Create a new SessionInfo with current timestamp.
    pub fn new() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let start_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            session_id: format!("session-{:x}", start_time),
            session_start_time: start_time,
        }
    }

    /// Convert to JSON-RPC response value (camelCase key).
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "sessionId": self.session_id
        })
    }
}

impl Default for SessionInfo {
    fn default() -> Self {
        Self::new()
    }
}

// =========================================================================
// GID Generation
// =========================================================================

fn generate_gid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    nanos.hash(&mut hasher);
    rand::random::<u64>().hash(&mut hasher);
    format!("{:01$x}", hasher.finish(), crate::constants::GID_HEX_DIGITS)
}

/// Generate a unique GID (Global IDentifier) for a download task.
///
/// Uses a combination of current time (nanoseconds), a hash, and a random
/// value to produce a 16-character hexadecimal identifier.
pub fn create_gid() -> String {
    generate_gid()
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use aria2_core::TorrentFileEntry;

    #[test]
    fn test_status_info_default() {
        let info = StatusInfo::default();
        assert!(info.gid.is_empty());
        assert_eq!(info.progress_percent(), 0.0);
    }

    #[test]
    fn test_status_info_builder() {
        let info = StatusInfo::new("abc123")
            .with_total_length(1000)
            .with_completed_length(500)
            .with_download_speed(1024)
            .with_status(DownloadStatus::Active);
        assert_eq!(info.gid, "abc123");
        assert!((info.progress_percent() - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_download_status_variants() {
        assert!(DownloadStatus::Active.is_active());
        assert!(DownloadStatus::Complete.is_stopped());
        assert_eq!(DownloadStatus::Error("test".to_string()).as_str(), "error");
    }

    #[test]
    fn test_file_info_default() {
        let fi = FileInfo::default();
        // All scalars are wire-format strings matching original aria2.
        assert_eq!(fi.selected, "true", "selected defaults to VLB_TRUE");
        assert_eq!(fi.index, "1", "index defaults to 1 (1-based)");
        assert_eq!(fi.length, "0", "length defaults to 0");
        assert_eq!(fi.completed_length, "0", "completed_length defaults to 0");
        assert_eq!(fi.uris.len(), 0);
    }

    #[test]
    fn test_file_info_builder() {
        let fi = FileInfo::new("/tmp/file.iso", 1048576)
            .with_uris(vec![UriEntry::new("http://example.com/file.iso")]);
        assert_eq!(fi.length, "1048576");
        assert_eq!(fi.path, "/tmp/file.iso");
        assert_eq!(fi.index, "1", "index defaults to 1 (1-based)");
        assert_eq!(fi.selected, "true", "selected defaults to true");
        assert_eq!(fi.uris.len(), 1);

        // with_index / with_selected override defaults.
        let fi2 = FileInfo::new("/tmp/x.bin", 0)
            .with_index(7)
            .with_selected(false);
        assert_eq!(fi2.index, "7");
        assert_eq!(fi2.selected, "false");
    }

    #[test]
    fn test_uri_entry() {
        let uri = UriEntry::new("http://example.com/file.iso").used();
        assert_eq!(uri.status, UriStatus::Used);

        let w = UriEntry::new("http://x.com/f").waiting();
        assert_eq!(w.status, UriStatus::Waiting);

        // Wire format: lowercase "used" / "waiting" matching original aria2
        // VLB_USED / VLB_WAITING constants.
        let used_json = serde_json::to_value(&uri).unwrap();
        assert_eq!(used_json["status"], "used", "UriStatus::Used -> \"used\"");
        let waiting_json = serde_json::to_value(&w).unwrap();
        assert_eq!(
            waiting_json["status"], "waiting",
            "UriStatus::Waiting -> \"waiting\""
        );
    }

    #[test]
    fn test_global_stat_default() {
        let stat = GlobalStat::default();
        // All fields default to "0" (string), matching util::itos(0).
        assert_eq!(stat.download_speed, "0");
        assert_eq!(stat.upload_speed, "0");
        assert_eq!(stat.num_active, "0");
        assert_eq!(stat.num_waiting, "0");
        assert_eq!(stat.num_stopped, "0");
        assert_eq!(stat.num_stopped_total, "0");
        let val = stat.to_json_value();
        assert!(val.get("downloadSpeed").is_some());
        // Wire format: JSON strings, not numbers.
        assert_eq!(val["downloadSpeed"], "0");
        assert_eq!(val["numActive"], "0");
    }

    #[test]
    fn test_global_stat_from_numbers() {
        let stat = GlobalStat::from_numbers(500_000, 100_000, 2, 3, 5, 7);
        assert_eq!(stat.download_speed, "500000");
        assert_eq!(stat.upload_speed, "100000");
        assert_eq!(stat.num_active, "2");
        assert_eq!(stat.num_waiting, "3");
        assert_eq!(stat.num_stopped, "5");
        assert_eq!(stat.num_stopped_total, "7");
        let val = stat.to_json_value();
        assert_eq!(val["downloadSpeed"], "500000");
        assert_eq!(val["numActive"], "2");
    }

    #[test]
    fn test_generate_gid() {
        let gid1 = create_gid();
        let gid2 = create_gid();
        assert_eq!(gid1.len(), 16);
        assert_ne!(gid1, gid2);
    }

    #[test]
    fn test_status_info_holds_torrent_file_entries() {
        let entries = vec![
            TorrentFileEntry {
                index: 0,
                path: "dir/file1.txt".to_string(),
                length: 500,
                completed_length: 500,
            },
            TorrentFileEntry {
                index: 1,
                path: "dir/file2.dat".to_string(),
                length: 524,
                completed_length: 200,
            },
        ];

        let info = StatusInfo::new("gid-torrent-001")
            .with_total_length(1024)
            .with_completed_length(700)
            .with_torrent_files(entries.clone());

        assert!(
            info.torrent_files.is_some(),
            "torrent_files should be Some after with_torrent_files"
        );
        let files = info.torrent_files.as_ref().unwrap();
        assert_eq!(files.len(), 2, "Should hold 2 file entries");
        assert_eq!(files[0].index, 0);
        assert_eq!(files[0].path, "dir/file1.txt");
        assert_eq!(files[0].length, 500);
        assert_eq!(files[1].index, 1);
        assert_eq!(files[1].path, "dir/file2.dat");
        assert_eq!(files[1].length, 524);

        let default_info = StatusInfo::default();
        assert!(
            default_info.torrent_files.is_none(),
            "Default StatusInfo should have None torrent_files"
        );

        // torrent_files is marked #[serde(skip)] to match original aria2:
        // original aria2 uses the `files` array for BT entries, never emits
        // a separate `torrentFiles` field. Strict clients may error on
        // unknown fields.
        let serialized = serde_json::to_value(&info).unwrap();
        assert!(
            serialized.get("torrentFiles").is_none(),
            "torrentFiles should NOT appear in JSON output (serde(skip))"
        );
    }

    #[test]
    fn test_peer_info_serialization() {
        let peer = PeerInfo::new("peer-abc123", "192.168.1.100")
            .with_port(6881u16)
            .with_bitfield("ffff")
            .with_am_choking(false)
            .with_peer_choking(true)
            .with_download_speed(1048576u64)
            .with_upload_speed(512000u64)
            .with_seeder(false);
        let json = serde_json::to_value(&peer).unwrap();
        assert_eq!(json["peerId"], "peer-abc123");
        assert_eq!(json["ip"], "192.168.1.100");
        assert_eq!(json["port"], "6881");
        assert_eq!(json["bitfield"], "ffff");
        assert_eq!(json["amChoking"], "false");
        assert_eq!(json["peerChoking"], "true");
        assert_eq!(json["downloadSpeed"], "1048576");
        assert_eq!(json["uploadSpeed"], "512000");
        assert_eq!(json["seeder"], "false");

        let roundtrip: PeerInfo = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip.port, "6881");
        assert_eq!(roundtrip.bitfield.as_deref(), Some("ffff"));
        assert_eq!(roundtrip.seeder.as_deref(), Some("false"));
    }

    #[test]
    fn test_peer_info_optional_fields_are_omitted_when_unset() {
        // Original aria2 omits `bitfield` for fresh peers (no pieces yet)
        // and `seeder` when the peer hasn't announced. Verify serde respects
        // the `skip_serializing_if` directive so plugins don't see nulls.
        let peer = PeerInfo::new("peer-x", "10.0.0.1");
        let json_str = serde_json::to_string(&peer).unwrap();
        assert!(
            !json_str.contains("bitfield"),
            "bitfield should be omitted when unset, got: {json_str}"
        );
        assert!(
            !json_str.contains("seeder"),
            "seeder should be omitted when unset, got: {json_str}"
        );
        // Required fields still present.
        assert!(json_str.contains("\"port\":\"0\""));
        assert!(json_str.contains("\"amChoking\":\"false\""));
    }
}

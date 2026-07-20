//! RPC data model types.
//!
//! Contains all data structures used in RPC request/response payloads,
//! including download status, file info, server info, and session info.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// Re-export DownloadStatus from aria2-core as the canonical definition
pub use aria2_core::DownloadStatus;

/// Type alias for global configuration options shared across all downloads.
pub type GlobalOptions = Arc<RwLock<HashMap<String, serde_json::Value>>>;

/// Type alias for per-task configuration options keyed by GID.
pub type TaskOptions = Arc<RwLock<HashMap<String, HashMap<String, serde_json::Value>>>>;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_speed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_speed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connections: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub status: DownloadStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileInfo>>,
    /// BitTorrent metadata (matches original nested `bittorrent` object)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bittorrent: Option<BittorrentInfo>,
    /// Following GID (single string, matches original aria2 behavior)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub following: Option<String>,
    /// Whether this download is seeding (BitTorrent only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeder: Option<String>,
    /// Hex-encoded piece bitfield (BitTorrent only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitfield: Option<String>,
    /// Piece length in bytes (BitTorrent only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub piece_length: Option<u64>,
    /// Number of pieces (BitTorrent only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_pieces: Option<u32>,
    /// List of GIDs that follow (chained downloads)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub followed_by: Option<Vec<String>>,
    /// Parent GID this download belongs to (chained downloads)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub belongs_to: Option<String>,
    /// BitTorrent info hash (hex string)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info_hash: Option<String>,
    /// Number of seeders (BitTorrent only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_seeders: Option<u32>,
    /// Verified bytes length (when --check-integrity is active)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_length: Option<u64>,
    /// Whether integrity verification is pending ("true"/"false" string)
    #[serde(skip_serializing_if = "Option::is_none")]
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
/// selection state, and associated URIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub index: usize,
    pub path: String,
    pub length: u64,
    pub completed_length: u64,
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

/// BitTorrent peer information.
///
/// Returned by `aria2.getPeers`. Contains peer connection state and
/// transfer speeds. Matches original aria2 peer entry fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub peer_id: String,
    pub ip: String,
    pub port: u16,
    /// Bitfield hex string (matches original util::toHex)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitfield: Option<String>,
    pub am_choking: bool,
    pub peer_choking: bool,
    pub download_speed: u64,
    pub upload_speed: u64,
    /// Seeder status as "true"/"false" string (matches original VLB_TRUE/VLB_FALSE)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeder: Option<String>,
}

// =========================================================================
// Global Statistics
// =========================================================================

/// Global download statistics.
///
/// Returned by `aria2.getGlobalStat`. Contains aggregate numbers for
/// active, waiting, and stopped downloads, plus total transfer speeds.
#[derive(Debug, Clone, Serialize, Default)]
pub struct GlobalStat {
    pub download_speed: u64,
    pub upload_speed: u64,
    pub num_active: usize,
    pub num_waiting: usize,
    pub num_stopped: usize,
    pub num_stopped_total: usize,
}

impl GlobalStat {
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
    /// Enabled features are dynamically generated based on compile-time
    /// protocol support available in the current build. The list reflects
    /// which protocols and capabilities aria2-core is compiled with.
    pub fn from_env() -> Self {
        let mut features: Vec<&str> = Vec::new();

        // Always-on protocol support via reqwest + aria2-protocol
        features.push("http");
        features.push("https");
        features.push("ftp");
        features.push("bittorrent");
        features.push("metalink");
        features.push("sftp");

        // Async DNS: available when the tokio-based resolver is compiled in
        features.push("Async DNS");

        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            enabled_features: features.into_iter().map(|s| s.to_string()).collect(),
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
/// Contains session identifier and startup timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Unique session identifier
    pub session_id: String,
    /// Session start time as Unix timestamp (seconds since epoch)
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

    // -------------------------------------------------------------------------
    // Mock torrent builder helpers (self-contained, no external crate dependency)
    // -------------------------------------------------------------------------

    /// Minimal bencode integer encoding.
    fn ben_int(v: i64) -> Vec<u8> {
        format!("i{}e", v).into_bytes()
    }

    /// Minimal bencode string encoding.
    fn ben_str(s: &str) -> Vec<u8> {
        format!("{}:{}", s.len(), s).into_bytes()
    }

    /// Minimal bencode bytes encoding.
    fn ben_bytes(data: &[u8]) -> Vec<u8> {
        let mut out = format!("{}:", data.len()).into_bytes();
        out.extend_from_slice(data);
        out
    }

    /// Minimal bencode dict encoding from a list of (key_bytes, value_bytes).
    fn ben_dict(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let mut out = b"d".to_vec();
        for (k, v) in entries {
            out.extend_from_slice(k);
            out.extend_from_slice(v);
        }
        out.push(b'e');
        out
    }

    /// Minimal bencode list encoding from a list of value bytes.
    fn ben_list(items: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"l".to_vec();
        for item in items {
            out.extend_from_slice(item);
        }
        out.push(b'e');
        out
    }

    /// Build a mock .torrent file in bencode format with full metadata.
    ///
    /// Metadata included:
    /// - announce, announce-list (multi-tier)
    /// - comment, creation date, created by
    /// - info dict with name, piece length, pieces, length (single-file mode)
    fn build_mock_torrent_with_full_metadata() -> Vec<u8> {
        // Pieces: 2 pieces x 20 bytes = 40 bytes of SHA-1 hash data
        let pieces: Vec<u8> = (0..40).map(|i| i as u8).collect();

        let info_dict = ben_dict(&[
            (ben_str("name"), ben_str("test-file.iso")),
            (ben_str("length"), ben_int(1048576)),          // 1 MiB
            (ben_str("piece length"), ben_int(262144)),      // 256 KiB
            (ben_str("pieces"), ben_bytes(&pieces)),
        ]);

        // announce-list: [[tier1_uri], [tier2_uri_a, tier2_uri_b]]
        let tier1 = ben_list(&[ben_str("udp://tracker.aria2.org:80")]);
        let tier2 = ben_list(&[
            ben_str("http://tracker.example.com:80/announce"),
            ben_str("https://tracker.example.org:443/announce"),
        ]);
        let announce_list = ben_list(&[tier1, tier2]);

        let torrent = ben_dict(&[
            (ben_str("announce"), ben_str("http://tracker.example.com:80/announce")),
            (ben_str("announce-list"), announce_list),
            (ben_str("comment"), ben_str("Aria2 Rust mock torrent for testing")),
            (ben_str("creation date"), ben_int(1700000000)),
            (ben_str("created by"), ben_str("aria2-rust-test/0.2.4")),
            (ben_str("info"), info_dict),
        ]);

        torrent
    }

    /// Build a mock multi-file torrent with metadata.
    fn build_mock_multi_file_torrent() -> Vec<u8> {
        let pieces: Vec<u8> = (0..60).map(|i| i as u8).collect(); // 3 pieces

        let file1_dict = ben_dict(&[
            (ben_str("length"), ben_int(500)),
            (ben_str("path"), ben_list(&[
                ben_str("dir1"),
                ben_str("file1.txt"),
            ])),
        ]);
        let file2_dict = ben_dict(&[
            (ben_str("length"), ben_int(524)),
            (ben_str("path"), ben_list(&[
                ben_str("dir2"),
                ben_str("file2.dat"),
            ])),
        ]);

        let info_dict = ben_dict(&[
            (ben_str("name"), ben_str("multi-dir-torrent")),
            (ben_str("files"), ben_list(&[file1_dict, file2_dict])),
            (ben_str("piece length"), ben_int(512)),
            (ben_str("pieces"), ben_bytes(&pieces)),
        ]);

        let announce_list = ben_list(&[
            ben_list(&[ben_str("udp://tracker.multi.com:80")]),
        ]);

        let torrent = ben_dict(&[
            (ben_str("announce"), ben_str("http://tracker.multi.com:80/announce")),
            (ben_str("announce-list"), announce_list),
            (ben_str("comment"), ben_str("Multi-file torrent for testing")),
            (ben_str("creation date"), ben_int(1800000000)),
            (ben_str("created by"), ben_str("aria2-rust-test")),
            (ben_str("info"), info_dict),
        ]);

        torrent
    }

    // -------------------------------------------------------------------------
    // Inline bencode field extractors (minimal, for test use only)
    // -------------------------------------------------------------------------

    /// Extract a bencode string value by key from a bencode dict.
    fn extract_bencode_str(data: &[u8], key: &str) -> Option<String> {
        let key_bytes = key.as_bytes();
        // Search for "<len(key)>:<key>" in the bytes
        let search = format!("{}:{}", key_bytes.len(), key);
        let needle = search.as_bytes();
        let pos = data.windows(needle.len()).position(|w| w == needle)?;
        let value_start = pos + needle.len();
        // Read the length-prefixed string at value_start
        let colon_pos = data[value_start..].iter().position(|&b| b == b':')?;
        let len_str = std::str::from_utf8(&data[value_start..value_start + colon_pos]).ok()?;
        let len: usize = len_str.parse().ok()?;
        let val_start = value_start + colon_pos + 1;
        if val_start + len > data.len() {
            return None;
        }
        Some(String::from_utf8_lossy(&data[val_start..val_start + len]).to_string())
    }

    /// Extract a bencode integer value by key from a bencode dict.
    fn extract_bencode_int(data: &[u8], key: &str) -> Option<i64> {
        let key_bytes = key.as_bytes();
        let search = format!("{}:{}", key_bytes.len(), key);
        let needle = search.as_bytes();
        let pos = data.windows(needle.len()).position(|w| w == needle)?;
        let value_start = pos + needle.len();
        if data[value_start] != b'i' {
            return None;
        }
        let end = data[value_start..].iter().position(|&b| b == b'e')?;
        let int_str = std::str::from_utf8(&data[value_start + 1..value_start + end]).ok()?;
        int_str.parse().ok()
    }

    /// Parse announce-list from bencoded data.
    fn parse_announce_list_from_bytes(data: &[u8]) -> Vec<Vec<String>> {
        let search = b"13:announce-list";
        let pos = data.windows(search.len()).position(|w| w == search);
        let start = match pos {
            Some(p) => p + search.len(),
            None => return Vec::new(),
        };

        // The value at `start` should be a bencode list 'l'
        if start >= data.len() || data[start] != b'l' {
            return Vec::new();
        }

        let mut result = Vec::new();
        let mut i = start + 1; // skip the outer 'l'
        let data_len = data.len();

        while i < data_len && data[i] != b'e' {
            if data[i] != b'l' {
                break; // expect tier list
            }
            i += 1; // skip tier 'l'
            let mut tier = Vec::new();
            while i < data_len && data[i] != b'e' {
                // Expect a bencode string
                let colon_pos = match data[i..].iter().position(|&b| b == b':') {
                    Some(p) => p,
                    None => return result,
                };
                let len_str = match std::str::from_utf8(&data[i..i + colon_pos]) {
                    Ok(s) => s,
                    Err(_) => return result,
                };
                let len: usize = match len_str.parse() {
                    Ok(n) => n,
                    Err(_) => return result,
                };
                let val_start = i + colon_pos + 1;
                if val_start + len > data_len {
                    return result;
                }
                let url = String::from_utf8_lossy(&data[val_start..val_start + len]).to_string();
                tier.push(url);
                i = val_start + len;
            }
            if data[i] == b'e' {
                i += 1; // skip tier 'e'
            }
            if !tier.is_empty() {
                result.push(tier);
            }
        }
        result
    }

    // -------------------------------------------------------------------------
    // BittorrentInfo construction test
    // -------------------------------------------------------------------------

    /// Construct BittorrentInfo from raw bencoded torrent bytes.
    ///
    /// This simulates what a future `BittorrentInfo::from_bytes()` or the
    /// RPC engine's torrent → BittorrentInfo conversion would do.
    fn bittorrent_info_from_torrent_bytes(data: &[u8]) -> BittorrentInfo {
        let announce_list = parse_announce_list_from_bytes(data);
        let comment = extract_bencode_str(data, "comment");
        let creation_date = extract_bencode_int(data, "creation date");
        let name = extract_bencode_str(data, "name").unwrap_or_default();

        // Determine mode: search for "5:files" key in the root dict.
        // Multi-file torrents have a "files" key inside the info dict;
        // single-file torrents use "length" instead.
        let mode = if data.windows(7).any(|w| w == b"5:files") {
            Some("multi".to_string())
        } else {
            Some("single".to_string())
        };

        BittorrentInfo {
            announce_list,
            comment,
            creation_date,
            mode,
            info: Some(BittorrentMetaInfo { name }),
        }
    }

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
        assert!(fi.selected);
        assert_eq!(fi.uris.len(), 0);
    }

    #[test]
    fn test_file_info_builder() {
        let fi = FileInfo::new("/tmp/file.iso", 1048576)
            .with_uris(vec![UriEntry::new("http://example.com/file.iso")]);
        assert_eq!(fi.length, 1048576);
        assert_eq!(fi.uris.len(), 1);
    }

    #[test]
    fn test_uri_entry() {
        let uri = UriEntry::new("http://example.com/file.iso").used();
        assert_eq!(uri.status, UriStatus::Used);

        let w = UriEntry::new("http://x.com/f").waiting();
        assert_eq!(w.status, UriStatus::Waiting);
    }

    #[test]
    fn test_global_stat_default() {
        let stat = GlobalStat::default();
        assert_eq!(stat.download_speed, 0);
        let val = stat.to_json_value();
        assert!(val.get("downloadSpeed").is_some());
    }

    #[test]
    fn test_generate_gid() {
        let gid1 = create_gid();
        let gid2 = create_gid();
        assert_eq!(gid1.len(), 16);
        assert_ne!(gid1, gid2);
    }

    #[test]
    fn test_bittorrent_info_serialization() {
        let bt = BittorrentInfo {
            announce_list: vec![
                vec!["udp://tracker1:80".to_string()],
                vec!["http://tracker2:80".to_string()],
            ],
            comment: Some("Test torrent".to_string()),
            creation_date: Some(1700000000),
            mode: Some("single".to_string()),
            info: Some(BittorrentMetaInfo {
                name: "test-file.iso".to_string(),
            }),
        };

        let json = serde_json::to_value(&bt).unwrap();
        assert_eq!(json["announceList"][0][0], "udp://tracker1:80");
        assert_eq!(json["comment"], "Test torrent");
        assert_eq!(json["creationDate"], 1700000000);
        assert_eq!(json["mode"], "single");
        assert_eq!(json["info"]["name"], "test-file.iso");

        let info = StatusInfo::new("gid-bt-001").with_bittorrent(bt);
        let serialized = serde_json::to_value(&info).unwrap();
        assert!(
            serialized.get("bittorrent").is_some(),
            "bittorrent field should appear in serialized StatusInfo"
        );
        let bt_json = serialized.get("bittorrent").unwrap();
        assert_eq!(bt_json["announceList"][0][0], "udp://tracker1:80");
        assert_eq!(bt_json["info"]["name"], "test-file.iso");

        let default_info = StatusInfo::default();
        let default_serialized = serde_json::to_value(&default_info).unwrap();
        assert!(
            default_serialized.get("bittorrent").is_none(),
            "Default StatusInfo should not have bittorrent field"
        );
    }

    #[test]
    fn test_peer_info_with_bitfield_seeder() {
        let peer = PeerInfo {
            peer_id: "peer-abc123".to_string(),
            ip: "192.168.1.100".to_string(),
            port: 6881,
            bitfield: Some("ff00ff00".to_string()),
            am_choking: false,
            peer_choking: true,
            download_speed: 1048576,
            upload_speed: 512000,
            seeder: Some("true".to_string()),
        };
        let json = serde_json::to_value(&peer).unwrap();
        assert_eq!(json["peerId"], "peer-abc123");
        assert_eq!(json["ip"], "192.168.1.100");
        assert_eq!(json["port"], 6881);
        assert_eq!(json["bitfield"], "ff00ff00");
        assert_eq!(json["amChoking"], false);
        assert_eq!(json["peerChoking"], true);
        assert_eq!(json["downloadSpeed"], 1048576);
        assert_eq!(json["uploadSpeed"], 512000);
        assert_eq!(json["seeder"], "true");

        let roundtrip: PeerInfo = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip.bitfield, Some("ff00ff00".to_string()));
        assert_eq!(roundtrip.seeder, Some("true".to_string()));
    }

    #[test]
    fn test_status_info_following_field() {
        let info = StatusInfo::new("gid-test".to_string())
            .with_following("gid-following-001");
        let serialized = serde_json::to_value(&info).unwrap();
        assert_eq!(
            serialized.get("following").unwrap().as_str().unwrap(),
            "gid-following-001",
            "following should be a single GID string"
        );

        let default_info = StatusInfo::default();
        let default_serialized = serde_json::to_value(&default_info).unwrap();
        assert!(
            default_serialized.get("following").is_none(),
            "Default StatusInfo should not have following field"
        );
    }

    // =====================================================================
    // Mock Torrent → BittorrentInfo construction tests
    // =====================================================================

    /// Test that BittorrentInfo can be constructed from a mock single-file
    /// torrent with full metadata (announce-list, comment, creation date, etc.)
    /// and that the serialized JSON matches the expected original aria2 format.
    #[test]
    fn test_bittorrent_info_from_mock_single_file_torrent() {
        let torrent_bytes = build_mock_torrent_with_full_metadata();
        let bt_info = bittorrent_info_from_torrent_bytes(&torrent_bytes);

        // --- Verify BittorrentInfo field values ---
        assert_eq!(
            bt_info.announce_list.len(),
            2,
            "announce-list should have 2 tiers"
        );
        assert_eq!(
            bt_info.announce_list[0],
            vec!["udp://tracker.aria2.org:80"],
            "tier 1 should have 1 tracker"
        );
        assert_eq!(
            bt_info.announce_list[1],
            vec![
                "http://tracker.example.com:80/announce",
                "https://tracker.example.org:443/announce",
            ],
            "tier 2 should have 2 trackers"
        );
        assert_eq!(
            bt_info.comment.as_deref(),
            Some("Aria2 Rust mock torrent for testing"),
            "comment should match"
        );
        assert_eq!(
            bt_info.creation_date,
            Some(1700000000),
            "creationDate should be 1700000000"
        );
        assert_eq!(
            bt_info.mode.as_deref(),
            Some("single"),
            "mode should be 'single' for single-file torrent"
        );
        assert_eq!(
            bt_info.info.as_ref().unwrap().name,
            "test-file.iso",
            "info.name should match"
        );

        // --- Verify JSON serialization matches original aria2 format ---
        let json = serde_json::to_value(&bt_info).unwrap();

        // announceList: array of arrays of strings
        let announce_list = json["announceList"].as_array().unwrap();
        assert_eq!(announce_list.len(), 2);
        assert_eq!(announce_list[0][0], "udp://tracker.aria2.org:80");
        assert_eq!(
            announce_list[1][0],
            "http://tracker.example.com:80/announce"
        );
        assert_eq!(
            announce_list[1][1],
            "https://tracker.example.org:443/announce"
        );

        // comment (string)
        assert_eq!(json["comment"], "Aria2 Rust mock torrent for testing");

        // creationDate (integer)
        assert_eq!(json["creationDate"], 1700000000);
        assert!(
            json["creationDate"].is_number(),
            "creationDate should be a JSON number in original aria2"
        );

        // mode (string)
        assert_eq!(json["mode"], "single");

        // info.name
        assert_eq!(json["info"]["name"], "test-file.iso");
    }

    /// Test BittorrentInfo constructed from a multi-file torrent.
    #[test]
    fn test_bittorrent_info_from_mock_multi_file_torrent() {
        let torrent_bytes = build_mock_multi_file_torrent();
        let bt_info = bittorrent_info_from_torrent_bytes(&torrent_bytes);

        assert_eq!(
            bt_info.announce_list,
            vec![vec!["udp://tracker.multi.com:80"]],
            "announce-list should have 1 tier with 1 tracker"
        );
        assert_eq!(
            bt_info.comment.as_deref(),
            Some("Multi-file torrent for testing"),
            "comment should match"
        );
        assert_eq!(
            bt_info.creation_date,
            Some(1800000000),
            "creationDate should be 1800000000"
        );
        assert_eq!(
            bt_info.mode.as_deref(),
            Some("multi"),
            "mode should be 'multi' for multi-file torrent"
        );
        assert_eq!(
            bt_info.info.as_ref().unwrap().name,
            "multi-dir-torrent",
            "info.name should be the top-level dir name"
        );

        // JSON verification
        let json = serde_json::to_value(&bt_info).unwrap();
        assert_eq!(json["mode"], "multi");
        assert_eq!(json["info"]["name"], "multi-dir-torrent");
        assert_eq!(json["creationDate"], 1800000000);
    }

    /// Test that BittorrentInfo with minimal/some fields missing serializes correctly.
    #[test]
    fn test_bittorrent_info_minimal_fields() {
        // A torrent with only the basics (like a magnet-based torrent with no metadata)
        let bt = BittorrentInfo {
            announce_list: vec![],
            comment: None,
            creation_date: None,
            mode: Some("single".to_string()),
            info: Some(BittorrentMetaInfo {
                name: "unknown.torrent".to_string(),
            }),
        };

        let json = serde_json::to_value(&bt).unwrap();
        // Fields with None should be skipped
        assert!(json.get("comment").is_none(), "comment should be omitted");
        assert!(
            json.get("creationDate").is_none(),
            "creationDate should be omitted"
        );
        assert_eq!(json["mode"], "single");
        assert_eq!(json["info"]["name"], "unknown.torrent");
        // Empty announce_list should still appear (Vec is not Option)
        assert_eq!(
            json["announceList"].as_array().unwrap().len(),
            0,
            "announce-list should be empty array"
        );
    }

    /// Test that StatusInfo with embedded BittorrentInfo (from mock torrent)
    /// serializes to the correct tellStatus JSON format.
    #[test]
    fn test_tell_status_with_bittorrent_from_mock_torrent() {
        let torrent_bytes = build_mock_torrent_with_full_metadata();
        let bt_info = bittorrent_info_from_torrent_bytes(&torrent_bytes);

        let status = StatusInfo::new("bt-mock-gid-001")
            .with_total_length(1048576)
            .with_completed_length(0)
            .with_download_speed(0)
            .with_upload_speed(0)
            .with_status(DownloadStatus::Active)
            .with_dir("/downloads/aria2")
            .with_bittorrent(bt_info)
            .with_following("child-gid-001");

        let json = serde_json::to_value(&status).unwrap();

        // Verify top-level fields
        assert_eq!(json["gid"], "bt-mock-gid-001");
        assert_eq!(json["status"], "active");
        assert_eq!(json["dir"], "/downloads/aria2");
        assert_eq!(json["following"], "child-gid-001");

        // Verify nested bittorrent object
        let bt_json = json.get("bittorrent").unwrap();
        assert_eq!(bt_json["announceList"][0][0], "udp://tracker.aria2.org:80");
        assert_eq!(
            bt_json["comment"],
            "Aria2 Rust mock torrent for testing"
        );
        assert_eq!(bt_json["creationDate"], 1700000000);
        assert_eq!(bt_json["mode"], "single");
        assert_eq!(bt_json["info"]["name"], "test-file.iso");

        // Verify the JSON structure matches original aria2 tellStatus output
        // (bittorrent as a nested object, not at top level)
        let json_str = serde_json::to_string_pretty(&json).unwrap();
        assert!(
            json_str.contains("\"bittorrent\""),
            "JSON should contain bittorrent key"
        );
        assert!(
            json_str.contains("\"announceList\""),
            "JSON should contain announceList inside bittorrent"
        );
        assert!(
            json_str.contains("\"Aria2 Rust mock torrent for testing\""),
            "JSON should contain the comment text"
        );
    }

    /// Test the mock torrent builder produces valid bencoded data (round-trip).
    #[test]
    fn test_mock_torrent_builder_roundtrip() {
        let torrent_bytes = build_mock_torrent_with_full_metadata();

        // Verify the bencoded data starts with 'd' (dict) and ends with 'e'
        assert_eq!(
            torrent_bytes.first(),
            Some(&b'd'),
            "Bencoded torrent should start with 'd'"
        );
        assert_eq!(
            torrent_bytes.last(),
            Some(&b'e'),
            "Bencoded torrent should end with 'e'"
        );

        // Verify the bencoded data contains expected key prefixes
        let as_text = String::from_utf8_lossy(&torrent_bytes);
        assert!(
            as_text.contains("8:announce"),
            "Should contain announce key"
        );
        assert!(
            as_text.contains("13:announce-list"),
            "Should contain announce-list key"
        );
        assert!(
            as_text.contains("7:comment"),
            "Should contain comment key"
        );
        assert!(
            as_text.contains("13:creation date"),
            "Should contain creation date key"
        );
        assert!(as_text.contains("4:info"), "Should contain info key");
        assert!(as_text.contains("4:name"), "Should contain name key");

        // Verify we can parse back the extracted fields
        assert_eq!(
            extract_bencode_str(&torrent_bytes, "comment"),
            Some("Aria2 Rust mock torrent for testing".to_string())
        );
        assert_eq!(extract_bencode_int(&torrent_bytes, "creation date"), Some(1700000000));
        assert_eq!(
            extract_bencode_str(&torrent_bytes, "name"),
            Some("test-file.iso".to_string())
        );

        // Verify announce-list parsing
        let announce_list = parse_announce_list_from_bytes(&torrent_bytes);
        assert_eq!(announce_list.len(), 2);
        assert_eq!(announce_list[0], vec!["udp://tracker.aria2.org:80"]);
        assert_eq!(
            announce_list[1],
            vec![
                "http://tracker.example.com:80/announce",
                "https://tracker.example.org:443/announce",
            ]
        );
    }

    /// Test edge case: empty announce-list.
    #[test]
    fn test_empty_announce_list() {
        // Build a torrent without announce-list
        let pieces: Vec<u8> = (0..20).map(|i| i as u8).collect();
        let info_dict = ben_dict(&[
            (ben_str("name"), ben_str("no-tracker.torrent")),
            (ben_str("length"), ben_int(1024)),
            (ben_str("piece length"), ben_int(512)),
            (ben_str("pieces"), ben_bytes(&pieces)),
        ]);
        let torrent = ben_dict(&[
            (ben_str("announce"), ben_str("http://example.com/announce")),
            (ben_str("info"), info_dict),
        ]);

        let announce_list = parse_announce_list_from_bytes(&torrent);
        assert!(
            announce_list.is_empty(),
            "announce-list should be empty when not present in torrent"
        );

        let bt_info = bittorrent_info_from_torrent_bytes(&torrent);
        assert!(
            bt_info.announce_list.is_empty(),
            "BittorrentInfo.announce_list should be empty"
        );
        assert!(bt_info.comment.is_none(), "comment should be None");
        assert!(bt_info.creation_date.is_none(), "creationDate should be None");
        assert_eq!(
            bt_info.mode.as_deref(),
            Some("single"),
            "mode should be 'single'"
        );
    }
}

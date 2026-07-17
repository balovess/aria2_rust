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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torrent_files: Option<Vec<TorrentFileEntry>>,
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
    pub fn with_torrent_files(mut self, files: Vec<TorrentFileEntry>) -> Self {
        self.torrent_files = Some(files);
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
            index: 0,
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
/// transfer speeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub peer_id: String,
    pub ip: String,
    pub port: u16,
    pub am_choking: bool,
    pub peer_choking: bool,
    pub download_speed: u64,
    pub upload_speed: u64,
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
    pub fn from_env() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            enabled_features: vec![
                "http".to_string(),
                "https".to_string(),
                "ftp".to_string(),
                "bittorrent".to_string(),
                "metalink".to_string(),
                "sftp".to_string(),
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

        let serialized = serde_json::to_value(&info).unwrap();
        assert!(
            serialized.get("torrentFiles").is_some(),
            "torrentFiles should appear in JSON output"
        );
        let tf_arr = serialized.get("torrentFiles").unwrap().as_array().unwrap();
        assert_eq!(tf_arr.len(), 2);
    }

    #[test]
    fn test_peer_info_serialization() {
        let peer = PeerInfo {
            peer_id: "peer-abc123".to_string(),
            ip: "192.168.1.100".to_string(),
            port: 6881,
            am_choking: false,
            peer_choking: true,
            download_speed: 1048576,
            upload_speed: 512000,
        };
        let json = serde_json::to_value(&peer).unwrap();
        assert_eq!(json["peerId"], "peer-abc123");
        assert_eq!(json["ip"], "192.168.1.100");
        assert_eq!(json["port"], 6881);
        assert_eq!(json["amChoking"], false);
        assert_eq!(json["peerChoking"], true);
        assert_eq!(json["downloadSpeed"], 1048576);
        assert_eq!(json["uploadSpeed"], 512000);

        let roundtrip: PeerInfo = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip.port, 6881);
    }
}

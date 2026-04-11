use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use aria2_core::engine::multi_file_layout::TorrentFileEntry;

#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl AuthConfig {
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }
    pub fn with_basic_auth(mut self, user: impl Into<String>, pass: impl Into<String>) -> Self {
        self.username = Some(user.into());
        self.password = Some(pass.into());
        self
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }
    pub fn has_basic(&self) -> bool {
        self.username.is_some() && self.password.is_some()
    }

    pub fn verify_token(&self, provided: &str) -> bool {
        match &self.token {
            None => true,
            Some(t) => t == provided,
        }
    }

    pub fn verify_basic(&self, encoded: &str) -> bool {
        let decoded = base64_decode(encoded).unwrap_or_default();
        if let Some(colon_pos) = decoded.find(':') {
            let (user, pass) = decoded.split_at(colon_pos);
            let pass = &pass[1..];
            match (&self.username, &self.password) {
                (Some(u), Some(p)) => u == user && p == pass,
                _ => false,
            }
        } else {
            false
        }
    }
}

fn base64_decode(s: &str) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

#[derive(Debug, Clone)]
pub struct CorsConfig {
    pub allow_origin: String,
    pub allow_methods: String,
    pub allow_headers: String,
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allow_origin: "*".to_string(),
            allow_methods: "POST, GET, OPTIONS".to_string(),
            allow_headers: "Content-Type, Authorization".to_string(),
            allow_credentials: false,
        }
    }
}

impl CorsConfig {
    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.allow_origin = origin.into();
        self
    }
    pub fn with_credentials(mut self) -> Self {
        self.allow_credentials = true;
        self
    }

    pub fn to_headers(&self) -> Vec<(&str, &str)> {
        vec![
            ("Access-Control-Allow-Origin", &self.allow_origin),
            ("Access-Control-Allow-Methods", &self.allow_methods),
            ("Access-Control-Allow-Headers", &self.allow_headers),
            ("Access-Control-Max-Age", "86400"),
        ]
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub auth: AuthConfig,
    pub cors: CorsConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 6800,
            auth: AuthConfig::default(),
            cors: CorsConfig::default(),
        }
    }
}

impl ServerConfig {
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }
    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.auth = auth;
        self
    }
    pub fn with_cors(mut self, cors: CorsConfig) -> Self {
        self.cors = cors;
        self
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

pub type GlobalOptions = Arc<RwLock<HashMap<String, serde_json::Value>>>;
pub type TaskOptions = Arc<RwLock<HashMap<String, HashMap<String, serde_json::Value>>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum DownloadStatus {
    #[default]
    Active,
    Waiting,
    Paused,
    Error,
    Complete,
    Removed,
}

impl DownloadStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active | Self::Waiting)
    }
    pub fn is_stopped(&self) -> bool {
        !self.is_active() && !matches!(self, Self::Removed)
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Waiting => "waiting",
            Self::Paused => "paused",
            Self::Error => "error",
            Self::Complete => "complete",
            Self::Removed => "removed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub enum UriStatus {
    Used,
    #[default]
    Waiting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub peer_id: String,
    pub ip: String,
    pub port: u16,
    pub am_choking: bool,
    pub peer_choking: bool,
    pub download_speed: u64,
    pub upload_speed: u64,
}

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
    format!("{:016x}", hasher.finish())
}

pub fn create_gid() -> String {
    generate_gid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_config_default() {
        let auth = AuthConfig::default();
        assert!(!auth.has_token());
        assert!(!auth.has_basic());
        assert!(auth.verify_token(""));
    }

    #[test]
    fn test_auth_config_token() {
        let auth = AuthConfig::default().with_token("my-secret");
        assert!(auth.has_token());
        assert!(auth.verify_token("my-secret"));
        assert!(!auth.verify_token("wrong"));
    }

    #[test]
    fn test_auth_config_basic() {
        let auth = AuthConfig::default().with_basic_auth("admin", "pass123");
        assert!(auth.has_basic());
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"admin:pass123");
        assert!(auth.verify_basic(&encoded));
        assert!(
            !auth.verify_basic(
                base64::engine::general_purpose::STANDARD
                    .encode(b"admin:wrong")
                    .as_str()
            )
        );
    }

    #[test]
    fn test_cors_config_default() {
        let cors = CorsConfig::default();
        let headers = cors.to_headers();
        assert!(
            headers
                .iter()
                .any(|(k, _)| k == &"Access-Control-Allow-Origin")
        );
    }

    #[test]
    fn test_server_config_default() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.port, 6800);
        assert_eq!(cfg.addr(), "127.0.0.1:6800");
    }

    #[test]
    fn test_server_config_builder() {
        let cfg = ServerConfig::default()
            .with_port(8080)
            .with_host("0.0.0.0")
            .with_auth(AuthConfig::default().with_token("tok"));
        assert_eq!(cfg.port, 8080);
        assert!(cfg.auth.has_token());
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
        assert_eq!(DownloadStatus::Error.as_str(), "error");
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
            serialized.get("torrent_files").is_some(),
            "torrent_files should appear in JSON output"
        );
        let tf_arr = serialized.get("torrent_files").unwrap().as_array().unwrap();
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
        assert_eq!(json["peer_id"], "peer-abc123");
        assert_eq!(json["ip"], "192.168.1.100");
        assert_eq!(json["port"], 6881);
        assert_eq!(json["am_choking"], false);
        assert_eq!(json["peer_choking"], true);
        assert_eq!(json["download_speed"], 1048576);
        assert_eq!(json["upload_speed"], 512000);

        let roundtrip: PeerInfo = serde_json::from_value(json).unwrap();
        assert_eq!(roundtrip.port, 6881);
    }
}

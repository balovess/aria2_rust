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

// =========================================================================
// RPC Authentication Middleware (G4 Part B)
// =========================================================================

use crate::json_rpc::JsonRpcError;

/// Middleware for token-based RPC authorization.
///
/// Validates that incoming JSON-RPC requests include a valid secret token
/// when `rpc-secret` is configured. An empty/absent secret means auth is
/// disabled and all requests are accepted.
///
/// The token is extracted from the `token` parameter in each request's params.
pub struct RpcAuthMiddleware {
    /// Secret token for authorization. Empty string = no auth required.
    secret: String,
}

impl RpcAuthMiddleware {
    /// Create a new authentication middleware with the given secret.
    ///
    /// # Arguments
    ///
    /// * `secret` - The RPC secret token. Pass empty string to disable auth.
    pub fn new(secret: &str) -> Self {
        Self {
            secret: secret.to_string(),
        }
    }

    /// Validate a JSON-RPC request's token parameter.
    ///
    /// Returns `Ok(())` if authentication passes, `Err(JsonRpcError::Unauthorized)` if it fails.
    ///
    /// # Behavior
    ///
    /// - If no secret is configured (empty string) → always returns `Ok(())`
    /// - If token matches the secret → returns `Ok(())`
    /// - If token is provided but wrong → returns `Err(Unauthorized("Invalid token"))`
    /// - If no token provided but secret is set → returns `Err(Unauthorized("Token required"))`
    pub fn validate(&self, token: Option<&str>) -> Result<(), JsonRpcError> {
        // No auth configured — accept all requests
        if self.secret.is_empty() {
            return Ok(());
        }
        match token {
            Some(t) if t == self.secret => Ok(()),
            Some(_) => Err(JsonRpcError::Unauthorized("Invalid token".to_string())),
            None => Err(JsonRpcError::Unauthorized(
                "Token required (set rpc-secret)".to_string(),
            )),
        }
    }

    /// Returns true if authentication is enabled (non-empty secret).
    pub fn is_auth_enabled(&self) -> bool {
        !self.secret.is_empty()
    }

    /// Returns a reference to the configured secret (for testing/debugging only).
    #[allow(dead_code)]
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

impl Default for RpcAuthMiddleware {
    fn default() -> Self {
        Self::new("")
    }
}

#[derive(Debug, Clone)]
pub struct CorsConfig {
    pub allow_origin: String,
    pub allow_methods: String,
    pub allow_headers: String,
    pub allow_credentials: bool,
    /// Parsed list of allowed origins for efficient lookup
    allowed_origins: Vec<String>,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self::with_allowed_origins(vec!["*".to_string()])
    }
}

impl CorsConfig {
    /// Create a new CorsConfig from a comma-separated list of allowed origins
    ///
    /// Special value "*" allows all origins (wildcard mode).
    /// Multiple origins can be specified as "http://localhost:8080,https://example.com"
    pub fn with_allowed_origins(origins: Vec<String>) -> Self {
        let allow_origin = if origins.len() == 1 && origins[0] == "*" {
            "*".to_string()
        } else {
            origins.join(", ")
        };

        Self {
            allow_origin: allow_origin.clone(),
            allow_methods: "POST, GET, OPTIONS".to_string(),
            allow_headers: "Content-Type, Authorization".to_string(),
            allow_credentials: false,
            allowed_origins: origins,
        }
    }

    /// Create CorsConfig from an option value string (comma-separated origins)
    pub fn from_option_value(value: &str) -> Self {
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed == "*" {
            return Self::default();
        }

        let origins: Vec<String> = if trimmed == "*" {
            vec!["*".to_string()]
        } else {
            trimmed
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };

        Self::with_allowed_origins(origins)
    }

    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.allow_origin = origin.into();
        self
    }

    pub fn with_credentials(mut self) -> Self {
        self.allow_credentials = true;
        self
    }

    /// Check if a given origin is allowed by this CORS configuration
    ///
    /// Returns true if:
    /// - Wildcard mode is enabled ("*" is in allowed_origins)
    /// - The origin exactly matches one of the allowed origins
    /// - No origin header is provided (browser navigation / non-CORS request)
    pub fn allows_origin(&self, origin: Option<&str>) -> bool {
        // Wildcard allows everything
        if self.allowed_origins.contains(&"*".to_string()) {
            return true;
        }

        match origin {
            Some(o) => self.allowed_origins.iter().any(|allowed| allowed == o),
            None => true, // No Origin header = allow (browser navigation)
        }
    }

    /// Generate CORS headers for a response
    ///
    /// Returns None if the origin is not allowed.
    /// Returns Some(headers) with appropriate CORS headers if allowed.
    pub fn headers_for_origin(&self, origin: Option<&str>) -> Option<Vec<(&'static str, String)>> {
        let origin_str = match origin {
            Some(o) if self.allows_origin(Some(o)) => o.to_string(),
            None if self.allows_origin(None) => {
                // In wildcard mode, echo back *; otherwise no header
                if self.allowed_origins.contains(&"*".to_string()) {
                    "*".to_string()
                } else {
                    return Some(vec![]); // Allow but don't set specific origin
                }
            }
            _ => return None, // Origin not allowed
        };

        Some(vec![
            ("Access-Control-Allow-Origin", origin_str),
            ("Access-Control-Allow-Methods", self.allow_methods.clone()),
            ("Access-Control-Allow-Headers", self.allow_headers.clone()),
            ("Access-Control-Max-Age", "86400".to_string()),
        ])
    }

    /// Get headers as static str pairs (for non-origin-specific responses)
    pub fn to_headers(&self) -> Vec<(&str, &str)> {
        vec![
            ("Access-Control-Allow-Origin", &self.allow_origin),
            ("Access-Control-Allow-Methods", &self.allow_methods),
            ("Access-Control-Allow-Headers", &self.allow_headers),
            ("Access-Control-Max-Age", "86400"),
        ]
    }

    /// Handle OPTIONS preflight request - returns true if preflight should be allowed
    pub fn handle_preflight(&self, origin: Option<&str>) -> bool {
        self.allows_origin(origin)
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

// =========================================================================
// L3 RPC Query Method Return Types
// =========================================================================

/// URI information returned by `aria2.getUris`.
///
/// Contains the URI string and its current status (used or waiting).
/// Reuses [`UriEntry`] internally for compatibility.
pub type UriInfo = UriEntry;

/// Server connection information for a specific file index.
///
/// Returned by `aria2.getServers`, grouped by file index in a
/// [`ServerInfoIndex`] wrapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    // =========================================================================
    // RpcAuthMiddleware Tests (G4 Part B)
    // =========================================================================

    #[test]
    fn test_auth_valid_token_passes() {
        let middleware = RpcAuthMiddleware::new("my-secret-token");
        // Valid token should pass
        assert!(middleware.validate(Some("my-secret-token")).is_ok());
    }

    #[test]
    fn test_auth_wrong_token_rejected() {
        let middleware = RpcAuthMiddleware::new("my-secret-token");
        let result = middleware.validate(Some("wrong-token"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), -32001);
        assert!(err.to_string().contains("Invalid token"));
    }

    #[test]
    fn test_auth_no_secret_configured_accepts_all() {
        let middleware = RpcAuthMiddleware::new(""); // Empty secret = no auth
        // All should pass when no secret is configured
        assert!(middleware.validate(None).is_ok());
        assert!(middleware.validate(Some("anything")).is_ok());
        assert!(middleware.validate(Some("")).is_ok());
        assert!(
            !middleware.is_auth_enabled(),
            "Auth should be disabled with empty secret"
        );
    }

    #[test]
    fn test_auth_token_required_when_secret_set() {
        let middleware = RpcAuthMiddleware::new("secret123");
        // No token provided but secret is set → should fail
        let result = middleware.validate(None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), -32001);
        assert!(err.to_string().contains("Token required"));
        assert!(
            middleware.is_auth_enabled(),
            "Auth should be enabled with non-empty secret"
        );
    }

    #[test]
    fn test_auth_middleware_default() {
        let middleware = RpcAuthMiddleware::default();
        assert!(!middleware.is_auth_enabled());
        assert!(middleware.validate(None).is_ok());
        assert!(middleware.validate(Some("x")).is_ok());
    }

    #[test]
    fn test_auth_middleware_secret_accessor() {
        let middleware = RpcAuthMiddleware::new("test-secret");
        assert_eq!(middleware.secret(), "test-secret");
    }

    // =========================================================================
    // CORS Configuration Tests (H6)
    // =========================================================================

    #[test]
    fn test_cors_wildcard_allows_any_origin() {
        let cors = CorsConfig::default(); // Default is wildcard "*"

        // Wildcard should allow any origin
        assert!(cors.allows_origin(Some("http://localhost:8080")));
        assert!(cors.allows_origin(Some("https://example.com")));
        assert!(cors.allows_origin(Some("http://192.168.1.1:3000")));
        assert!(cors.allows_origin(None)); // No origin header
    }

    #[test]
    fn test_cors_specific_domain_blocks_others() {
        let cors = CorsConfig::from_option_value("http://localhost:8080,https://example.com");

        // Allowed origins should pass
        assert!(
            cors.allows_origin(Some("http://localhost:8080")),
            "Should allow exact match for localhost"
        );
        assert!(
            cors.allows_origin(Some("https://example.com")),
            "Should allow exact match for example.com"
        );

        // Non-allowed origins should be blocked
        assert!(
            !cors.allows_origin(Some("http://evil.com")),
            "Should block non-listed origin"
        );
        assert!(
            !cors.allows_origin(Some("http://localhost:8081")),
            "Should block different port"
        );
        assert!(
            !cors.allows_origin(Some("http://localhost:8080/extra")),
            "Should block origin with path (strict matching)"
        );

        // No origin header should still be allowed
        assert!(
            cors.allows_origin(None),
            "No origin header should be allowed"
        );
    }

    #[test]
    fn test_cors_preflight_returns_true_for_allowed() {
        let cors = CorsConfig::from_option_value("http://localhost:8080");

        // Preflight should succeed for allowed origin
        assert!(cors.handle_preflight(Some("http://localhost:8080")));

        // Preflight should fail for disallowed origin
        assert!(!cors.handle_preflight(Some("http://evil.com")));

        // No origin - preflight should succeed
        assert!(cors.handle_preflight(None));
    }

    #[test]
    fn test_cors_from_option_value_parsing() {
        // Test wildcard
        let cors_wildcard = CorsConfig::from_option_value("*");
        assert!(cors_wildcard.allows_origin(Some("anything")));
        assert_eq!(cors_wildcard.allow_origin, "*");

        // Test empty string defaults to wildcard
        let cors_empty = CorsConfig::from_option_value("");
        assert!(cors_empty.allows_origin(Some("anything")));

        // Test multiple origins
        let cors_multi =
            CorsConfig::from_option_value("http://a.com, https://b.com, http://c.com:9090");
        assert!(cors_multi.allows_origin(Some("http://a.com")));
        assert!(cors_multi.allows_origin(Some("https://b.com")));
        assert!(cors_multi.allows_origin(Some("http://c.com:9090")));
        assert!(!cors_multi.allows_origin(Some("http://d.com")));

        // Test with whitespace handling
        let cors_spaces = CorsConfig::from_option_value("  http://a.com , https://b.com  ");
        assert!(cors_spaces.allows_origin(Some("http://a.com")));
        assert!(cors_spaces.allows_origin(Some("https://b.com")));
    }

    #[test]
    fn test_cors_headers_for_origin() {
        let cors = CorsConfig::from_option_value("http://localhost:8080");

        // Allowed origin should produce headers
        let headers = cors.headers_for_origin(Some("http://localhost:8080"));
        assert!(
            headers.is_some(),
            "Should return headers for allowed origin"
        );
        let headers = headers.unwrap();
        assert!(
            headers
                .iter()
                .any(|(k, _)| *k == "Access-Control-Allow-Origin"),
            "Should contain Allow-Origin header"
        );

        // Disallowed origin should return None
        let blocked = cors.headers_for_origin(Some("http://evil.com"));
        assert!(blocked.is_none(), "Should return None for blocked origin");
    }

    #[test]
    fn test_cors_with_allowed_origins_constructor() {
        let cors = CorsConfig::with_allowed_origins(vec![
            "https://api.example.com".to_string(),
            "http://localhost:3000".to_string(),
        ]);

        assert!(cors.allows_origin(Some("https://api.example.com")));
        assert!(cors.allows_origin(Some("http://localhost:3000")));
        assert!(!cors.allows_origin(Some("http://other.com")));
    }
}

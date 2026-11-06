//! SSH connection options and host-key policy.

use std::path::PathBuf;
use std::time::Duration;
use tracing::debug;
/// Host key verification modes for SSH connections
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HostKeyCheckingMode {
    /// Strict host key checking - reject unknown/changed host keys
    #[default]
    Strict,
    /// Accept new host keys automatically but detect changes
    AcceptNew,
    /// Disable all host key verification (insecure, use only for testing)
    Disable,
}

impl std::fmt::Display for HostKeyCheckingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Strict => write!(f, "strict"),
            Self::AcceptNew => write!(f, "accept-new"),
            Self::Disable => write!(f, "disable"),
        }
    }
}

/// Configuration options for establishing an SSH connection
#[derive(Debug, Clone)]
pub struct SshOptions {
    /// Remote hostname or IP address
    pub host: String,
    /// TCP port number (default: 22)
    pub port: u16,
    /// Username for authentication
    pub username: String,
    /// Password for password-based authentication (optional if using key auth)
    pub password: Option<String>,
    /// Path to private key file (optional if using password auth)
    pub private_key_path: Option<String>,
    /// Passphrase for encrypted private keys
    pub private_key_passphrase: Option<String>,
    /// Timeout for TCP connection establishment
    pub connect_timeout: Duration,
    /// Timeout for read operations on the SSH channel
    pub read_timeout: Duration,
    /// Host key verification mode
    pub host_key_mode: HostKeyCheckingMode,
    /// Expected fingerprint in `hashType=digest` form.
    pub host_key_fingerprint: Option<String>,
    /// Compression setting (not all servers support this)
    pub compression: bool,
    /// Keep-alive interval (None to disable)
    pub keepalive_interval: Option<Duration>,
    /// Preferred ciphers (empty = server default)
    pub preferred_ciphers: Vec<String>,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 22,
            username: String::new(),
            password: None,
            private_key_path: None,
            private_key_passphrase: None,
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
            host_key_mode: HostKeyCheckingMode::default(),
            host_key_fingerprint: None,
            compression: false,
            keepalive_interval: Some(Duration::from_secs(60)),
            preferred_ciphers: Vec::new(),
        }
    }
}

impl SshOptions {
    /// Create new SSH options with required fields
    pub fn new(host: &str, username: &str) -> Self {
        Self {
            host: host.to_string(),
            port: 22,
            username: username.to_string(),
            ..Default::default()
        }
    }

    /// Set the port number
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set the password for authentication
    pub fn with_password(mut self, password: &str) -> Self {
        self.password = Some(password.to_string());
        self
    }

    /// Set the path to a private key file for authentication
    pub fn with_private_key(mut self, path: &str) -> Self {
        self.private_key_path = Some(path.to_string());
        self
    }

    /// Set a passphrase for an encrypted private key
    pub fn with_passphrase(mut self, passphrase: &str) -> Self {
        self.private_key_passphrase = Some(passphrase.to_string());
        self
    }

    /// Set the expected host-key fingerprint.
    pub fn with_host_key_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.host_key_fingerprint = Some(fingerprint.into());
        self
    }

    /// Set the host key checking mode
    pub fn with_host_key_mode(mut self, mode: HostKeyCheckingMode) -> Self {
        self.host_key_mode = mode;
        self
    }

    /// Set custom timeouts
    pub fn with_timeouts(mut self, connect: Duration, read: Duration) -> Self {
        self.connect_timeout = connect;
        self.read_timeout = read;
        self
    }

    /// Enable or disable compression
    pub fn with_compression(mut self, enabled: bool) -> Self {
        self.compression = enabled;
        self
    }

    /// Get a human-readable target identifier string
    pub fn target(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }

    /// Check if this configuration has valid authentication credentials
    pub fn has_auth_credentials(&self) -> bool {
        self.password.is_some() || self.private_key_path.is_some()
    }

    /// Resolve the actual private key path, checking common locations
    /// if not explicitly specified. Returns the path to use or None.
    pub fn resolve_key_path(&self) -> Option<PathBuf> {
        if let Some(ref path) = self.private_key_path {
            return Some(PathBuf::from(path));
        }

        // Auto-detect key files in standard locations when no explicit path given
        // Only attempt auto-detection if we have no password auth configured
        if self.password.is_none() {
            let home = dirs::home_dir();
            if let Some(home) = home {
                // Check common key files in order of preference
                for key_name in &["id_ed25519", "id_rsa", "id_ecdsa"] {
                    let candidate = home.join(".ssh").join(key_name);
                    if candidate.exists() {
                        debug!("auto-detected SSH key: {}", candidate.display());
                        return Some(candidate);
                    }
                }
            }
        }

        None
    }
}

// =============================================================================

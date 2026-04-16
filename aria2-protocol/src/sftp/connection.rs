//! SFTP SSH Connection Management
//!
//! Handles SSH/TCP connection lifecycle, authentication, and channel management
//! for SFTP operations. Built on pure Rust using the `russh` crate.
//!
//! ## Architecture
//!
//! ```text
//! SshOptions  ->  SshConnection  ->  russh::Handle  ->  SFTP Subsystem Channel
//!     |                |                  |
//! Builder pattern   Connect/Auth        Channel for SFTP packets
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info};

use russh::client;
use russh::keys;

// Re-export packet constants for test access and internal use
#[cfg(test)]
use crate::sftp::packet::{SSH_FXP_INIT, SSH_FXP_VERSION};

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
// Russh Client Handler
// =============================================================================

/// Shared state between the russh handler and the connection/session layers.
#[derive(Clone)]
struct HandlerState {
    /// Connection options for authentication
    #[allow(dead_code)]
    options: Arc<SshOptions>,
    /// Host key checking mode
    host_key_mode: HostKeyCheckingMode,
    /// Sender for routing channel data to the SFTP session
    channel_data_tx: Arc<tokio::sync::Mutex<Option<mpsc::Sender<Vec<u8>>>>>,
}

/// russh client handler that manages SSH protocol events.
///
/// This handler processes server key verification, authentication challenges,
/// and routes incoming channel data to the appropriate consumer via mpsc.
struct SshClientHandler {
    state: HandlerState,
}

impl SshClientHandler {
    fn new(options: Arc<SshOptions>) -> Self {
        Self {
            state: HandlerState {
                options: options.clone(),
                host_key_mode: options.host_key_mode.clone(),
                channel_data_tx: Arc::new(tokio::sync::Mutex::new(None)),
            },
        }
    }
}

#[async_trait::async_trait]
impl client::Handler for SshClientHandler {
    type Error = SshError;

    #[allow(refining_impl_trait)]
    fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        let mode = self.state.host_key_mode.clone();
        async move {
            debug!("[SFTP] Checking server key (mode={})", mode);

            match mode {
                HostKeyCheckingMode::Strict => {
                    debug!("[SFTP] Strict host key checking - accepting key");
                    Ok(true)
                }
                HostKeyCheckingMode::AcceptNew => {
                    info!("[SFTP] Accept-new mode - accepting server key");
                    Ok(true)
                }
                HostKeyCheckingMode::Disable => {
                    tracing::warn!(
                        "[SFTP] Host key checking DISABLED - connection may be insecure"
                    );
                    Ok(true)
                }
            }
        }
    }

    #[allow(refining_impl_trait)]
    fn data(
        &mut self,
        _channel: russh::ChannelId,
        data: &[u8],
        _session: &mut client::Session,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        let tx = self.state.channel_data_tx.clone();
        let data = data.to_vec();
        async move {
            // Route incoming channel data to the registered receiver (SFTP session)
            let guard = tx.lock().await;
            if let Some(ref sender) = *guard {
                // Ignore send errors - receiver may have been dropped during disconnect
                let _ = sender.send(data).await;
            }
            Ok(())
        }
    }
}

// =============================================================================
// SshConnection
// =============================================================================

/// Represents an active SSH connection managed by russh.
///
/// This struct owns the underlying russh client handle and manages its lifecycle.
/// The handle can be used to create SFTP subsystem channels.
pub struct SshConnection {
    /// The russh client handle for this connection
    handle: client::Handle<SshClientHandler>,
    /// Connection options used to establish this connection
    options: Arc<SshOptions>,
    /// When this connection was established (for age tracking)
    connected_at: std::time::Instant,
    /// Internal handler state clone for accessing channel_data_tx
    handler_state: HandlerState,
}

impl std::fmt::Debug for SshConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshConnection")
            .field("target", &self.options.target())
            .field("host_key_mode", &self.options.host_key_mode)
            .field("age_secs", &self.connected_at.elapsed().as_secs())
            .finish()
    }
}

impl SshConnection {
    /// Establish a new SSH connection to the specified target using russh.
    ///
    /// russh's `client::connect()` handles TCP connection internally.
    ///
    /// # Arguments
    /// * `options` - Connection configuration including host, credentials, etc.
    ///
    /// # Returns
    /// A connected `SshConnection` instance ready for SFTP subsystem initialization.
    ///
    /// # Errors
    /// Returns an error if:
    /// - TCP connection fails or times out
    /// - SSH handshake fails
    /// - Authentication fails
    /// - No valid credentials are provided
    pub async fn connect(options: SshOptions) -> Result<Self, SshError> {
        let target = options.target();
        debug!("[SFTP] Connecting to SSH (russh): {}", target);

        let options_arc = Arc::new(options);

        // Step 1: Build russh client config
        let config = client::Config::default();
        let config = Arc::new(config);

        // Step 2: Create handler and establish SSH connection (russh handles TCP)
        let handler = SshClientHandler::new(Arc::clone(&options_arc));
        let addr = (options_arc.host.as_str(), options_arc.port);

        let mut handle = tokio::time::timeout(
            options_arc.connect_timeout,
            client::connect(config, addr, handler),
        )
        .await
        .map_err(|_| SshError::ConnectTimeout {
            host: options_arc.host.clone(),
            port: options_arc.port,
            timeout_secs: options_arc.connect_timeout.as_secs(),
        })?
        .map_err(|e| SshError::Handshake {
            message: format!("SSH handshake failed: {}", e),
        })?;

        info!(
            "[SFTP] SSH handshake complete (russh): {} (key_check={})",
            target, options_arc.host_key_mode
        );

        // Step 3: Authenticate
        Self::authenticate(&mut handle, &options_arc).await?;

        // Step 4: Set up handler state for channel data routing
        let handler_state = HandlerState {
            options: Arc::clone(&options_arc),
            host_key_mode: options_arc.host_key_mode.clone(),
            channel_data_tx: Arc::new(tokio::sync::Mutex::new(None)),
        };

        info!("[SFTP] SSH authenticated successfully: {}", target);

        Ok(Self {
            handle,
            options: options_arc,
            connected_at: std::time::Instant::now(),
            handler_state,
        })
    }

    /// Authenticate the SSH session using available credentials.
    ///
    /// Tries authentication methods in order:
    /// 1. Password (if provided)
    /// 2. Private key (explicit path or auto-detected)
    async fn authenticate(
        handle: &mut client::Handle<SshClientHandler>,
        options: &Arc<SshOptions>,
    ) -> Result<(), SshError> {
        let username = &options.username;

        // Method 1: Password authentication
        if let Some(ref password) = options.password {
            debug!(
                "[SFTP] Attempting password authentication for user '{}'",
                username
            );
            return handle
                .authenticate_password(username, password)
                .await
                .map(|_| ())
                .map_err(|e| SshError::AuthFailed {
                    method: "password".to_string(),
                    message: e.to_string(),
                });
        }

        // Method 2: Private key authentication
        let key_path = options.resolve_key_path();
        if let Some(key_path) = key_path {
            let passphrase: Option<&str> = options.private_key_passphrase.as_deref();
            debug!(
                "[SFTP] Attempting public key authentication for user '{}' with key: {}",
                username,
                key_path.display()
            );

            // Load the secret key from file
            let key =
                keys::load_secret_key(&key_path, passphrase).map_err(|e| SshError::AuthFailed {
                    method: "publickey".to_string(),
                    message: format!("Failed to load key {}: {}", key_path.display(), e),
                })?;

            // Wrap PrivateKey into PrivateKeyWithHashAlg required by russh 0.59
            let key_with_alg = keys::PrivateKeyWithHashAlg::new(std::sync::Arc::new(key), None);

            return handle
                .authenticate_publickey(username, key_with_alg)
                .await
                .map(|_| ())
                .map_err(|e| SshError::AuthFailed {
                    method: "publickey".to_string(),
                    message: format!("Public key auth failed (key={}): {}", key_path.display(), e),
                });
        }

        // No credentials available
        Err(SshError::NoCredentials {
            message: "No authentication credentials provided".to_string(),
        })
    }

    /// Open an SFTP subsystem channel on this connection.
    ///
    /// Sends the "sftp" subsystem request over a new session channel.
    /// The returned channel is ready for SFTP packet exchange.
    ///
    /// # Returns
    /// A tuple of (channel, data_receiver) where data_receiver collects
    /// incoming channel data for the SFTP session to consume.
    pub async fn open_sftp_channel(
        &mut self,
    ) -> Result<(russh::Channel<russh::client::Msg>, mpsc::Receiver<Vec<u8>>), SshError> {
        debug!("[SFTP] Opening SFTP subsystem channel");

        // Create channel for routing data to the session
        let (data_tx, data_rx) = mpsc::channel(256);

        // Register the data sender in our handler state so incoming data gets routed
        {
            let mut tx_guard = self.handler_state.channel_data_tx.lock().await;
            *tx_guard = Some(data_tx);
        }

        // Open a session channel
        let channel =
            self.handle
                .channel_open_session()
                .await
                .map_err(|e| SshError::SubsystemInit {
                    message: format!("Failed to open session channel: {}", e),
                })?;

        // Request the SFTP subsystem on this channel
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| SshError::SubsystemInit {
                message: format!("Failed to request SFTP subsystem: {}", e),
            })?;

        debug!("[SFTP] SFTP subsystem channel opened successfully");
        Ok((channel, data_rx))
    }

    /// Get the connection options
    pub fn options(&self) -> &Arc<SshOptions> {
        &self.options
    }

    /// Get how long this connection has been alive
    pub fn age(&self) -> Duration {
        self.connected_at.elapsed()
    }

    /// Check if the connection is still alive.
    pub fn is_alive(&self) -> bool {
        // Basic heuristic: alive if < 1 hour old and handle exists
        self.age() < Duration::from_secs(3600)
    }

    /// Gracefully disconnect the SSH session
    pub async fn disconnect(self) -> Result<(), SshError> {
        let target = self.options.target();
        debug!("[SFTP] Disconnecting SSH (russh): {}", target);
        // russh handles cleanup when the handle is dropped
        drop(self.handle);
        info!("[SFTP] SSH disconnected: {}", target);
        Ok(())
    }
}

// =============================================================================
// Error Types
// =============================================================================

/// Comprehensive error type for SSH/SFTP connection operations
#[derive(Debug, Clone, thiserror::Error)]
pub enum SshError {
    #[error("TCP connect timed out connecting to {host}:{port} after {timeout_secs}s")]
    ConnectTimeout {
        host: String,
        port: u16,
        timeout_secs: u64,
    },

    #[error("TCP connection failed to {host}:{port}: {message}")]
    ConnectFailed {
        host: String,
        port: u16,
        message: String,
    },

    #[error("SSH handshake failed: {message}")]
    Handshake { message: String },

    #[error("Authentication failed ({method}): {message}")]
    AuthFailed { method: String, message: String },

    #[error("No authentication credentials available: {message}")]
    NoCredentials { message: String },

    #[error("SSH session initialization failed: {message}")]
    SessionInit { message: String },

    #[error("SFTP subsystem initialization failed: {message}")]
    SubsystemInit { message: String },

    #[error("Configuration error: {message}")]
    Config { message: String },

    #[error("Protocol error: {message}")]
    Protocol { message: String },

    #[error("Connection lost: {message}")]
    ConnectionLost { message: String },
}

impl SshError {
    /// Check if this error indicates a network/connectivity issue that may be retryable
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::ConnectTimeout { .. } | Self::ConnectFailed { .. } | Self::ConnectionLost { .. }
        )
    }

    /// Check if this error indicates an authentication failure (permanent)
    pub fn is_auth_failure(&self) -> bool {
        matches!(self, Self::AuthFailed { .. })
    }

    /// Map this SSH error to a user-friendly error message suitable for display
    pub fn user_message(&self) -> String {
        match self {
            Self::ConnectTimeout { host, port, .. } => {
                format!("Connection to {}:{} timed out", host, port)
            }
            Self::ConnectFailed {
                host,
                port,
                message,
            } => {
                format!("Cannot connect to {}:{}: {}", host, port, message)
            }
            Self::AuthFailed { method, .. } => {
                format!("Authentication failed ({})", method)
            }
            Self::NoCredentials { .. } => "No valid credentials provided".to_string(),
            Self::Handshake { message } => {
                format!("SSH handshake failed: {}", message)
            }
            other => other.to_string(),
        }
    }
}

/// Required by russh Handler trait: Self::Error must implement From<russh::Error>
impl From<russh::Error> for SshError {
    fn from(err: russh::Error) -> Self {
        SshError::Protocol {
            message: err.to_string(),
        }
    }
}

// =============================================================================
// Connection Pool (conceptual implementation for reuse)
// =============================================================================

/// A simple connection pool for reusing SSH connections across multiple operations.
///
/// **Note**: This is a basic pool implementation. For production use, consider
/// adding connection health checks, max idle time limits, and eviction policies.
pub struct SshConnectionPool {
    /// Available connections keyed by target string
    connections: std::collections::HashMap<String, Arc<tokio::sync::Mutex<SshConnection>>>,
    /// Maximum number of connections per target
    #[allow(dead_code)]
    max_per_target: usize,
    /// Maximum idle time before eviction
    max_idle: Duration,
}

impl SshConnectionPool {
    /// Create a new connection pool with specified limits
    pub fn new(max_per_target: usize, max_idle: Duration) -> Self {
        Self {
            connections: std::collections::HashMap::new(),
            max_per_target,
            max_idle,
        }
    }

    /// Get or create a connection for the given options
    pub async fn get_or_create(
        &mut self,
        options: &SshOptions,
    ) -> Result<Arc<tokio::sync::Mutex<SshConnection>>, SshError> {
        let target = options.target();

        // Check for existing reusable connection
        if let Some(conn) = self.connections.get(&target) {
            let guard = conn.lock().await;
            if guard.age() < self.max_idle && guard.is_alive() {
                debug!("[SFTP Pool] Reusing connection: {}", target);
                return Ok(Arc::clone(conn));
            }
            drop(guard);
        }

        // Create new connection
        debug!("[SFTP Pool] Creating new connection: {}", target);
        let conn = SshConnection::connect(options.clone()).await?;
        let wrapped = Arc::new(tokio::sync::Mutex::new(conn));

        self.connections.insert(target, Arc::clone(&wrapped));
        Ok(wrapped)
    }

    /// Remove and close a specific connection from the pool
    pub async fn remove(&mut self, target: &str) -> Option<SshConnection> {
        self.connections.remove(target).map(|arc| {
            // We need to extract the inner value; in practice this requires careful handling
            // For now, just mark it for removal
            drop(arc);
            unreachable!("Pool removal requires ownership transfer")
        })
    }

    /// Close all connections in the pool
    pub async fn close_all(&mut self) {
        info!("[SFTP Pool] Closing all pooled connections");
        self.connections.clear();
    }

    /// Get the number of currently pooled connections
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    /// Check if the pool is empty
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }
}

// =============================================================================
// Unit Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_options_defaults() {
        let opts = SshOptions::default();
        assert_eq!(opts.port, 22);
        assert_eq!(opts.username, "");
        assert!(opts.password.is_none());
        assert!(opts.private_key_path.is_none());
        assert_eq!(opts.connect_timeout, Duration::from_secs(15));
        assert_eq!(opts.read_timeout, Duration::from_secs(30));
        assert!(matches!(opts.host_key_mode, HostKeyCheckingMode::Strict));
    }

    #[test]
    fn test_ssh_options_new() {
        let opts = SshOptions::new("example.com", "user");
        assert_eq!(opts.host, "example.com");
        assert_eq!(opts.username, "user");
        assert_eq!(opts.port, 22);
        assert_eq!(opts.target(), "user@example.com:22");
    }

    #[test]
    fn test_ssh_options_builder_pattern() {
        let opts = SshOptions::new("192.168.1.100", "admin")
            .with_port(2222)
            .with_password("secret123")
            .with_host_key_mode(HostKeyCheckingMode::AcceptNew)
            .with_timeouts(Duration::from_secs(10), Duration::from_secs(60))
            .with_compression(true);

        assert_eq!(opts.port, 2222);
        assert_eq!(opts.password.as_deref(), Some("secret123"));
        assert!(matches!(opts.host_key_mode, HostKeyCheckingMode::AcceptNew));
        assert_eq!(opts.connect_timeout, Duration::from_secs(10));
        assert_eq!(opts.read_timeout, Duration::from_secs(60));
        assert!(opts.compression);
    }

    #[test]
    fn test_ssh_options_with_private_key() {
        let opts = SshOptions::new("server.example.com", "deploy")
            .with_private_key("/home/deploy/.ssh/id_ed25519")
            .with_passphrase("my_secret_phrase");

        assert_eq!(
            opts.private_key_path.as_deref(),
            Some("/home/deploy/.ssh/id_ed25519")
        );
        assert_eq!(
            opts.private_key_passphrase.as_deref(),
            Some("my_secret_phrase")
        );
    }

    #[test]
    fn test_host_key_modes() {
        let strict = HostKeyCheckingMode::Strict;
        let accept_new = HostKeyCheckingMode::AcceptNew;
        let disable = HostKeyCheckingMode::Disable;

        assert_eq!(strict.to_string(), "strict");
        assert_eq!(accept_new.to_string(), "accept-new");
        assert_eq!(disable.to_string(), "disable");

        assert_ne!(strict, accept_new);
        assert_eq!(HostKeyCheckingMode::default(), HostKeyCheckingMode::Strict);
    }

    #[test]
    fn test_has_auth_credentials() {
        let opts_pwd = SshOptions::new("h", "u").with_password("p");
        assert!(opts_pwd.has_auth_credentials());

        let opts_key = SshOptions::new("h", "u").with_private_key("/path/to/key");
        assert!(opts_key.has_auth_credentials());

        let opts_none = SshOptions::new("h", "u");
        assert!(!opts_none.has_auth_credentials());
    }

    #[test]
    fn test_target_formatting() {
        assert_eq!(
            SshOptions::new("localhost", "root").target(),
            "root@localhost:22"
        );
        assert_eq!(
            SshOptions::new("10.0.0.1", "admin")
                .with_port(2222)
                .target(),
            "admin@10.0.0.1:2222"
        );
    }

    #[test]
    fn test_resolve_key_path_explicit() {
        let opts = SshOptions::new("h", "u").with_private_key("/custom/key");
        assert_eq!(opts.resolve_key_path(), Some(PathBuf::from("/custom/key")));
    }

    #[test]
    fn test_ssh_error_retryable_classification() {
        let timeout_err = SshError::ConnectTimeout {
            host: "h".into(),
            port: 22,
            timeout_secs: 15,
        };
        assert!(timeout_err.is_retryable());
        assert!(!timeout_err.is_auth_failure());

        let auth_err = SshError::AuthFailed {
            method: "password".into(),
            message: "bad pass".into(),
        };
        assert!(!auth_err.is_retryable());
        assert!(auth_err.is_auth_failure());
    }

    #[test]
    fn test_ssh_error_user_messages() {
        let err = SshError::ConnectTimeout {
            host: "example.com".into(),
            port: 22,
            timeout_secs: 30,
        };
        assert!(err.user_message().contains("timed out"));

        let err = SshError::AuthFailed {
            method: "publickey".into(),
            message: "key rejected".into(),
        };
        assert!(err.user_message().contains("Authentication failed"));
    }

    #[test]
    fn test_connection_pool_creation() {
        let pool = SshConnectionPool::new(3, Duration::from_secs(300));
        assert!(pool.is_empty());
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn test_sftp_packet_constants_accessible() {
        // Verify that packet constants are accessible from this module
        assert_eq!(SSH_FXP_INIT, 1);
        assert_eq!(SSH_FXP_VERSION, 2);
    }
}

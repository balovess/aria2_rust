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

use md5::Md5;
use russh::client;
use russh::keys;
use russh::keys::ssh_key::HashAlg;
use sha1::Digest as Sha1Digest;
use sha1::Sha1;
use std::sync::Arc;
use tracing::{debug, info};

mod options;
#[cfg(test)]
mod tests;

pub use options::{HostKeyCheckingMode, SshOptions};

#[cfg(test)]
use crate::sftp::packet::{SSH_FXP_INIT, SSH_FXP_VERSION};
fn matches_fingerprint(key: &russh::keys::ssh_key::PublicKey, expected: &str) -> bool {
    let Some((algorithm, digest)) = expected.split_once('=') else {
        return key.fingerprint(HashAlg::Sha256).to_string() == expected;
    };
    let Ok(bytes) = key.to_bytes() else {
        return false;
    };

    let algorithm = algorithm.to_ascii_lowercase();
    let digest = digest.trim();
    match algorithm.as_str() {
        "md5" => hex::encode(Md5::digest(&bytes)).eq_ignore_ascii_case(&digest.replace(':', "")),
        "sha-1" | "sha1" => {
            hex::encode(Sha1::digest(&bytes)).eq_ignore_ascii_case(&digest.replace(':', ""))
        }
        "sha-256" | "sha256" => {
            let actual = key.fingerprint(HashAlg::Sha256).to_string();
            actual.eq_ignore_ascii_case(digest)
                || actual
                    .strip_prefix("SHA256:")
                    .is_some_and(|value| value.eq_ignore_ascii_case(digest))
        }
        "sha-512" | "sha512" => {
            let actual = key.fingerprint(HashAlg::Sha512).to_string();
            actual.eq_ignore_ascii_case(digest)
                || actual
                    .strip_prefix("SHA512:")
                    .is_some_and(|value| value.eq_ignore_ascii_case(digest))
        }
        _ => false,
    }
}

// Russh Client Handler
// =============================================================================

/// russh client handler that manages SSH protocol events.
///
/// The SFTP session reads its own `russh::Channel`, so this handler only owns
/// connection-level verification behavior.
struct SshClientHandler {
    options: Arc<SshOptions>,
}

impl SshClientHandler {
    fn new(options: Arc<SshOptions>) -> Self {
        Self { options }
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
        let mode = self.options.host_key_mode.clone();
        let expected = self.options.host_key_fingerprint.clone();
        let server_key = _server_public_key.clone();
        async move {
            debug!("[SFTP] Checking server key (mode={})", mode);
            if let Some(expected) = expected {
                if !matches_fingerprint(&server_key, &expected) {
                    return Err(SshError::Handshake {
                        message: format!("SSH host-key fingerprint mismatch: expected {expected}"),
                    });
                }
                return Ok(true);
            }

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
}

impl std::fmt::Debug for SshConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshConnection")
            .field("target", &self.options.target())
            .field("host_key_mode", &self.options.host_key_mode)
            .finish()
    }
}

impl SshConnection {
    /// Establish a new SSH connection to the specified target using russh.
    ///
    /// The default direct-routing SFTP entry point.
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
        let stream = tokio::time::timeout(
            options.connect_timeout,
            tokio::net::TcpStream::connect((options.host.as_str(), options.port)),
        )
        .await
        .map_err(|_| SshError::ConnectTimeout {
            host: options.host.clone(),
            port: options.port,
            timeout_secs: options.connect_timeout.as_secs(),
        })?
        .map_err(|error| SshError::Handshake {
            message: format!("SSH TCP connection failed: {error}"),
        })?;
        Self::connect_with_stream(options, stream).await
    }

    /// Complete the SSH handshake over a TCP stream created by the caller.
    ///
    /// Keeping socket creation outside this protocol crate lets the engine
    /// apply its process-wide outbound policy without exposing that policy in
    /// the public SFTP option structure.
    pub async fn connect_with_stream(
        options: SshOptions,
        stream: tokio::net::TcpStream,
    ) -> Result<Self, SshError> {
        let target = options.target();
        debug!("[SFTP] Connecting to SSH (russh): {}", target);

        let options_arc = Arc::new(options);

        // Step 1: Build russh client config
        let config = client::Config::default();
        let config = Arc::new(config);

        // Step 2: Create handler and establish SSH connection over the supplied TCP stream.
        let handler = SshClientHandler::new(Arc::clone(&options_arc));

        let mut handle = tokio::time::timeout(
            options_arc.connect_timeout,
            client::connect_stream(config, stream, handler),
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

        info!("[SFTP] SSH authenticated successfully: {}", target);

        Ok(Self {
            handle,
            options: options_arc,
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
            let result = handle
                .authenticate_password(username, password)
                .await
                .map_err(|e| SshError::AuthFailed {
                    method: "password".to_string(),
                    message: e.to_string(),
                })?;
            return Self::require_successful_authentication(result, "password");
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

            let result = handle
                .authenticate_publickey(username, key_with_alg)
                .await
                .map_err(|e| SshError::AuthFailed {
                    method: "publickey".to_string(),
                    message: format!("Public key auth failed (key={}): {}", key_path.display(), e),
                })?;
            return Self::require_successful_authentication(result, "publickey");
        }

        // No credentials available
        Err(SshError::NoCredentials {
            message: "No authentication credentials provided".to_string(),
        })
    }

    /// Translate russh's protocol-level result into the command-level contract.
    ///
    /// A rejected credential is represented by `Ok(AuthResult::Failure { .. })`
    /// by russh, not an I/O error. Treating that value as success would allow a
    /// client to proceed after the server has declined authentication.
    fn require_successful_authentication(
        result: client::AuthResult,
        method: &str,
    ) -> Result<(), SshError> {
        if result.success() {
            return Ok(());
        }

        Err(SshError::AuthFailed {
            method: method.to_string(),
            message: "server rejected credentials".to_string(),
        })
    }

    /// Open an SFTP subsystem channel on this connection.
    ///
    /// Sends the "sftp" subsystem request over a new session channel.
    /// The returned channel is ready for SFTP packet exchange.
    ///
    /// # Returns
    /// The channel owns both the send and receive halves, so callers can use
    /// its `ChannelStream` without a handler-level forwarding adapter.
    pub async fn open_sftp_channel(
        &mut self,
    ) -> Result<russh::Channel<russh::client::Msg>, SshError> {
        debug!("[SFTP] Opening SFTP subsystem channel");

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
        Ok(channel)
    }

    /// Get the connection options
    pub fn options(&self) -> &Arc<SshOptions> {
        &self.options
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

/// Errors produced while opening or managing an SSH connection for SFTP.
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
    /// Check if this error indicates a network or connectivity issue that may be retryable.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::ConnectTimeout { .. } | Self::ConnectFailed { .. } | Self::ConnectionLost { .. }
        )
    }

    /// Check if this error indicates a permanent authentication failure.
    pub fn is_auth_failure(&self) -> bool {
        matches!(self, Self::AuthFailed { .. })
    }

    /// Map this SSH error to a user-friendly message suitable for display.
    pub fn user_message(&self) -> String {
        match self {
            Self::ConnectTimeout { host, port, .. } => {
                format!("Connection to {host}:{port} timed out")
            }
            Self::ConnectFailed {
                host,
                port,
                message,
            } => format!("Cannot connect to {host}:{port}: {message}"),
            Self::AuthFailed { method, .. } => format!("Authentication failed ({method})"),
            Self::NoCredentials { .. } => "No valid credentials provided".to_string(),
            Self::Handshake { message } => format!("SSH handshake failed: {message}"),
            other => other.to_string(),
        }
    }
}

/// Required by russh's handler trait: the handler error must convert from russh errors.
impl From<russh::Error> for SshError {
    fn from(error: russh::Error) -> Self {
        Self::Protocol {
            message: error.to_string(),
        }
    }
}

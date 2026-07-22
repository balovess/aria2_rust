//! HTTP connection configuration and state types

use std::time::Duration;

/// HTTP connection configuration
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Maximum concurrent connections
    pub max_connections: usize,
    /// TCP connection timeout
    pub connect_timeout: Duration,
    /// Read timeout
    pub read_timeout: Duration,
    /// Write timeout
    pub write_timeout: Duration,
    /// Idle connection timeout (LRU eviction)
    pub idle_timeout: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            max_connections: crate::constants::HTTP_CONFIG_DEFAULT_MAX_CONNECTIONS,
            connect_timeout: Duration::from_secs(
                crate::constants::HTTP_CONFIG_DEFAULT_CONNECT_TIMEOUT_SECS,
            ),
            read_timeout: Duration::from_secs(
                crate::constants::HTTP_CONFIG_DEFAULT_READ_TIMEOUT_SECS,
            ),
            write_timeout: Duration::from_secs(
                crate::constants::HTTP_CONFIG_DEFAULT_WRITE_TIMEOUT_SECS,
            ),
            idle_timeout: Duration::from_secs(
                crate::constants::HTTP_CONFIG_DEFAULT_IDLE_TIMEOUT_SECS,
            ),
        }
    }
}

/// HTTP connection state
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    /// Idle and available
    Idle,
    /// Currently in use
    InUse,
    /// Closed
    Closed,
}

// Re-export HttpResponse for use in connection module
pub use aria2_protocol::http::response::HttpResponse;

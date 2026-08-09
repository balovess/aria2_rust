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
    /// Idle connection timeout (LRU eviction).
    /// Connections idle longer than this are evicted by `check_timeout()`.
    /// Mirrors C++ `SocketPoolEntry::timeout_`.
    pub idle_timeout: Duration,
    /// Maximum idle connections per (host, port, proxy) key.
    /// When `put_back()` would exceed this limit, the oldest idle connection
    /// for that key is evicted first (LRU). 0 = unlimited.
    /// Mirrors C++ per-key pool sizing; default matches
    /// `HTTP_DEFAULT_POOL_MAX_IDLE_PER_HOST`.
    pub max_idle_per_host: usize,
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
            max_idle_per_host: crate::constants::HTTP_DEFAULT_POOL_MAX_IDLE_PER_HOST,
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

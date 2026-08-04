//! Active connection management with timeout-controlled I/O

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::error::{Aria2Error, RecoverableError, Result};

use crate::network::ConnectionContext;

/// Proxy configuration for a pooled connection.
///
/// Matches the C++ `createProxyRequest()` concept: connections through
/// different proxies cannot be reused for direct or different-proxy requests.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProxyInfo {
    /// Proxy hostname
    pub host: String,
    /// Proxy port
    pub port: u16,
}

/// A key that uniquely identifies a reusable connection in the pool.
///
/// Two connections are interchangeable only when they share the same
/// `ConnectionPoolKey`.  This mirrors the C++ `poolSocket(request,
/// proxyRequest, socket)` call where both the target *and* the proxy
/// are part of the pool identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConnectionPoolKey {
    /// Target host:port (e.g. "cdn.example.com:443")
    pub target: String,
    /// None = direct connection; Some = connection through this proxy
    pub proxy: Option<ProxyInfo>,
}

/// Active connection information
#[derive(Debug)]
pub struct ActiveConnection {
    /// Unique connection ID
    pub id: u64,
    /// TCP stream
    pub stream: TcpStream,
    /// Target host (host:port)
    pub host: String,
    /// Concrete origin connection selected by DNS.
    pub connection: ConnectionContext,
    /// Last used timestamp (updated on every I/O and on pool re-entry)
    pub last_used: Instant,
    /// Timestamp when this connection was placed into the idle pool.
    /// `None` while the connection is in active use.
    /// Mirrors C++ `SocketPoolEntry::registeredTime_`.
    pub pooled_at: Option<Instant>,
    /// Pool key (target + proxy identity)
    pub pool_key: ConnectionPoolKey,
}

impl ActiveConnection {
    /// Check if the connection is still valid for reuse.
    ///
    /// Uses a non-blocking probe via `peer_addr()` to catch clearly
    /// broken sockets.  For a more thorough check (detecting half-closed
    /// connections where the peer sent FIN), the caller should attempt
    /// a `read_with_timeout` with a zero-length deadline after acquiring
    /// from the pool — matching the C++ `socket->isReadable(0)` pattern.
    pub fn is_valid(&self) -> bool {
        self.stream.peer_addr().is_ok()
    }

    /// Update last used time
    pub fn touch(&mut self) {
        self.last_used = Instant::now();
    }

    /// Mark this connection as idle in the pool.
    /// Sets `pooled_at` to now and touches `last_used`.
    /// Mirrors C++ `poolSocket()` which records `registeredTime_`.
    pub fn mark_pooled(&mut self) {
        let now = Instant::now();
        self.last_used = now;
        self.pooled_at = Some(now);
    }

    /// Mark this connection as actively in use (removed from pool).
    /// Clears `pooled_at`.
    pub fn mark_in_use(&mut self) {
        self.pooled_at = None;
    }

    /// Check if this connection has been idle longer than the given timeout.
    /// Uses `pooled_at` (the pool entry time) for the check, matching
    /// C++ `SocketPoolEntry::isTimeout()`.
    pub fn is_idle_timeout(&self, timeout: Duration) -> bool {
        match self.pooled_at {
            Some(t) => t.elapsed() >= timeout,
            None => false,
        }
    }

    /// Asynchronous read with timeout control
    ///
    /// Reads data from the TCP stream into the buffer, subject to read_timeout.
    /// Used for reading HTTP response headers and body.
    pub async fn read_with_timeout(
        &mut self,
        buf: &mut [u8],
        read_timeout: Duration,
    ) -> Result<usize> {
        timeout(read_timeout, self.stream.read(buf))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Read data failed: {}", e),
                })
            })
    }

    /// Asynchronous write with timeout control
    ///
    /// Writes data to the TCP stream, subject to write_timeout.
    /// Used for sending HTTP request headers and body.
    pub async fn write_with_timeout(
        &mut self,
        buf: &[u8],
        write_timeout: Duration,
    ) -> Result<usize> {
        timeout(write_timeout, self.stream.write(buf))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Write data failed: {}", e),
                })
            })
    }

    /// Flush write buffer with timeout control
    pub async fn flush_with_timeout(&mut self, write_timeout: Duration) -> Result<()> {
        timeout(write_timeout, self.stream.flush())
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Flush buffer failed: {}", e),
                })
            })
    }

    /// Close the connection (bidirectional shutdown)
    pub async fn shutdown(&mut self) -> Result<()> {
        match self.stream.shutdown().await {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::debug!("Failed to close connection: id={}, error={}", self.id, e);
                Ok(())
            }
        }
    }

    /// Return the logical and concrete identity of this connection.
    pub fn connection_context(&self) -> &ConnectionContext {
        &self.connection
    }

    /// Whether this connection is routed through a proxy.
    pub fn is_proxied(&self) -> bool {
        self.pool_key.proxy.is_some()
    }

    /// Get peer address
    pub fn peer_addr(&self) -> Result<SocketAddr> {
        self.stream.peer_addr().map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Failed to get peer address: {}", e),
            })
        })
    }

    /// Get local address
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.stream.local_addr().map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Failed to get local address: {}", e),
            })
        })
    }
}

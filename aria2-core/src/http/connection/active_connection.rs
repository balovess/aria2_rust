//! Active connection management with timeout-controlled I/O

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::error::{Aria2Error, RecoverableError, Result};

/// Active connection information
#[derive(Debug)]
pub struct ActiveConnection {
    /// Unique connection ID
    pub id: u64,
    /// TCP stream
    pub stream: TcpStream,
    /// Target host
    pub host: String,
    /// Last used timestamp
    pub last_used: Instant,
}

impl ActiveConnection {
    /// Check if the connection is still valid
    pub fn is_valid(&self) -> bool {
        // Check if the connection has been closed or errored
        self.stream.peer_addr().is_ok()
    }

    /// Update last used time
    pub fn touch(&mut self) {
        self.last_used = Instant::now();
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

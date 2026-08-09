//! I/O helpers for proxy connections.
//!
//! Provides timeout-governed read/write/connect primitives used by
//! both CONNECT tunnel and forward proxy modes.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info};

use crate::error::{Aria2Error, Result};
use crate::http::header_processor::{HttpHeaderProcessor, HttpResponseHead};

use super::config::HttpProxyConfig;

/// Buffer size for reading proxy response headers.
pub(crate) const READ_BUF_SIZE: usize = 4096;

/// Maximum number of auth retry attempts (prevent infinite loops on bad creds).
pub(crate) const MAX_AUTH_RETRIES: u32 = 2;

/// Read the proxy's HTTP response head using the streaming [HttpHeaderProcessor].
///
/// Applies the config's read_timeout to each read operation.
pub(crate) async fn read_proxy_response(
    stream: &mut TcpStream,
    read_timeout: Duration,
) -> Result<HttpResponseHead> {
    let mut processor = HttpHeaderProcessor::new();
    let mut buf = [0u8; READ_BUF_SIZE];

    loop {
        let n = tokio::time::timeout(read_timeout, stream.read(&mut buf))
            .await
            .map_err(|_| Aria2Error::Network("Timeout reading proxy response".to_string()))?
            .map_err(|e| Aria2Error::Network(format!("Error reading proxy response: {}", e)))?;

        if n == 0 {
            return Err(Aria2Error::Network(
                "Connection closed by proxy before response complete".to_string(),
            ));
        }

        let state = processor.feed(&buf[..n]);
        if state.is_complete() {
            return processor.get_result();
        }
        if state.is_error() {
            return Err(Aria2Error::Parse(format!(
                "Error parsing proxy response: {}",
                state.is_error()
            )));
        }
    }
}

/// Write all bytes to the stream with a timeout.
pub(crate) async fn write_all_timeout(
    stream: &mut TcpStream,
    data: &[u8],
    write_timeout: Duration,
) -> Result<()> {
    tokio::time::timeout(write_timeout, stream.write_all(data))
        .await
        .map_err(|_| Aria2Error::Network("Timeout writing to proxy".to_string()))?
        .map_err(|e| Aria2Error::Network(format!("Error writing to proxy: {}", e)))?;
    Ok(())
}

/// Connect to the proxy TCP endpoint with a timeout.
pub(crate) async fn connect_to_proxy(config: &HttpProxyConfig) -> Result<TcpStream> {
    let addr = format!("{}:{}", config.proxy_host, config.proxy_port);
    debug!("Connecting to proxy at {}", addr);

    let stream = tokio::time::timeout(config.connect_timeout, TcpStream::connect(&addr))
        .await
        .map_err(|_| {
            Aria2Error::Network(format!(
                "Timeout connecting to proxy {} ({}s)",
                addr,
                config.connect_timeout.as_secs()
            ))
        })?
        .map_err(|e| {
            Aria2Error::Network(format!("Failed to connect to proxy '{}': {}", addr, e))
        })?;

    info!("Connected to proxy at {}", addr);
    Ok(stream)
}

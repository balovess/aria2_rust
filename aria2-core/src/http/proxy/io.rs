//! I/O helpers for proxy connections.
//!
//! Provides timeout-governed read/write/connect primitives used by
//! both CONNECT tunnel and forward proxy modes.

use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{debug, info};

use crate::error::{Aria2Error, Result};
use crate::http::header_processor::HttpResponseHead;
use crate::network::OutboundNetworkPolicy;

use super::config::HttpProxyConfig;

/// Maximum number of auth retry attempts (prevent infinite loops on bad creds).
pub(crate) const MAX_AUTH_RETRIES: u32 = 2;

/// Read a proxy response head through the protocol parser while leaving any
/// response body bytes unread on the stream.
///
/// Applies the config's read timeout to each transport read.
pub(crate) async fn read_proxy_response(
    stream: &mut TcpStream,
    read_timeout: Duration,
) -> Result<HttpResponseHead> {
    aria2_protocol::http::header_processor::read_response_head(stream, read_timeout)
        .await
        .map_err(|error| match error {
            aria2_protocol::http::header_processor::HttpResponseHeadReadError::Timeout => {
                Aria2Error::Network("Timeout reading proxy response".to_string())
            }
            aria2_protocol::http::header_processor::HttpResponseHeadReadError::Io(error)
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                Aria2Error::Network(
                    "Connection closed by proxy before response complete".to_string(),
                )
            }
            aria2_protocol::http::header_processor::HttpResponseHeadReadError::Io(error) => {
                Aria2Error::Network(format!("Error reading proxy response: {error}"))
            }
            aria2_protocol::http::header_processor::HttpResponseHeadReadError::HeaderTooLarge => {
                Aria2Error::Parse(
                    "Error parsing proxy response: Too large HTTP header block".into(),
                )
            }
            aria2_protocol::http::header_processor::HttpResponseHeadReadError::InvalidResponse(
                message,
            ) => Aria2Error::HttpProtocol(message),
        })
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

/// Connect to a proxy through the shared outbound source policy.
pub(crate) async fn connect_to_proxy_with_policy(
    config: &HttpProxyConfig,
    policy: &OutboundNetworkPolicy,
) -> Result<TcpStream> {
    let addr = format!("{}:{}", config.proxy_host, config.proxy_port);
    debug!("Connecting to proxy at {} through outbound policy", addr);
    let stream = tokio::time::timeout(
        config.connect_timeout,
        policy.connect_host(&config.proxy_host, config.proxy_port),
    )
    .await
    .map_err(|_| {
        Aria2Error::Network(format!(
            "Timeout connecting to proxy {} ({}s)",
            addr,
            config.connect_timeout.as_secs()
        ))
    })?
    .map_err(|e| Aria2Error::Network(format!("Failed to connect to proxy '{}': {}", addr, e)))?;
    info!("Connected to proxy at {}", addr);
    Ok(stream)
}

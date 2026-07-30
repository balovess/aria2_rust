//! HTTP proxy tunneling module
//!
//! Implements HTTP-over-HTTP-proxy connections, supporting both CONNECT
//! tunneling (for HTTPS/FTP) and forward proxy mode (absolute URI for
//! plain HTTP). Collapses the C++ command chain:
//!
//! - `AbstractProxyRequestCommand` (sends proxy request)
//! - `AbstractProxyResponseCommand` (validates proxy response, non-200 = error)
//! - `HttpProxyRequestCommand` (builds CONNECT or forward request)
//! - `HttpProxyResponseCommand` (reads response, proceeds to next command)
//!
//! into a single async function using Rust's async/await model.
//!
//! # Proxy Modes
//!
//! - **Tunnel**: `CONNECT host:port HTTP/1.1` — transparent TCP tunnel
//!   for HTTPS and FTP-over-proxy connections.
//! - **Forward**: Absolute-URI forwarding — `GET http://target/path HTTP/1.1`
//!   for plain HTTP through a proxy.

pub mod auth;
pub mod connect;

#[cfg(test)]
mod tests;

use std::time::Duration;

use tokio::net::TcpStream;

use crate::error::Result;

use connect::HttpProxyTunnel;

// ---------------------------------------------------------------------------
// HttpProxyType
// ---------------------------------------------------------------------------

/// The type of proxy connection that was established.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HttpProxyType {
    /// CONNECT tunnel -- the proxy relays raw bytes (HTTPS, FTP).
    Tunnel,
    /// Forward -- the proxy sees absolute-URI requests (plain HTTP).
    Forward,
}

// ---------------------------------------------------------------------------
// HttpProxyTunnelConfig
// ---------------------------------------------------------------------------

/// Configuration for establishing an HTTP proxy tunnel or forward connection.
#[derive(Debug, Clone)]
pub struct HttpProxyTunnelConfig {
    /// Proxy server hostname
    pub proxy_host: String,
    /// Proxy server port
    pub proxy_port: u16,
    /// Target server hostname
    pub target_host: String,
    /// Target server port
    pub target_port: u16,
    /// Proxy authentication username (None = no auth)
    pub username: Option<String>,
    /// Proxy authentication password
    pub password: Option<String>,
    /// Connection timeout for connecting to the proxy
    pub connect_timeout: Duration,
    /// Read timeout for proxy response
    pub read_timeout: Duration,
    /// Write timeout for sending requests
    pub write_timeout: Duration,
    /// User-Agent header value
    pub user_agent: String,
}

impl Default for HttpProxyTunnelConfig {
    fn default() -> Self {
        Self {
            proxy_host: String::new(),
            proxy_port: 8080,
            target_host: String::new(),
            target_port: 80,
            username: None,
            password: None,
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(30),
            user_agent: "aria2/rust".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// HttpProxyTunnelResult
// ---------------------------------------------------------------------------

/// Result of successfully establishing a proxy connection.
#[derive(Debug)]
pub struct HttpProxyTunnelResult {
    /// The TCP stream (tunneled or connected through the proxy)
    pub stream: TcpStream,
    /// The type of proxy connection established
    pub proxy_type: HttpProxyType,
}

// ---------------------------------------------------------------------------
// Top-level entry point
// ---------------------------------------------------------------------------

/// Establish an HTTP proxy connection (tunnel or forward).
///
/// This is the primary entry point, equivalent to the C++ command chain:
/// `AbstractProxyRequestCommand` + `AbstractProxyResponseCommand` +
/// `HttpProxyRequestCommand` + `HttpProxyResponseCommand`.
///
/// - For tunnel mode, sends `CONNECT host:port HTTP/1.1` and validates the
///   proxy response. On success the `TcpStream` is a transparent tunnel.
/// - For forward mode, connects to the proxy and returns the `TcpStream`
///   for the caller to send the absolute-URI request through.
/// - Handles 407 Proxy Authentication Required with Basic/Digest retry.
///
/// # Errors
///
/// - `RecoverableError::Timeout` on connection or read timeout
/// - `RecoverableError::TemporaryNetworkFailure` on proxy rejection,
///   authentication failure, or I/O errors
pub async fn establish_http_proxy_tunnel(
    config: &HttpProxyTunnelConfig,
    proxy_type: HttpProxyType,
) -> Result<HttpProxyTunnelResult> {
    match proxy_type {
        HttpProxyType::Tunnel => {
            let stream = HttpProxyTunnel::establish_tunnel(config).await?;
            Ok(HttpProxyTunnelResult {
                stream,
                proxy_type: HttpProxyType::Tunnel,
            })
        }
        HttpProxyType::Forward => {
            let stream = HttpProxyTunnel::establish_forward(config).await?;
            Ok(HttpProxyTunnelResult {
                stream,
                proxy_type: HttpProxyType::Forward,
            })
        }
    }
}

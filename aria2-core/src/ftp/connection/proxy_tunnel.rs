//! FTP-over-HTTP-proxy CONNECT tunnel establishment
//!
//! Implements the HTTP CONNECT method for tunneling FTP through an
//! HTTP proxy. The flow is:
//!
//! 1. Connect to the HTTP proxy server
//! 2. Send `CONNECT host:port HTTP/1.1` request
//! 3. Optionally handle 407 Proxy Authentication Required
//! 4. On 200 Connection Established, the socket becomes a tunnel
//! 5. Normal FTP negotiation proceeds over the tunneled connection
//!
//! This is equivalent to the C++ `FtpTunnelRequestCommand` +
//! `FtpTunnelResponseCommand` + `AbstractProxyRequestCommand` +
//! `AbstractProxyResponseCommand` chain, collapsed into a single
//! async function using Rust's async/await model.

use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::proxy_tunnel::HttpProxyTunnelConfig;
use crate::http::proxy_tunnel::connect::HttpProxyTunnel;
use crate::network::OutboundNetworkPolicy;

/// Configuration for establishing an FTP-over-HTTP-proxy tunnel.
#[derive(Debug, Clone)]
pub struct FtpProxyTunnelConfig {
    /// Proxy server hostname
    pub proxy_host: String,
    /// Proxy server port
    pub proxy_port: u16,
    /// Target FTP server hostname
    pub target_host: String,
    /// Target FTP server port
    pub target_port: u16,
    /// Proxy authentication username (empty if no auth)
    pub proxy_username: String,
    /// Proxy authentication password (empty if no auth)
    pub proxy_password: String,
    /// Connection timeout for connecting to the proxy
    pub connect_timeout: Duration,
    /// Read timeout for proxy response
    pub read_timeout: Duration,
    /// User-Agent header to send
    pub user_agent: String,
}

impl Default for FtpProxyTunnelConfig {
    fn default() -> Self {
        Self {
            proxy_host: String::new(),
            proxy_port: 8080,
            target_host: String::new(),
            target_port: 21,
            proxy_username: String::new(),
            proxy_password: String::new(),
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            user_agent: crate::constants::USER_AGENT.to_string(),
        }
    }
}

/// Result of a successful proxy tunnel establishment.
pub struct FtpProxyTunnelResult {
    /// The tunneled TCP stream (now behaves as a direct connection to the FTP server)
    pub stream: TcpStream,
}

/// FTP proxy tunnel handler.
///
/// Establishes an HTTP CONNECT tunnel through a proxy server for
/// FTP protocol. After the tunnel is established, the returned
/// `TcpStream` can be used for normal FTP negotiation.
///
/// # C++ Equivalent
///
/// This replaces the C++ command chain:
/// - `FtpTunnelRequestCommand` (sends CONNECT request)
/// - `AbstractProxyRequestCommand` (handles proxy auth)
/// - `FtpTunnelResponseCommand` (reads 200 response)
/// - `AbstractProxyResponseCommand` (validates response)
pub struct FtpProxyTunnel;

impl FtpProxyTunnel {
    /// Establish an FTP-over-HTTP-proxy tunnel using the CONNECT method.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for the proxy tunnel
    ///
    /// # Returns
    ///
    /// A `TcpStream` that is tunneled through the proxy to the target
    /// FTP server. This stream can be used directly for FTP negotiation.
    ///
    /// # Errors
    ///
    /// - If the proxy connection fails
    /// - If the proxy rejects the CONNECT request (non-200 response)
    /// - If proxy authentication fails after retry
    /// - If timeout occurs
    pub async fn establish(config: &FtpProxyTunnelConfig) -> Result<TcpStream> {
        Self::establish_with_policy(config, &OutboundNetworkPolicy::direct()).await
    }

    /// Establish a tunnel through the shared outbound policy.
    pub async fn establish_with_policy(
        config: &FtpProxyTunnelConfig,
        policy: &OutboundNetworkPolicy,
    ) -> Result<TcpStream> {
        // Step 1: Connect to the proxy server
        debug!(
            "Connecting to HTTP proxy {}:{} for FTP tunnel to {}:{}",
            config.proxy_host, config.proxy_port, config.target_host, config.target_port
        );

        let connect = async {
            if policy.is_direct() {
                TcpStream::connect((config.proxy_host.as_str(), config.proxy_port))
                    .await
                    .map_err(|error| Aria2Error::Network(error.to_string()))
            } else {
                policy
                    .connect_host(&config.proxy_host, config.proxy_port)
                    .await
                    .map_err(|error| Aria2Error::Network(error.to_string()))
            }
        };
        let stream = timeout(config.connect_timeout, connect)
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e: Aria2Error| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Failed to connect to proxy {}:{}: {}",
                        config.proxy_host, config.proxy_port, e
                    ),
                })
            })?;

        let http_config = HttpProxyTunnelConfig {
            proxy_host: config.proxy_host.clone(),
            proxy_port: config.proxy_port,
            target_host: config.target_host.clone(),
            target_port: config.target_port,
            username: (!config.proxy_username.is_empty()).then(|| config.proxy_username.clone()),
            password: Some(config.proxy_password.clone()),
            connect_timeout: config.connect_timeout,
            read_timeout: config.read_timeout,
            write_timeout: config.write_timeout(),
            user_agent: config.user_agent.clone(),
        };
        let stream =
            HttpProxyTunnel::establish_tunnel_on_stream(stream, &http_config, false).await?;

        info!(
            "FTP proxy tunnel established: {}:{} -> {}:{}",
            config.proxy_host, config.proxy_port, config.target_host, config.target_port
        );
        Ok(stream)
    }
}

impl FtpProxyTunnelConfig {
    /// Calculate write timeout from connect_timeout.
    fn write_timeout(&self) -> Duration {
        self.connect_timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn proxy_tunnel_binds_configured_source_address() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("proxy tunnel fixture should bind");
        let address = listener.local_addr().expect("proxy tunnel fixture address");
        let server = tokio::spawn(async move {
            let (mut stream, peer) = listener.accept().await.expect("proxy should accept");
            let mut request = [0u8; 4096];
            let bytes = stream
                .read(&mut request)
                .await
                .expect("read CONNECT request");
            assert!(
                String::from_utf8_lossy(&request[..bytes])
                    .starts_with("CONNECT ftp.example.com:21 HTTP/1.1\r\n")
            );
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .expect("write CONNECT response");
            peer
        });

        let config = FtpProxyTunnelConfig {
            proxy_host: "localhost".to_string(),
            proxy_port: address.port(),
            target_host: "ftp.example.com".to_string(),
            target_port: 21,
            ..Default::default()
        };
        let stream = FtpProxyTunnel::establish_with_policy(
            &config,
            &OutboundNetworkPolicy::single(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                127, 0, 0, 2,
            ))),
        )
        .await
        .expect("proxy tunnel should use the configured source");

        assert_eq!(
            server
                .await
                .expect("proxy tunnel fixture should finish")
                .ip(),
            stream.local_addr().expect("read tunnel local address").ip()
        );
        assert_eq!(
            stream.local_addr().expect("read tunnel local address").ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 2))
        );
    }

    #[tokio::test]
    async fn ftp_proxy_tunnel_uses_shared_http_response_parser() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("proxy fixture should bind");
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let n = stream.read(&mut request).await.unwrap();
            assert!(request[..n].starts_with(b"CONNECT ftp.example.com:21 HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 Connection Established\r\n\r\n220 FTP ready\r\n")
                .await
                .unwrap();
        });

        let config = FtpProxyTunnelConfig {
            proxy_host: "127.0.0.1".into(),
            proxy_port: address.port(),
            target_host: "ftp.example.com".into(),
            target_port: 21,
            ..Default::default()
        };
        let mut stream = FtpProxyTunnel::establish(&config)
            .await
            .expect("the informational response should be skipped");
        let mut greeting = [0; 15];
        stream.read_exact(&mut greeting).await.unwrap();
        assert_eq!(&greeting, b"220 FTP ready\r\n");
        assert!(stream.peer_addr().is_ok());
        server.await.unwrap();
    }

    #[test]
    fn test_build_basic_auth_header() {
        let header = aria2_protocol::http::auth::basic_authorization("user", "pass");
        assert!(header.starts_with("Basic "));
        // Base64 of "user:pass" is "dXNlcjpwYXNz"
        assert_eq!(header, "Basic dXNlcjpwYXNz");
    }

    #[test]
    fn test_proxy_tunnel_config_default() {
        let config = FtpProxyTunnelConfig::default();
        assert_eq!(config.proxy_port, 8080);
        assert_eq!(config.target_port, 21);
        assert!(config.proxy_username.is_empty());
        assert!(config.proxy_password.is_empty());
    }

    #[tokio::test]
    async fn ftp_proxy_tunnel_retries_auth_without_sending_credentials_preemptively() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("proxy fixture should bind");
        let address = listener.local_addr().expect("proxy fixture address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("proxy should accept");
            let mut request = [0; 2048];
            let length = stream.read(&mut request).await.expect("read first CONNECT");
            let first = String::from_utf8_lossy(&request[..length]);
            assert!(first.starts_with("CONNECT ftp.example.com:21 HTTP/1.1\r\n"));
            assert!(!first.contains("Proxy-Authorization:"));

            stream
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 1\r\n\r\nx",
                )
                .await
                .expect("send proxy challenge");

            let length = stream
                .read(&mut request)
                .await
                .expect("read authenticated CONNECT");
            let retry = String::from_utf8_lossy(&request[..length]);
            assert!(retry.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n220 FTP ready\r\n")
                .await
                .expect("complete tunnel");
        });

        let config = FtpProxyTunnelConfig {
            proxy_host: "127.0.0.1".into(),
            proxy_port: address.port(),
            target_host: "ftp.example.com".into(),
            target_port: 21,
            proxy_username: "user".into(),
            proxy_password: "pass".into(),
            ..Default::default()
        };
        let mut stream = FtpProxyTunnel::establish(&config)
            .await
            .expect("challenge retry should establish the tunnel");
        let mut greeting = [0; 15];
        stream
            .read_exact(&mut greeting)
            .await
            .expect("read FTP greeting through tunnel");
        assert_eq!(&greeting, b"220 FTP ready\r\n");
        server.await.expect("proxy fixture should finish");
    }
}

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

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::proxy_tunnel::connect::{HttpProxyTunnel, ProxyResponse};
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
        let mut stream = timeout(config.connect_timeout, connect)
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

        stream.set_nodelay(true).map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("set_nodelay failed: {}", e),
            })
        })?;

        // Step 2: Send CONNECT request (without auth first)
        let connect_request = Self::build_connect_request(config, None);
        Self::send_request(&mut stream, &connect_request, config.write_timeout()).await?;

        // Step 3: Read proxy response
        let response = Self::read_proxy_response(&mut stream, config.read_timeout).await?;

        match response.status_code {
            200 => {
                // Tunnel established successfully
                info!(
                    "FTP proxy tunnel established: {}:{} -> {}:{}",
                    config.proxy_host, config.proxy_port, config.target_host, config.target_port
                );
                Ok(stream)
            }
            407 => {
                // Proxy authentication required
                Self::handle_proxy_auth(stream, config, &response).await
            }
            status => {
                // Proxy rejected the CONNECT request
                Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!(
                            "Proxy {}:{} rejected CONNECT to {}:{}: {} {}",
                            config.proxy_host,
                            config.proxy_port,
                            config.target_host,
                            config.target_port,
                            status,
                            response.reason_phrase
                        ),
                    },
                ))
            }
        }
    }

    /// Handle 407 Proxy Authentication Required response.
    ///
    /// Parses the Proxy-Authenticate header and retries the CONNECT
    /// request with appropriate credentials.
    async fn handle_proxy_auth(
        mut stream: TcpStream,
        config: &FtpProxyTunnelConfig,
        response: &ProxyResponse,
    ) -> Result<TcpStream> {
        if config.proxy_username.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Proxy {}:{} requires authentication but no credentials provided",
                        config.proxy_host, config.proxy_port
                    ),
                },
            ));
        }

        // Consume any remaining response body
        Self::consume_response_body(&mut stream, config.read_timeout).await?;

        // Check what auth scheme the proxy supports
        let proxy_authenticate = response
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Proxy-Authenticate"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");

        let scheme = proxy_authenticate
            .split_ascii_whitespace()
            .next()
            .unwrap_or_default();
        let auth_header = if scheme.eq_ignore_ascii_case("Basic") {
            // Basic authentication
            Self::build_basic_auth_header(&config.proxy_username, &config.proxy_password)
        } else if scheme.eq_ignore_ascii_case("Digest") {
            // Digest authentication
            Self::build_digest_auth_header(
                &config.proxy_username,
                &config.proxy_password,
                proxy_authenticate,
                &format!("{}:{}", config.target_host, config.target_port),
            )
        } else {
            // Unknown or unsupported auth scheme
            warn!(
                "Unsupported proxy auth scheme: {}",
                proxy_authenticate
                    .split_whitespace()
                    .next()
                    .unwrap_or("unknown")
            );
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Proxy requires unsupported authentication scheme: {}",
                        proxy_authenticate
                    ),
                },
            ));
        };

        // Retry CONNECT with auth
        let connect_request = Self::build_connect_request(config, Some(&auth_header));
        Self::send_request(&mut stream, &connect_request, config.write_timeout()).await?;

        let retry_response = Self::read_proxy_response(&mut stream, config.read_timeout).await?;

        match retry_response.status_code {
            200 => {
                info!(
                    "FTP proxy tunnel established with authentication: {}:{} -> {}:{}",
                    config.proxy_host, config.proxy_port, config.target_host, config.target_port
                );
                Ok(stream)
            }
            407 => Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Proxy authentication failed for {}:{}",
                        config.proxy_host, config.proxy_port
                    ),
                },
            )),
            status => Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Proxy rejected CONNECT after auth: {} {}",
                        status, retry_response.reason_phrase
                    ),
                },
            )),
        }
    }

    /// Build the HTTP CONNECT request string.
    fn build_connect_request(config: &FtpProxyTunnelConfig, auth_header: Option<&str>) -> String {
        let target = format!("{}:{}", config.target_host, config.target_port);
        let mut headers = vec![
            ("Host", target.as_str()),
            ("User-Agent", config.user_agent.as_str()),
        ];
        if let Some(auth) = auth_header {
            headers.push(("Proxy-Authorization", auth));
        }
        String::from_utf8(aria2_protocol::http::request::serialize_http_request(
            "CONNECT", &target, &headers, None,
        ))
        .expect("HTTP CONNECT fields are UTF-8")
    }

    /// Send the CONNECT request to the proxy.
    async fn send_request(
        stream: &mut TcpStream,
        request: &str,
        write_timeout: Duration,
    ) -> Result<()> {
        debug!("Sending CONNECT request to proxy:\n{}", request.trim_end());
        timeout(write_timeout, stream.write_all(request.as_bytes()))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to send CONNECT request: {}", e),
                })
            })?;
        stream.flush().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Failed to flush CONNECT request: {}", e),
            })
        })?;
        Ok(())
    }

    /// Read the proxy's HTTP response to the CONNECT request.
    async fn read_proxy_response(
        stream: &mut TcpStream,
        read_timeout: Duration,
    ) -> Result<ProxyResponse> {
        HttpProxyTunnel::read_proxy_response(stream, read_timeout).await
    }

    /// Consume any response body data (for 407 responses).
    async fn consume_response_body(stream: &mut TcpStream, _read_timeout: Duration) -> Result<()> {
        // Read any remaining data with a short timeout
        let mut buf = [0u8; 4096];
        match timeout(Duration::from_secs(2), stream.read(&mut buf)).await {
            Ok(Ok(n)) => {
                debug!("Consumed {} bytes of proxy response body", n);
                Ok(())
            }
            Ok(Err(e)) => {
                debug!("Error consuming proxy response body: {}", e);
                Ok(())
            }
            Err(_) => {
                debug!("Timeout consuming proxy response body (acceptable)");
                Ok(())
            }
        }
    }

    /// Build a Basic proxy-auth header value.
    fn build_basic_auth_header(username: &str, password: &str) -> String {
        use base64::Engine;
        let credentials = format!("{}:{}", username, password);
        let encoded = base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
        format!("Basic {}", encoded)
    }

    /// Build a Digest proxy-auth header value.
    ///
    /// Parses the Proxy-Authenticate challenge and generates
    /// a Proxy-Authorization response.
    fn build_digest_auth_header(
        username: &str,
        password: &str,
        challenge: &str,
        uri: &str,
    ) -> String {
        crate::http::proxy_tunnel::auth::build_digest_auth_header(
            username, password, challenge, uri,
        )
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
    fn test_build_connect_request_without_auth() {
        let config = FtpProxyTunnelConfig {
            proxy_host: "proxy.example.com".to_string(),
            proxy_port: 8080,
            target_host: "ftp.example.com".to_string(),
            target_port: 21,
            user_agent: crate::constants::USER_AGENT.to_string(),
            ..Default::default()
        };

        let request = FtpProxyTunnel::build_connect_request(&config, None);
        assert!(request.starts_with("CONNECT ftp.example.com:21 HTTP/1.1\r\n"));
        assert!(request.contains("Host: ftp.example.com:21\r\n"));
        assert!(request.contains(&format!("User-Agent: {}\r\n", crate::constants::USER_AGENT)));
        assert!(!request.contains("Proxy-Authorization"));
        assert!(request.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_build_connect_request_with_auth() {
        let config = FtpProxyTunnelConfig {
            target_host: "ftp.example.com".to_string(),
            target_port: 21,
            ..Default::default()
        };

        let request = FtpProxyTunnel::build_connect_request(&config, Some("Basic dXNlcjpwYXNz"));
        assert!(request.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
    }

    #[test]
    fn test_build_basic_auth_header() {
        let header = FtpProxyTunnel::build_basic_auth_header("user", "pass");
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

    #[test]
    fn test_build_digest_auth_header() {
        let challenge = r#"Digest realm="test, private", nonce="abc123", qop="auth-int", algorithm=SHA-256-sess"#;
        let header = FtpProxyTunnel::build_digest_auth_header(
            "user",
            "pass",
            challenge,
            "ftp.example.com:21",
        );
        assert!(header.starts_with("Digest"));
        assert!(header.contains("username=\"user\""));
        assert!(header.contains("realm=\"test, private\""));
        assert!(header.contains("nonce=\"abc123\""));
        assert!(header.contains("qop=auth-int"));
        assert!(header.contains("algorithm=SHA-256-sess"));
        assert!(header.contains("response=\""));
    }
}

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

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, RecoverableError, Result};

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
            user_agent: "aria2/rust".to_string(),
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
        // Step 1: Connect to the proxy server
        debug!(
            "Connecting to HTTP proxy {}:{} for FTP tunnel to {}:{}",
            config.proxy_host, config.proxy_port, config.target_host, config.target_port
        );

        let mut stream = timeout(
            config.connect_timeout,
            TcpStream::connect((config.proxy_host.as_str(), config.proxy_port)),
        )
        .await
        .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
        .map_err(|e| {
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

        let auth_header = if proxy_authenticate.starts_with("Basic") {
            // Basic authentication
            Self::build_basic_auth_header(&config.proxy_username, &config.proxy_password)
        } else if proxy_authenticate.starts_with("Digest") {
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
        let mut request = format!(
            "CONNECT {}:{} HTTP/1.1\r\n\
             Host: {}:{}\r\n\
             User-Agent: {}\r\n",
            config.target_host,
            config.target_port,
            config.target_host,
            config.target_port,
            config.user_agent
        );

        if let Some(auth) = auth_header {
            request.push_str(&format!("Proxy-Authorization: {}\r\n", auth));
        }

        request.push_str("\r\n");
        request
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
        let mut buf_reader = tokio::io::BufReader::new(stream);
        let response = timeout(read_timeout, Self::parse_response(&mut buf_reader))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))??;

        // We need to get the stream back from buf_reader
        // Since we borrowed it, the buffered data is already consumed
        Ok(response)
    }

    /// Parse an HTTP response from the proxy.
    async fn parse_response<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Result<ProxyResponse> {
        // Parse status line
        let mut status_line = String::new();
        reader.read_line(&mut status_line).await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Failed to read proxy response: {}", e),
            })
        })?;

        let status_line = status_line.trim_end();
        debug!("Proxy response status: {}", status_line);

        // Parse: HTTP/x.x status_code reason_phrase
        let parts: Vec<&str> = status_line.splitn(3, ' ').collect();
        if parts.len() < 2 {
            return Err(Aria2Error::Parse(format!(
                "Malformed proxy response: {}",
                status_line
            )));
        }

        let status_code: u16 = parts[1]
            .parse()
            .map_err(|_| Aria2Error::Parse(format!("Invalid proxy status code: {}", parts[1])))?;

        let reason_phrase = parts.get(2).unwrap_or(&"").to_string();

        // Parse headers
        let mut headers = Vec::new();
        loop {
            let mut header_line = String::new();
            reader.read_line(&mut header_line).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to read proxy headers: {}", e),
                })
            })?;

            let trimmed = header_line.trim_end();
            if trimmed.is_empty() {
                break; // End of headers
            }

            if let Some(colon_pos) = trimmed.find(':') {
                let name = trimmed[..colon_pos].trim().to_string();
                let value = trimmed[colon_pos + 1..].trim().to_string();
                headers.push((name, value));
            }
        }

        Ok(ProxyResponse {
            status_code,
            reason_phrase,
            headers,
        })
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
        // Parse digest challenge parameters
        let realm = extract_digest_param(challenge, "realm").unwrap_or("unknown");
        let nonce = extract_digest_param(challenge, "nonce").unwrap_or("");
        let opaque = extract_digest_param(challenge, "opaque");
        let qop = extract_digest_param(challenge, "qop");
        let algorithm = extract_digest_param(challenge, "algorithm")
            .unwrap_or("MD5")
            .to_uppercase();

        if nonce.is_empty() {
            return Self::build_basic_auth_header(username, password);
        }

        // Compute digest response
        let ha1 = if algorithm == "MD5-SESS" {
            let ha1_base = md5_hex(&format!("{}:{}:{}", username, realm, password));
            md5_hex(&format!("{}:{}:{}", ha1_base, nonce, "00000001"))
        } else {
            md5_hex(&format!("{}:{}:{}", username, realm, password))
        };

        let (ha2, qop_value) =
            if qop == Some("auth") || qop == Some("auth-int") {
                let ha2 = md5_hex(&format!("CONNECT:{}", uri));
                (ha2, "auth")
            } else {
                let ha2 = md5_hex(&format!("CONNECT:{}", uri));
                (ha2, "")
            };

        let response = if qop_value.is_empty() {
            md5_hex(&format!("{}:{}:{}", ha1, nonce, ha2))
        } else {
            let cnonce = "aria2rustcnonce";
            let nc = "00000001";
            md5_hex(&format!(
                "{}:{}:{}:{}:{}:{}",
                ha1, nonce, nc, cnonce, qop_value, ha2
            ))
        };

        let mut header = format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\"",
            username, realm, nonce, uri, response
        );

        if let Some(opaque) = opaque {
            header.push_str(&format!(", opaque=\"{}\"", opaque));
        }

        if !qop_value.is_empty() {
            header.push_str(&format!(
                ", qop={}, nc=00000001, cnonce=\"{}\"",
                qop_value, "aria2rustcnonce"
            ));
        }

        header.push_str(&format!(", algorithm={}", algorithm));

        header
    }
}

impl FtpProxyTunnelConfig {
    /// Calculate write timeout from connect_timeout.
    fn write_timeout(&self) -> Duration {
        self.connect_timeout
    }
}

/// Parsed HTTP response from the proxy.
#[derive(Debug)]
struct ProxyResponse {
    /// HTTP status code (e.g., 200, 407)
    status_code: u16,
    /// Reason phrase (e.g., "Connection Established")
    reason_phrase: String,
    /// Response headers as (name, value) pairs
    headers: Vec<(String, String)>,
}

/// Extract a parameter value from a Digest challenge string.
///
/// Parses strings like: `Digest realm="example.com", nonce="abc123"`
fn extract_digest_param<'a>(challenge: &'a str, param: &str) -> Option<&'a str> {
    let search = format!("{}=", param);
    // Split on commas, skipping the "Digest" scheme prefix
    let params_section = if let Some(pos) = challenge.find(' ') {
        &challenge[pos + 1..]
    } else {
        challenge
    };

    for part in params_section.split(',') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&search) {
                // Handle quoted values
                if let Some(stripped) = rest.strip_prefix('"') {
                    if let Some(end) = stripped.find('"') {
                        return Some(&stripped[..end]);
                }
            } else {
                // Unquoted value - take until comma or end
                let end = rest.find(',').unwrap_or(rest.len());
                return Some(&rest[..end]);
            }
        }
    }
    None
}

/// Compute MD5 hex digest.
fn md5_hex(input: &str) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    format!("{:x}", result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_connect_request_without_auth() {
        let config = FtpProxyTunnelConfig {
            proxy_host: "proxy.example.com".to_string(),
            proxy_port: 8080,
            target_host: "ftp.example.com".to_string(),
            target_port: 21,
            user_agent: "aria2/rust".to_string(),
            ..Default::default()
        };

        let request = FtpProxyTunnel::build_connect_request(&config, None);
        assert!(request.starts_with("CONNECT ftp.example.com:21 HTTP/1.1\r\n"));
        assert!(request.contains("Host: ftp.example.com:21\r\n"));
        assert!(request.contains("User-Agent: aria2/rust\r\n"));
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
    fn test_extract_digest_param() {
        let challenge = r#"Digest realm="testrealm", nonce="abc123", qop="auth""#;
        assert_eq!(extract_digest_param(challenge, "realm"), Some("testrealm"));
        assert_eq!(extract_digest_param(challenge, "nonce"), Some("abc123"));
        assert_eq!(extract_digest_param(challenge, "qop"), Some("auth"));
        assert_eq!(extract_digest_param(challenge, "opaque"), None);
    }

    #[test]
    fn test_extract_digest_param_unquoted() {
        let challenge = "Digest algorithm=MD5, realm=\"test\"";
        assert_eq!(extract_digest_param(challenge, "algorithm"), Some("MD5"));
        assert_eq!(extract_digest_param(challenge, "realm"), Some("test"));
    }

    #[test]
    fn test_md5_hex() {
        // Known MD5 value
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("hello"), "5d41402abc4b2a76b9719d911017c592");
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
        let challenge =
            r#"Digest realm="test@example.com", nonce="abc123", qop="auth", algorithm=MD5"#;
        let header = FtpProxyTunnel::build_digest_auth_header(
            "user",
            "pass",
            challenge,
            "ftp.example.com:21",
        );
        assert!(header.starts_with("Digest"));
        assert!(header.contains("username=\"user\""));
        assert!(header.contains("realm=\"test@example.com\""));
        assert!(header.contains("nonce=\"abc123\""));
        assert!(header.contains("response=\""));
    }
}

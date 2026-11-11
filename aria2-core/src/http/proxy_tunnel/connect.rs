//! CONNECT tunnel establishment and I/O helpers

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::request::basic_auth;
use aria2_protocol::http::header_processor::{find_header_end, parse_response_head};

use super::HttpProxyTunnelConfig;
use super::auth;

// ---------------------------------------------------------------------------
// HttpProxyTunnel -- internal implementation
// ---------------------------------------------------------------------------

/// HTTP proxy tunnel handler.
pub(crate) struct HttpProxyTunnel;

impl HttpProxyTunnel {
    const MAX_AUTH_RETRIES: u32 = 1;

    pub(crate) async fn establish_tunnel(config: &HttpProxyTunnelConfig) -> Result<TcpStream> {
        let stream = Self::connect_to_proxy(config).await?;
        let stream = Self::tunnel_handshake(stream, config, 0).await?;
        info!(
            "HTTP proxy tunnel established: {}:{} -> {}:{}",
            config.proxy_host, config.proxy_port, config.target_host, config.target_port
        );
        Ok(stream)
    }

    pub(crate) async fn establish_forward(config: &HttpProxyTunnelConfig) -> Result<TcpStream> {
        let stream = Self::connect_to_proxy(config).await?;
        info!(
            "HTTP forward-proxy connection: {}:{} for target {}:{}",
            config.proxy_host, config.proxy_port, config.target_host, config.target_port
        );
        Ok(stream)
    }

    // -- Connection & handshake ------------------------------------------------

    async fn connect_to_proxy(config: &HttpProxyTunnelConfig) -> Result<TcpStream> {
        debug!(
            "Connecting to proxy {}:{} for {}:{}",
            config.proxy_host, config.proxy_port, config.target_host, config.target_port
        );
        let stream = timeout(
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
        Ok(stream)
    }

    /// Perform the CONNECT handshake with proxy auth retry logic.
    ///
    /// Uses a loop instead of recursion to avoid `Pin<Box<dyn Future>>`
    /// lifetime issues. Max auth retries bounded by `MAX_AUTH_RETRIES`.
    async fn tunnel_handshake(
        mut stream: TcpStream,
        config: &HttpProxyTunnelConfig,
        _auth_retry: u32,
    ) -> Result<TcpStream> {
        let auth_header = auth::maybe_preemptive_basic_auth(config);
        let request = Self::build_connect_request(config, auth_header.as_deref());
        Self::send_request(&mut stream, &request, config.write_timeout).await?;

        let mut remaining_retries = Self::MAX_AUTH_RETRIES;
        loop {
            let response = Self::read_proxy_response(&mut stream, config.read_timeout).await?;
            match response.status_code {
                200 => return Ok(stream),
                407 if remaining_retries > 0 => {
                    remaining_retries -= 1;
                    let has_creds = config.username.as_ref().is_some_and(|u| !u.is_empty());
                    if !has_creds {
                        return Err(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: format!(
                                    "Proxy {}:{} requires auth but no credentials provided",
                                    config.proxy_host, config.proxy_port
                                ),
                            },
                        ));
                    }
                    Self::consume_response_body(&mut stream, config.read_timeout).await?;
                    let proxy_authenticate = response
                        .headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("proxy-authenticate"))
                        .map(|(_, v)| v.as_str())
                        .unwrap_or("");
                    let username = config.username.as_deref().unwrap_or("");
                    let password = config.password.as_deref().unwrap_or("");
                    let uri = format!("{}:{}", config.target_host, config.target_port);
                    let scheme = proxy_authenticate
                        .split_ascii_whitespace()
                        .next()
                        .unwrap_or_default();
                    let auth_header = if scheme.eq_ignore_ascii_case("Digest") {
                        auth::build_digest_auth_header(username, password, proxy_authenticate, &uri)
                    } else if scheme.eq_ignore_ascii_case("Basic") {
                        basic_auth(username, password)
                    } else {
                        warn!("Unsupported proxy auth scheme: {}", proxy_authenticate);
                        return Err(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: format!("Unsupported proxy auth: {}", proxy_authenticate),
                            },
                        ));
                    };
                    let request = Self::build_connect_request(config, Some(&auth_header));
                    Self::send_request(&mut stream, &request, config.write_timeout).await?;
                    // Loop back to read the response to the auth'd request
                }
                407 => {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: format!(
                                "Proxy auth failed for {}:{}",
                                config.proxy_host, config.proxy_port
                            ),
                        },
                    ));
                }
                status => {
                    return Err(Aria2Error::Recoverable(
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
                    ));
                }
            }
        }
    }

    // -- Request building ------------------------------------------------------

    /// Build the CONNECT request string (matches C++ `HttpRequest::createProxyRequest`).
    pub(crate) fn build_connect_request(
        config: &HttpProxyTunnelConfig,
        auth_header: Option<&str>,
    ) -> String {
        let target = format!("{}:{}", config.target_host, config.target_port);
        let mut headers = vec![
            ("User-Agent", config.user_agent.as_str()),
            ("Host", target.as_str()),
        ];
        if let Some(auth) = auth_header {
            headers.push(("Proxy-Authorization", auth));
        }
        String::from_utf8(aria2_protocol::http::request::serialize_http_request(
            "CONNECT", &target, &headers, None,
        ))
        .expect("HTTP CONNECT fields are UTF-8")
    }

    // -- I/O helpers -----------------------------------------------------------

    async fn send_request(
        stream: &mut TcpStream,
        request: &str,
        write_timeout: Duration,
    ) -> Result<()> {
        debug!("Sending proxy request:\n{}", request.trim_end());
        timeout(write_timeout, stream.write_all(request.as_bytes()))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Failed to send proxy request: {}", e),
                })
            })?;
        stream.flush().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Failed to flush proxy request: {}", e),
            })
        })?;
        Ok(())
    }

    /// Read proxy response using streaming `HttpHeaderProcessor`.
    /// Skips 1xx informational responses (matching C++ behavior).
    pub(crate) async fn read_proxy_response(
        stream: &mut TcpStream,
        read_timeout: Duration,
    ) -> Result<ProxyResponse> {
        loop {
            let response = timeout(read_timeout, Self::read_response_head(stream))
                .await
                .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))??;
            if (100..200).contains(&response.status_code) {
                debug!("Skipping 1xx informational: {}", response.status_code);
                continue;
            }
            return Ok(response);
        }
    }

    async fn read_response_head(stream: &mut TcpStream) -> Result<ProxyResponse> {
        const MAX_HEADER_BYTES: usize = 8 * 1024 + 4;
        let mut header = Vec::with_capacity(512);
        let mut peek_buffer = [0; MAX_HEADER_BYTES];

        loop {
            let remaining = MAX_HEADER_BYTES.saturating_sub(header.len());
            if remaining == 0 {
                return Err(Aria2Error::Parse(
                    "Failed to parse proxy response: header too large".to_string(),
                ));
            }
            let n = stream
                .peek(&mut peek_buffer[..remaining])
                .await
                .map_err(|error| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("Failed to read proxy response: {error}"),
                    })
                })?;
            if n == 0 {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "Connection closed while reading proxy response".to_string(),
                    },
                ));
            }

            let response_end = find_header_end(&peek_buffer[..n]).map(|index| index + 4);
            let bytes_to_consume = response_end.unwrap_or(n);
            let start = header.len();
            header.resize(start + bytes_to_consume, 0);
            stream
                .read_exact(&mut header[start..])
                .await
                .map_err(|error| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("Failed to read proxy response: {error}"),
                    })
                })?;

            if response_end.is_some() {
                break;
            }
        }

        let head = parse_response_head(&header)
            .map_err(|_| Aria2Error::Parse("Failed to parse proxy response".to_string()))?;
        let headers: Vec<(String, String)> = head
            .iter_headers()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Ok(ProxyResponse {
            status_code: head.status_code,
            reason_phrase: head.reason_phrase,
            headers,
        })
    }

    /// Consume any remaining response body (for 407 responses).
    async fn consume_response_body(stream: &mut TcpStream, _read_timeout: Duration) -> Result<()> {
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
}

// ---------------------------------------------------------------------------
// ProxyResponse (internal)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct ProxyResponse {
    pub(crate) status_code: u16,
    pub(crate) reason_phrase: String,
    pub(crate) headers: Vec<(String, String)>,
}

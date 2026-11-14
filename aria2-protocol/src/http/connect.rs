//! HTTP CONNECT handshake over a caller-established TCP stream.

use std::io;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::debug;

use super::auth::basic_authorization;
use super::digest::{DigestChallenge, DigestResponse};
use super::header_processor::{HttpResponseHeadReadError, read_response_head};
use super::request::serialize_http_request;

/// Settings for one HTTP CONNECT handshake.
pub struct HttpConnectOptions<'a> {
    /// Target host used in the CONNECT authority and Host field.
    pub target_host: &'a str,
    /// Target port used in the CONNECT authority and Host field.
    pub target_port: u16,
    /// User-Agent field value.
    pub user_agent: &'a str,
    /// Optional proxy username.
    pub username: Option<&'a str>,
    /// Proxy password, if configured.
    pub password: Option<&'a str>,
    /// Whether to send configured Basic credentials before a challenge.
    pub preemptive_basic_auth: bool,
    /// Maximum time to write each CONNECT request.
    pub write_timeout: Duration,
    /// Maximum time to read each proxy response head.
    pub read_timeout: Duration,
}

/// Authentication and request details for compatibility callers that need a
/// CONNECT retry policy different from the engine default.
#[derive(Debug, Clone, Copy)]
pub struct HttpConnectRetryPolicy {
    /// Maximum number of CONNECT retries after a 407 response.
    pub max_auth_retries: u32,
    /// Prefer a Digest challenge over Basic when the proxy sends several.
    pub prefer_digest: bool,
    /// Fall back to Basic when the proxy advertises only an unknown scheme.
    pub fallback_to_basic: bool,
    /// Include the legacy `Proxy-Connection: keep-alive` request field.
    pub proxy_connection_keep_alive: bool,
}

impl Default for HttpConnectRetryPolicy {
    fn default() -> Self {
        Self {
            max_auth_retries: 1,
            prefer_digest: false,
            fallback_to_basic: false,
            proxy_connection_keep_alive: false,
        }
    }
}

/// Failure while establishing an HTTP CONNECT tunnel.
#[derive(Debug, Error)]
pub enum HttpConnectError {
    /// A CONNECT read or write timed out.
    #[error("HTTP CONNECT operation timed out")]
    Timeout,
    /// A stream read or write failed.
    #[error("HTTP CONNECT stream I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The proxy response could not be parsed.
    #[error("Invalid HTTP CONNECT response: {0}")]
    InvalidResponse(String),
    /// The proxy asked for credentials but none were configured.
    #[error("proxy requires authentication but no credentials were provided")]
    CredentialsRequired,
    /// The proxy requested an authentication scheme that is not supported.
    #[error("unsupported proxy authentication challenge: {0}")]
    UnsupportedAuthentication(String),
    /// Authentication was rejected after the bounded retry.
    #[error("proxy authentication failed")]
    AuthenticationFailed,
    /// The proxy rejected the CONNECT request.
    #[error("proxy rejected CONNECT with status {status_code} {reason_phrase}")]
    Rejected {
        /// HTTP status returned by the proxy.
        status_code: u16,
        /// Reason phrase returned by the proxy.
        reason_phrase: String,
    },
}

struct ProxyResponse {
    status_code: u16,
    reason_phrase: String,
    headers: Vec<(String, String)>,
}

/// Complete an HTTP CONNECT handshake without opening or replacing the stream.
///
/// The caller retains DNS, socket creation, binding, and outbound-network
/// policy. This function owns CONNECT framing, informational responses,
/// proxy-auth challenge handling, and response-head reads. It reads exactly
/// through each header terminator so bytes following a successful response
/// remain available to the tunneled protocol.
pub async fn establish_connect_tunnel(
    stream: &mut TcpStream,
    options: &HttpConnectOptions<'_>,
) -> Result<(), HttpConnectError> {
    establish_connect_tunnel_with_retry_policy(stream, options, &HttpConnectRetryPolicy::default())
        .await
}

/// Complete a CONNECT handshake using an explicit proxy-authentication retry
/// policy. This supports public compatibility facades without moving their
/// protocol session logic back into the engine crate.
pub async fn establish_connect_tunnel_with_retry_policy(
    stream: &mut TcpStream,
    options: &HttpConnectOptions<'_>,
    retry_policy: &HttpConnectRetryPolicy,
) -> Result<(), HttpConnectError> {
    let preemptive_auth = if options.preemptive_basic_auth {
        options
            .username
            .filter(|username| !username.is_empty())
            .map(|username| basic_authorization(username, options.password.unwrap_or("")))
    } else {
        None
    };
    send_request(
        stream,
        options,
        preemptive_auth.as_deref(),
        retry_policy.proxy_connection_keep_alive,
    )
    .await?;

    let mut auth_retries = 0;
    loop {
        let response = read_proxy_response(stream, options.read_timeout).await?;
        match response.status_code {
            200 => return Ok(()),
            407 if auth_retries < retry_policy.max_auth_retries => {
                auth_retries += 1;
                let username = options
                    .username
                    .filter(|username| !username.is_empty())
                    .ok_or(HttpConnectError::CredentialsRequired)?;
                consume_response_body(stream, &response.headers, options.read_timeout).await?;
                let target = format!("{}:{}", options.target_host, options.target_port);
                let auth = challenged_authentication(
                    username,
                    options.password.unwrap_or(""),
                    &response.headers,
                    &target,
                    auth_retries,
                    retry_policy,
                )?;
                send_request(
                    stream,
                    options,
                    Some(&auth),
                    retry_policy.proxy_connection_keep_alive,
                )
                .await?;
            }
            407 => return Err(HttpConnectError::AuthenticationFailed),
            status_code => {
                return Err(HttpConnectError::Rejected {
                    status_code,
                    reason_phrase: response.reason_phrase,
                });
            }
        }
    }
}

fn challenged_authentication(
    username: &str,
    password: &str,
    headers: &[(String, String)],
    target: &str,
    nonce_count: u32,
    retry_policy: &HttpConnectRetryPolicy,
) -> Result<String, HttpConnectError> {
    let challenges = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("proxy-authenticate"))
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>();
    let challenge = if retry_policy.prefer_digest {
        challenges
            .iter()
            .copied()
            .find(|value| challenge_scheme(value).eq_ignore_ascii_case("digest"))
            .or_else(|| challenges.first().copied())
    } else {
        challenges.first().copied()
    }
    .unwrap_or_default();
    let scheme = challenge_scheme(challenge);
    if scheme.eq_ignore_ascii_case("basic") {
        return Ok(basic_authorization(username, password));
    }
    if scheme.eq_ignore_ascii_case("digest") {
        let Ok(challenge) = DigestChallenge::parse(challenge) else {
            // Preserve the established behavior for malformed or unsupported
            // Digest challenges by falling back to Basic.
            return Ok(basic_authorization(username, password));
        };
        return Ok(DigestResponse::compute(
            username,
            password,
            "CONNECT",
            target,
            &challenge,
            nonce_count,
        )
        .to_header_value());
    }
    if retry_policy.fallback_to_basic && !challenge.is_empty() {
        return Ok(basic_authorization(username, password));
    }
    Err(HttpConnectError::UnsupportedAuthentication(
        challenge.to_string(),
    ))
}

fn challenge_scheme(challenge: &str) -> &str {
    challenge
        .split_ascii_whitespace()
        .next()
        .unwrap_or_default()
}

async fn send_request(
    stream: &mut TcpStream,
    options: &HttpConnectOptions<'_>,
    auth_header: Option<&str>,
    proxy_connection_keep_alive: bool,
) -> Result<(), HttpConnectError> {
    let target = format!("{}:{}", options.target_host, options.target_port);
    let mut headers = Vec::with_capacity(4);
    if !options.user_agent.is_empty() {
        headers.push(("User-Agent", options.user_agent));
    }
    headers.push(("Host", target.as_str()));
    if let Some(auth) = auth_header {
        headers.push(("Proxy-Authorization", auth));
    }
    if proxy_connection_keep_alive {
        headers.push(("Proxy-Connection", "keep-alive"));
    }
    let request = serialize_http_request("CONNECT", &target, &headers, None);
    debug!(target = %target, authenticated = auth_header.is_some(), "Sending HTTP CONNECT request");
    timeout(options.write_timeout, stream.write_all(&request))
        .await
        .map_err(|_| HttpConnectError::Timeout)??;
    stream.flush().await?;
    Ok(())
}

async fn read_proxy_response(
    stream: &mut TcpStream,
    read_timeout: Duration,
) -> Result<ProxyResponse, HttpConnectError> {
    loop {
        let response =
            read_response_head(stream, read_timeout)
                .await
                .map_err(|error| match error {
                    HttpResponseHeadReadError::Timeout => HttpConnectError::Timeout,
                    HttpResponseHeadReadError::Io(error) => HttpConnectError::Io(error),
                    HttpResponseHeadReadError::HeaderTooLarge => {
                        HttpConnectError::InvalidResponse("Too large HTTP header block".to_string())
                    }
                    HttpResponseHeadReadError::InvalidResponse(message) => {
                        HttpConnectError::InvalidResponse(message)
                    }
                })?;
        if (100..200).contains(&response.status_code) {
            debug!(
                status = response.status_code,
                "Skipping HTTP CONNECT informational response"
            );
            continue;
        }
        let headers = response
            .iter_headers()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        return Ok(ProxyResponse {
            status_code: response.status_code,
            reason_phrase: response.reason_phrase,
            headers,
        });
    }
}

async fn consume_response_body(
    stream: &mut TcpStream,
    headers: &[(String, String)],
    read_timeout: Duration,
) -> Result<(), HttpConnectError> {
    const MAX_AUTH_RESPONSE_BODY_BYTES: usize = 64 * 1024;
    if let Some((_, transfer_encoding)) = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
    {
        if transfer_encoding
            .split(',')
            .any(|coding| coding.trim().eq_ignore_ascii_case("chunked"))
        {
            timeout(read_timeout, consume_chunked_body(stream))
                .await
                .map_err(|_| HttpConnectError::Timeout)??;
            return Ok(());
        }
        return Err(HttpConnectError::InvalidResponse(
            "unsupported proxy authentication response transfer coding".to_string(),
        ));
    }

    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok());

    if content_length == Some(0) {
        return Ok(());
    }
    if let Some(length) = content_length.filter(|length| *length <= MAX_AUTH_RESPONSE_BODY_BYTES) {
        let mut remaining = length;
        let mut buffer = [0; 4096];
        let result = timeout(read_timeout, async {
            while remaining > 0 {
                let chunk_length = remaining.min(buffer.len());
                stream.read_exact(&mut buffer[..chunk_length]).await?;
                remaining -= chunk_length;
            }
            Ok::<(), io::Error>(())
        })
        .await;
        result
            .map_err(|_| HttpConnectError::Timeout)?
            .map_err(HttpConnectError::Io)?;
        debug!(length, "Consumed proxy authentication response body");
        return Ok(());
    }

    if content_length.is_some() {
        return Err(HttpConnectError::InvalidResponse(
            "proxy authentication response body exceeds the discard limit".to_string(),
        ));
    }

    // Responses without a usable Content-Length may still carry a small body.
    // Preserve the bounded legacy drain for these uncommon proxy responses.
    let mut buffer = [0; 4096];
    match timeout(read_timeout, stream.read(&mut buffer)).await {
        Ok(Ok(length)) => debug!(length, "Consumed proxy authentication response body"),
        Ok(Err(error)) => debug!(%error, "Could not consume proxy authentication response body"),
        Err(_) => debug!("Timed out while consuming proxy authentication response body"),
    }
    Ok(())
}

async fn consume_chunked_body(stream: &mut TcpStream) -> Result<(), HttpConnectError> {
    const MAX_CHUNKED_BODY_BYTES: usize = 1024 * 1024;
    const MAX_CHUNK_LINE_BYTES: usize = 1024;

    let mut consumed = 0usize;
    loop {
        let line = read_crlf_line(stream, MAX_CHUNK_LINE_BYTES).await?;
        let size_text = std::str::from_utf8(&line[..line.len() - 2])
            .map_err(|error| HttpConnectError::InvalidResponse(error.to_string()))?;
        let size_text = size_text.split(';').next().unwrap_or_default().trim();
        let chunk_size = usize::from_str_radix(size_text, 16)
            .map_err(|error| HttpConnectError::InvalidResponse(error.to_string()))?;
        if chunk_size == 0 {
            let mut trailer_bytes = 0usize;
            loop {
                let trailer = read_crlf_line(stream, MAX_CHUNK_LINE_BYTES).await?;
                trailer_bytes = trailer_bytes.saturating_add(trailer.len());
                if trailer_bytes > MAX_CHUNKED_BODY_BYTES {
                    return Err(HttpConnectError::InvalidResponse(
                        "proxy authentication response trailers exceed the discard limit"
                            .to_string(),
                    ));
                }
                if trailer == b"\r\n" {
                    return Ok(());
                }
            }
        }

        consumed = consumed.saturating_add(chunk_size);
        if consumed > MAX_CHUNKED_BODY_BYTES {
            return Err(HttpConnectError::InvalidResponse(
                "proxy authentication response body exceeds the discard limit".to_string(),
            ));
        }
        let mut remaining = chunk_size;
        let mut buffer = [0; 4096];
        while remaining > 0 {
            let length = remaining.min(buffer.len());
            stream.read_exact(&mut buffer[..length]).await?;
            remaining -= length;
        }
        let mut terminator = [0; 2];
        stream.read_exact(&mut terminator).await?;
        if terminator != *b"\r\n" {
            return Err(HttpConnectError::InvalidResponse(
                "invalid proxy authentication response chunk terminator".to_string(),
            ));
        }
    }
}

async fn read_crlf_line(
    stream: &mut TcpStream,
    max_line_bytes: usize,
) -> Result<Vec<u8>, HttpConnectError> {
    let mut line = Vec::with_capacity(32);
    while line.len() < max_line_bytes {
        let mut byte = [0; 1];
        stream.read_exact(&mut byte).await?;
        line.push(byte[0]);
        if line.ends_with(b"\r\n") {
            return Ok(line);
        }
    }
    Err(HttpConnectError::InvalidResponse(
        "proxy authentication response chunk line exceeds the limit".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn options<'a>(user_agent: &'a str) -> HttpConnectOptions<'a> {
        HttpConnectOptions {
            target_host: "example.com",
            target_port: 443,
            user_agent,
            username: None,
            password: None,
            preemptive_basic_auth: false,
            write_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn skips_informational_responses_and_preserves_tunnel_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let length = stream.read(&mut request).await.unwrap();
            assert!(request[..length].starts_with(b"CONNECT example.com:443 HTTP/1.1\r\n"));
            stream
                .write_all(
                    b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 Connection Established\r\n\r\nTLS",
                )
                .await
                .unwrap();
        });

        let mut stream = TcpStream::connect(address).await.unwrap();
        establish_connect_tunnel(&mut stream, &options("test-agent"))
            .await
            .unwrap();
        let mut tunneled = [0; 3];
        stream.read_exact(&mut tunneled).await.unwrap();
        assert_eq!(&tunneled, b"TLS");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn challenge_authentication_is_sent_only_after_407() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let length = stream.read(&mut request).await.unwrap();
            assert!(
                !request[..length]
                    .windows(b"Proxy-Authorization:".len())
                    .any(|window| window == b"Proxy-Authorization:")
            );
            stream
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 1\r\n\r\nx",
                )
                .await
                .unwrap();
            let length = stream.read(&mut request).await.unwrap();
            assert!(
                request[..length]
                    .windows(b"Proxy-Authorization: Basic dXNlcjpwYXNz\r\n".len())
                    .any(|window| window == b"Proxy-Authorization: Basic dXNlcjpwYXNz\r\n")
            );
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
        });

        let mut stream = TcpStream::connect(address).await.unwrap();
        let options = HttpConnectOptions {
            username: Some("user"),
            password: Some("pass"),
            ..options("test-agent")
        };
        establish_connect_tunnel(&mut stream, &options)
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn proxy_auth_body_drain_preserves_bytes_after_content_length() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"xNEXT").await.unwrap();
        });

        let mut stream = TcpStream::connect(address).await.unwrap();
        let headers = vec![("content-length".to_string(), "1".to_string())];
        consume_response_body(&mut stream, &headers, Duration::from_secs(2))
            .await
            .unwrap();
        let mut remaining = [0; 4];
        stream.read_exact(&mut remaining).await.unwrap();
        assert_eq!(&remaining, b"NEXT");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn proxy_auth_body_drain_uses_configured_read_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            stream.write_all(b"x").await.unwrap();
        });

        let mut stream = TcpStream::connect(address).await.unwrap();
        let headers = vec![("content-length".to_string(), "1".to_string())];
        let result = consume_response_body(&mut stream, &headers, Duration::from_millis(10)).await;

        assert!(matches!(result, Err(HttpConnectError::Timeout)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn proxy_auth_chunked_body_drain_preserves_bytes_after_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(b"1\r\nx\r\n0\r\nTrailer: done\r\n\r\nNEXT")
                .await
                .unwrap();
        });

        let mut stream = TcpStream::connect(address).await.unwrap();
        let headers = vec![("transfer-encoding".to_string(), "chunked".to_string())];
        consume_response_body(&mut stream, &headers, Duration::from_secs(2))
            .await
            .unwrap();
        let mut remaining = [0; 4];
        stream.read_exact(&mut remaining).await.unwrap();
        assert_eq!(&remaining, b"NEXT");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn preemptive_basic_auth_is_sent_on_the_first_request_when_enabled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let length = stream.read(&mut request).await.unwrap();
            assert!(
                request[..length]
                    .windows(b"Proxy-Authorization: Basic dXNlcjpwYXNz\r\n".len())
                    .any(|window| window == b"Proxy-Authorization: Basic dXNlcjpwYXNz\r\n")
            );
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
        });

        let mut stream = TcpStream::connect(address).await.unwrap();
        let options = HttpConnectOptions {
            username: Some("user"),
            password: Some("pass"),
            preemptive_basic_auth: true,
            ..options("test-agent")
        };
        establish_connect_tunnel(&mut stream, &options)
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[test]
    fn md5_sess_connect_response_uses_client_nonce_in_session_hash() {
        let challenge = DigestChallenge::parse(
            r#"Digest realm="testrealm@host.com", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth", algorithm=MD5-sess"#,
        )
        .unwrap();
        let response = DigestResponse::compute_with_cnonce(
            "Mufasa",
            "Circle Of Life",
            "CONNECT",
            "target.example.com:443",
            &challenge,
            1,
            "aria2rustcnonce",
        )
        .to_header_value();

        assert!(response.contains("cnonce=\"aria2rustcnonce\""));
        assert!(response.contains("response=\"3ac03d7149007bfa5959a930308906e0\""));
    }

    #[test]
    fn compatibility_policy_prefers_digest_and_uses_retry_count() {
        let headers = vec![
            (
                "proxy-authenticate".to_string(),
                "Basic realm=\"test\"".to_string(),
            ),
            (
                "proxy-authenticate".to_string(),
                "Digest realm=\"test\", nonce=\"n1\", qop=\"auth\"".to_string(),
            ),
        ];
        let auth = challenged_authentication(
            "user",
            "pass",
            &headers,
            "example.com:443",
            2,
            &HttpConnectRetryPolicy {
                max_auth_retries: 2,
                prefer_digest: true,
                fallback_to_basic: true,
                proxy_connection_keep_alive: true,
            },
        )
        .unwrap();

        assert!(auth.starts_with("Digest "));
        assert!(auth.contains("nc=00000002"));
    }

    #[test]
    fn compatibility_policy_falls_back_to_basic_for_unknown_challenge() {
        let headers = vec![("proxy-authenticate".to_string(), "NTLM".to_string())];
        let auth = challenged_authentication(
            "user",
            "pass",
            &headers,
            "example.com:443",
            1,
            &HttpConnectRetryPolicy {
                fallback_to_basic: true,
                ..HttpConnectRetryPolicy::default()
            },
        )
        .unwrap();

        assert_eq!(auth, "Basic dXNlcjpwYXNz");
    }
}

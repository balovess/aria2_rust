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
use super::header_processor::{find_header_end, parse_response_head};
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
    let preemptive_auth = if options.preemptive_basic_auth {
        options
            .username
            .filter(|username| !username.is_empty())
            .map(|username| basic_authorization(username, options.password.unwrap_or("")))
    } else {
        None
    };
    send_request(stream, options, preemptive_auth.as_deref()).await?;

    let mut auth_retries = 0;
    loop {
        let response = read_proxy_response(stream, options.read_timeout).await?;
        match response.status_code {
            200 => return Ok(()),
            407 if auth_retries == 0 => {
                auth_retries += 1;
                let username = options
                    .username
                    .filter(|username| !username.is_empty())
                    .ok_or(HttpConnectError::CredentialsRequired)?;
                consume_response_body(stream).await;
                let challenge = response
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("proxy-authenticate"))
                    .map(|(_, value)| value.as_str())
                    .unwrap_or_default();
                let target = format!("{}:{}", options.target_host, options.target_port);
                let auth = challenged_authentication(
                    username,
                    options.password.unwrap_or(""),
                    challenge,
                    &target,
                )?;
                send_request(stream, options, Some(&auth)).await?;
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
    challenge: &str,
    target: &str,
) -> Result<String, HttpConnectError> {
    let scheme = challenge
        .split_ascii_whitespace()
        .next()
        .unwrap_or_default();
    if scheme.eq_ignore_ascii_case("basic") {
        return Ok(basic_authorization(username, password));
    }
    if scheme.eq_ignore_ascii_case("digest") {
        let Ok(challenge) = DigestChallenge::parse(challenge) else {
            // Preserve the existing tunnel behavior for malformed or
            // unsupported Digest challenges.
            return Ok(basic_authorization(username, password));
        };
        return Ok(DigestResponse::compute_with_cnonce(
            username,
            password,
            "CONNECT",
            target,
            &challenge,
            1,
            "aria2rustcnonce",
        )
        .to_header_value());
    }
    Err(HttpConnectError::UnsupportedAuthentication(
        challenge.to_string(),
    ))
}

async fn send_request(
    stream: &mut TcpStream,
    options: &HttpConnectOptions<'_>,
    auth_header: Option<&str>,
) -> Result<(), HttpConnectError> {
    let target = format!("{}:{}", options.target_host, options.target_port);
    let mut headers = vec![
        ("User-Agent", options.user_agent),
        ("Host", target.as_str()),
    ];
    if let Some(auth) = auth_header {
        headers.push(("Proxy-Authorization", auth));
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
        let response = timeout(read_timeout, read_response_head(stream))
            .await
            .map_err(|_| HttpConnectError::Timeout)??;
        if (100..200).contains(&response.status_code) {
            debug!(
                status = response.status_code,
                "Skipping HTTP CONNECT informational response"
            );
            continue;
        }
        return Ok(response);
    }
}

async fn read_response_head(stream: &mut TcpStream) -> Result<ProxyResponse, HttpConnectError> {
    const MAX_HEADER_BYTES: usize = 8 * 1024 + 4;
    let mut header = Vec::with_capacity(512);
    let mut peek_buffer = [0; MAX_HEADER_BYTES];

    loop {
        let remaining = MAX_HEADER_BYTES.saturating_sub(header.len());
        if remaining == 0 {
            return Err(HttpConnectError::InvalidResponse(
                "header too large".to_string(),
            ));
        }
        let length = stream.peek(&mut peek_buffer[..remaining]).await?;
        if length == 0 {
            return Err(HttpConnectError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed while reading response head",
            )));
        }

        let response_end = find_header_end(&peek_buffer[..length]).map(|index| index + 4);
        let bytes_to_consume = response_end.unwrap_or(length);
        let start = header.len();
        header.resize(start + bytes_to_consume, 0);
        stream.read_exact(&mut header[start..]).await?;

        if response_end.is_some() {
            break;
        }
    }

    let response = parse_response_head(&header)
        .map_err(|error| HttpConnectError::InvalidResponse(error.to_string()))?;
    let headers = response
        .iter_headers()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    Ok(ProxyResponse {
        status_code: response.status_code,
        reason_phrase: response.reason_phrase,
        headers,
    })
}

async fn consume_response_body(stream: &mut TcpStream) {
    let mut buffer = [0; 4096];
    match timeout(Duration::from_secs(2), stream.read(&mut buffer)).await {
        Ok(Ok(length)) => debug!(length, "Consumed proxy authentication response body"),
        Ok(Err(error)) => debug!(%error, "Could not consume proxy authentication response body"),
        Err(_) => debug!("Timed out while consuming proxy authentication response body"),
    }
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
}

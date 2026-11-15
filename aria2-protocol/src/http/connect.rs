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
use super::request::{
    HttpRequestSerializationError, is_valid_http_field_value, try_serialize_http_request,
};

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
    /// A caller-provided field cannot be serialized safely.
    #[error("invalid HTTP CONNECT request field: {0}")]
    InvalidRequestField(&'static str),
}

struct ProxyResponse {
    status_code: u16,
    reason_phrase: String,
    headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyResponseBodyFraming {
    Chunked,
    ContentLength(usize),
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
    if !is_valid_http_field_value(options.user_agent) {
        return Err(HttpConnectError::InvalidRequestField("User-Agent"));
    }
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
    let request = try_serialize_http_request("CONNECT", &target, &headers, None).map_err(
        |error| match error {
            HttpRequestSerializationError::InvalidMethod => {
                HttpConnectError::InvalidRequestField("method")
            }
            HttpRequestSerializationError::InvalidTarget => {
                HttpConnectError::InvalidRequestField("target authority")
            }
            HttpRequestSerializationError::InvalidFieldName => {
                HttpConnectError::InvalidRequestField("header name")
            }
            HttpRequestSerializationError::InvalidFieldValue => {
                HttpConnectError::InvalidRequestField("header value")
            }
        },
    )?;
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
    match proxy_response_body_framing(headers)? {
        ProxyResponseBodyFraming::Chunked => {
            timeout(read_timeout, consume_chunked_body(stream))
                .await
                .map_err(|_| HttpConnectError::Timeout)??;
            Ok(())
        }
        ProxyResponseBodyFraming::ContentLength(0) => Ok(()),
        ProxyResponseBodyFraming::ContentLength(length)
            if length <= MAX_AUTH_RESPONSE_BODY_BYTES =>
        {
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
            Ok(())
        }
        ProxyResponseBodyFraming::ContentLength(_) => Err(HttpConnectError::InvalidResponse(
            "proxy authentication response body exceeds the discard limit".to_string(),
        )),
    }
}

fn proxy_response_body_framing(
    headers: &[(String, String)],
) -> Result<ProxyResponseBodyFraming, HttpConnectError> {
    let transfer_encodings = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
        .flat_map(|(_, value)| value.split(','))
        .map(str::trim)
        .collect::<Vec<_>>();

    if !transfer_encodings.is_empty() {
        let chunked_count = transfer_encodings
            .iter()
            .filter(|coding| coding.eq_ignore_ascii_case("chunked"))
            .count();
        if transfer_encodings.iter().any(|coding| coding.is_empty())
            || chunked_count != 1
            || !transfer_encodings
                .last()
                .is_some_and(|coding| coding.eq_ignore_ascii_case("chunked"))
        {
            return Err(HttpConnectError::InvalidResponse(
                "unsupported proxy authentication response transfer coding".to_string(),
            ));
        }
        return Ok(ProxyResponseBodyFraming::Chunked);
    }

    let content_lengths = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .flat_map(|(_, value)| value.split(','))
        .map(str::trim)
        .map(|value| {
            value.parse::<usize>().map_err(|_| {
                HttpConnectError::InvalidResponse(
                    "invalid proxy authentication response Content-Length".to_string(),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let Some(&length) = content_lengths.first() else {
        return Err(HttpConnectError::InvalidResponse(
            "proxy authentication response has no body framing".to_string(),
        ));
    };
    if content_lengths.iter().any(|candidate| *candidate != length) {
        return Err(HttpConnectError::InvalidResponse(
            "conflicting proxy authentication response Content-Length values".to_string(),
        ));
    }
    Ok(ProxyResponseBodyFraming::ContentLength(length))
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
mod tests;

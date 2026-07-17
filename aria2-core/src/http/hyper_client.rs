//! Direct hyper HTTP client for the hot download path.
//!
//! Bypasses reqwest overhead for simple HTTP range GETs by using hyper's
//! `Client` (via `hyper-util`) with a tuned `HttpConnector`. The connector
//! enables hyper's built-in Happy Eyeballs racing (`set_happy_eyeballs_timeout`)
//! so IPv6/IPv4 connects are raced with a 250ms IPv6 head start, matching the
//! behaviour of `crate::http::happy_eyeballs::connect_happy_eyeballs`.
//!
//! For proxy / complex-auth / HTTPS-with-custom-TLS paths, reqwest is still
//! used (see `client_pool.rs` and `connection.rs`); this module does NOT remove
//! reqwest.
//!
//! # hyper 1.x notes
//! hyper 1.x removed `hyper::Body`, `hyper::client::Client`, and
//! `hyper::server::Server`. This module uses:
//! - `hyper_util::client::legacy::Client` for the pooled HTTP client
//! - `http_body_util::{Full, Empty}` for request/response bodies
//! - `http_body_util::BodyExt` for body frame iteration
//! - `hyper::body::Incoming` for received response bodies

use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::Stream;
use http::StatusCode;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tracing::{debug, warn};

use crate::error::{Aria2Error, FatalError, RecoverableError, Result};

/// IPv6 head start for hyper's built-in Happy Eyeballs (RFC 8305: 250ms).
const HAPPY_EYEBALLS_HEAD_START: Duration = Duration::from_millis(250);

/// Idle connection lifetime in the hyper connection pool.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Max idle connections kept per host in the hyper connection pool.
const POOL_MAX_IDLE_PER_HOST: usize = 16;

/// TCP keepalive interval applied to pooled connections.
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);

/// Upper bound for the initial `BytesMut` capacity (avoids huge allocations
/// when a caller passes a very large `length` hint).
const MAX_INITIAL_CAPACITY: u64 = 4 * 1024 * 1024;

/// A lightweight HTTP client using hyper directly (via hyper-util).
///
/// Intended for the hot download path: simple HTTP range GETs with no proxy
/// and no complex auth. It avoids reqwest's abstraction overhead (middleware,
/// redirect-policy objects, default headers plumbing) while keeping a
/// connection pool for reuse across segments.
///
/// HTTPS is supported via hyper's connector only when a TLS backend is wired
/// in; for the plain-HTTP hot path this client is sufficient. HTTPS fallback
/// to reqwest remains available.
pub struct HyperDirectClient {
    client: Client<HttpConnector, Full<Bytes>>,
}

impl HyperDirectClient {
    /// Create a new `HyperDirectClient` with a tuned `HttpConnector`.
    pub fn new() -> Self {
        let mut connector = HttpConnector::new();
        connector.set_keepalive(Some(TCP_KEEPALIVE));
        connector.set_nodelay(true);
        // Enable hyper's built-in Happy Eyeballs: race IPv6/IPv4 connect
        // attempts with a 250ms IPv6 head start. This mirrors our
        // `connect_happy_eyeballs` logic but is applied inside hyper's
        // connector. A future custom `Service` connector could delegate to
        // `connect_happy_eyeballs` directly for finer control (e.g. a custom
        // DNS resolver).
        connector.set_happy_eyeballs_timeout(Some(HAPPY_EYEBALLS_HEAD_START));

        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Some(POOL_IDLE_TIMEOUT))
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            .build(connector);
        Self { client }
    }

    /// Download a byte range from `url` and collect it into `Bytes`.
    ///
    /// * `length = Some(n)` requests `bytes={offset}-{offset+n-1}`.
    /// * `length = None` requests `bytes={offset}-` (read to end of body).
    ///
    /// Status handling mirrors `HttpSegmentDownloader::download_range`:
    /// - `206 Partial Content`: OK.
    /// - `200 OK`: warn (server ignored Range) and proceed.
    /// - `416 Range Not Satisfiable`: recoverable error.
    /// - other `4xx`: fatal error.
    /// - `5xx`: recoverable server error.
    ///
    /// The body is accumulated into a `BytesMut` (sized by the smaller of
    /// `length` and `MAX_INITIAL_CAPACITY`) and frozen into a zero-copy
    /// `Bytes`.
    pub async fn download_range(
        &self,
        url: &str,
        offset: u64,
        length: Option<u64>,
    ) -> Result<Bytes> {
        // Zero-length explicit range: short-circuit without a network round trip.
        if matches!(length, Some(0)) {
            return Ok(Bytes::new());
        }

        let range_header = build_range_header(offset, length);
        debug!("hyper direct range request: {} ({})", range_header, url);

        let request = build_range_request(url, &range_header)?;

        let response = self.client.request(request).await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("hyper request failed: {e}"),
            })
        })?;

        let status = response.status();
        match status.as_u16() {
            206 => {}
            200 => warn!(
                "hyper direct: server returned 200 instead of 206 for Range request \
                 (offset={}, len={:?}) at {}",
                offset, length, url
            ),
            416 => {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!("Range not satisfiable: {range_header}"),
                    },
                ));
            }
            code if (400..500).contains(&code) => {
                return Err(Aria2Error::Fatal(FatalError::Config(format!(
                    "HTTP client error {code}: {url}"
                ))));
            }
            code if code >= 500 => {
                return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                    code,
                }));
            }
            _ => {}
        }

        // Accumulate body into a BytesMut, then freeze to Bytes.
        // hyper 1.x uses `BodyExt::collect()` which gathers all frames into
        // a single `Collected<Bytes>` that can be converted to `Bytes`.
        let initial_cap = length.unwrap_or(0).min(MAX_INITIAL_CAPACITY) as usize;
        let mut buf = bytes::BytesMut::with_capacity(initial_cap);
        let collected = response.into_body().collect().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("hyper stream read error: {e}"),
            })
        })?;
        buf.extend_from_slice(&collected.to_bytes());

        // An empty body is only an error when a concrete non-zero length was
        // requested (mirrors `HttpSegmentDownloader::download_range`).
        if buf.is_empty() && matches!(length, Some(l) if l > 0) {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!("Empty response for range {range_header} from {url}"),
                },
            ));
        }

        Ok(buf.freeze())
    }

    /// Download a byte range and return the body as a stream of `Bytes` chunks.
    ///
    /// The caller is responsible for consuming the stream (e.g. writing chunks
    /// to disk). Each item is `Result<Bytes, std::io::Error>`; hyper errors are
    /// mapped to `std::io::Error`.
    ///
    /// Status handling here is stricter than `download_range`: only `2xx` and
    /// `206` are accepted; any other status is an error before streaming begins.
    ///
    /// The returned stream is boxed because hyper 1.x body types (`Incoming`
    /// vs `Empty`) are not unifyable without type erasure.
    pub async fn download_range_stream(
        &self,
        url: &str,
        offset: u64,
        length: Option<u64>,
    ) -> Result<Pin<Box<dyn Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send>>>
    {
        // Zero-length explicit range: return an empty stream without a request.
        if matches!(length, Some(0)) {
            return Ok(Box::pin(futures::stream::empty()));
        }

        let range_header = build_range_header(offset, length);
        debug!("hyper direct range stream: {} ({})", range_header, url);

        let request = build_range_request(url, &range_header)?;

        let response = self.client.request(request).await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("hyper request failed: {e}"),
            })
        })?;

        let status = response.status();
        if !status.is_success() && status != StatusCode::PARTIAL_CONTENT {
            return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                code: status.as_u16(),
            }));
        }

        // Convert the hyper 1.x body into a `Stream<Item = Result<Bytes, _>>`
        // via `BodyExt::into_data_stream()`, then map the error type.
        let stream = response
            .into_body()
            .into_data_stream()
            .map(|res| {
                res.map_err(|e| std::io::Error::other(format!("hyper stream error: {e}")))
            });
        Ok(Box::pin(stream))
    }
}

impl Default for HyperDirectClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a `bytes={start}-{end}` Range header value.
///
/// `None` length produces an open-ended range (`bytes={offset}-`).
///
/// The end byte is computed as `offset + (len - 1)` using saturating
/// arithmetic: `len.saturating_sub(1)` is evaluated first (safe for any `len`),
/// then `offset.saturating_add(...)` prevents overflow panics for extreme
/// offsets. This mirrors `HttpSegmentDownloader`'s `offset + length - 1` but
/// is panic-free. For realistic file offsets the saturating guards never
/// engage.
fn build_range_header(offset: u64, length: Option<u64>) -> String {
    match length {
        Some(len) => format!(
            "bytes={}-{}",
            offset,
            offset.saturating_add(len.saturating_sub(1))
        ),
        None => format!("bytes={offset}-"),
    }
}

/// Build a `GET` request with the given Range header and the project user-agent.
fn build_range_request(url: &str, range_header: &str) -> Result<http::Request<Full<Bytes>>> {
    let uri: http::Uri = url
        .parse()
        .map_err(|e| Aria2Error::Parse(format!("invalid URL {url:?}: {e}")))?;

    http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("range", range_header)
        .header("user-agent", crate::constants::USER_AGENT)
        .body(Full::new(Bytes::new()))
        .map_err(|e| Aria2Error::Io(format!("failed to build request: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::body::Incoming;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    /// Test payload served by the local server (11 bytes: "hello world").
    const PAYLOAD: &[u8] = b"hello world";

    /// Handler that honours `Range` requests against `PAYLOAD`.
    ///
    /// * `bytes=0-4`   -> `206` with "hello"
    /// * `bytes=6-`    -> `206` with "world"
    /// * out-of-range  -> `416`
    /// * no Range      -> `200` with full payload
    async fn handle(
        req: http::Request<Incoming>,
    ) -> std::result::Result<http::Response<Full<Bytes>>, Infallible> {
        let range = req
            .headers()
            .get("range")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if let Some(spec) = range.strip_prefix("bytes=") {
            let (start_s, end_s) = spec.split_once('-').unwrap_or((spec, ""));
            let start: usize = start_s.parse().unwrap_or(0);

            if start >= PAYLOAD.len() {
                return Ok(http::Response::builder()
                    .status(http::StatusCode::RANGE_NOT_SATISFIABLE)
                    .header("content-range", format!("bytes */{}", PAYLOAD.len()))
                    .body(Full::new(Bytes::new()))
                    .unwrap());
            }

            let last = PAYLOAD.len() - 1;
            let end: usize = if end_s.is_empty() {
                last
            } else {
                end_s.parse::<usize>().unwrap_or(last).min(last)
            };
            let slice = &PAYLOAD[start..=end];

            return Ok(http::Response::builder()
                .status(http::StatusCode::PARTIAL_CONTENT)
                .header(
                    "content-range",
                    format!("bytes {}-{}/{}", start, end, PAYLOAD.len()),
                )
                .body(Full::new(Bytes::copy_from_slice(slice)))
                .unwrap());
        }

        Ok(http::Response::new(Full::new(Bytes::copy_from_slice(
            PAYLOAD,
        ))))
    }

    /// Spawn a local hyper 1.x server on an ephemeral port and return its address.
    ///
    /// Uses `tokio::net::TcpListener` + `hyper::server::conn::http1::Builder`
    /// (the hyper 1.x replacement for the removed `hyper::server::Server`).
    async fn spawn_server() -> SocketAddr {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = TcpListener::bind(addr).await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                // Accept connections until the listener is dropped (test ends).
                let (stream, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };
                let io = TokioIo::new(stream);
                tokio::spawn(async move {
                    let _ = http1::Builder::new()
                        .serve_connection(io, service_fn(handle))
                        .await;
                });
            }
        });
        local_addr
    }

    #[tokio::test]
    async fn test_download_range_partial() {
        let addr = spawn_server().await;
        let url = format!("http://{addr}/");
        let client = HyperDirectClient::new();
        let data = client.download_range(&url, 0, Some(5)).await.unwrap();
        assert_eq!(data.as_ref(), b"hello");
    }

    #[tokio::test]
    async fn test_download_range_open_ended() {
        let addr = spawn_server().await;
        let url = format!("http://{addr}/");
        let client = HyperDirectClient::new();
        // bytes=6- => "world"
        let data = client.download_range(&url, 6, None).await.unwrap();
        assert_eq!(data.as_ref(), b"world");
    }

    #[tokio::test]
    async fn test_download_range_zero_length() {
        let addr = spawn_server().await;
        let url = format!("http://{addr}/");
        let client = HyperDirectClient::new();
        let data = client.download_range(&url, 0, Some(0)).await.unwrap();
        assert!(data.is_empty());
    }

    #[tokio::test]
    async fn test_download_range_416_returns_error() {
        let addr = spawn_server().await;
        let url = format!("http://{addr}/");
        let client = HyperDirectClient::new();
        // offset beyond payload length => 416
        let result = client.download_range(&url, 100, Some(5)).await;
        assert!(result.is_err(), "expected error for 416 status");
        // Should be a recoverable TemporaryNetworkFailure.
        match result.unwrap_err() {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. }) => {}
            other => panic!("expected TemporaryNetworkFailure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_download_range_stream_partial() {
        let addr = spawn_server().await;
        let url = format!("http://{addr}/");
        let client = HyperDirectClient::new();
        let mut stream = client
            .download_range_stream(&url, 0, Some(5))
            .await
            .unwrap();

        let mut total = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("stream chunk should be ok");
            total.extend_from_slice(&chunk);
        }
        assert_eq!(&total, b"hello");
    }

    #[tokio::test]
    async fn test_download_range_stream_zero_length_yields_nothing() {
        let addr = spawn_server().await;
        let url = format!("http://{addr}/");
        let client = HyperDirectClient::new();
        let mut stream = client
            .download_range_stream(&url, 0, Some(0))
            .await
            .unwrap();

        let mut count = 0;
        while let Some(chunk) = stream.next().await {
            count += chunk.expect("chunk ok").len() as u32;
        }
        assert_eq!(count, 0, "zero-length range stream should yield no bytes");
    }

    #[test]
    fn test_build_range_header() {
        assert_eq!(build_range_header(0, Some(5)), "bytes=0-4");
        assert_eq!(build_range_header(10, Some(1)), "bytes=10-10");
        assert_eq!(build_range_header(6, None), "bytes=6-");
        // Extreme offset with len=1: end must equal offset (offset + 0), and
        // the saturating add must NOT panic nor wrap to offset-1.
        assert_eq!(
            build_range_header(u64::MAX, Some(1)),
            "bytes=18446744073709551615-18446744073709551615"
        );
    }

    #[test]
    fn test_default_equals_new() {
        // Both should construct without panic; structurally identical.
        let _a = HyperDirectClient::default();
        let _b = HyperDirectClient::new();
    }
}

//! HTTP segment downloader — range requests and streaming download logic.
//!
//! Contains the core `HttpSegmentDownloader` struct and its methods for
//! probing range support, downloading byte ranges (buffered and streaming),
//! and the `WriteChunk` type used to pipeline data to disk writers.

use bytes::BytesMut;
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::debug;

use crate::constants;
use crate::engine::command::ProgressUpdate;
use crate::engine::download_cookie::CookieHelper;
use crate::engine::http_segment_downloader::progress::SegmentProgress;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::auth::{AuthConfigFactory, AuthResolveOptions};
use crate::http::auth_challenge_handler::{self, AuthChallengeResult};
use crate::http::client_pool::ensure_rustls_provider;
use crate::http::response_processor::range::parse_content_range_value;
use crate::http::skip_response::MAX_REDIRECT_COUNT;
use crate::http::{AuthScheme, HttpAuthChallenge, HttpRequestPolicy};

/// A chunk of data to be written to disk at a specific offset.
pub struct WriteChunk {
    pub offset: u64,
    pub data: bytes::Bytes,
}

pub struct HttpSegmentDownloader {
    pub client: reqwest::Client,
    request_policy: HttpRequestPolicy,
    cookie_helper: Option<CookieHelper>,
    auth_options: Option<AuthResolveOptions>,
    netrc_path: Option<String>,
    last_peer_addr: std::sync::Mutex<Option<std::net::SocketAddr>>,
}

/// Validates that a partial response covers exactly the requested byte range.
fn validate_content_range(
    response: &reqwest::Response,
    offset: u64,
    length: u64,
    expected_entity_length: u64,
) -> Result<()> {
    let Some(value) = response.headers().get(reqwest::header::CONTENT_RANGE) else {
        return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
    };
    let value = value
        .to_str()
        .map_err(|_| Aria2Error::Recoverable(RecoverableError::CannotResume))?;
    let Some((start, end, total)) = parse_content_range_value(value) else {
        return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
    };
    if expected_entity_length != 0 && total != expected_entity_length {
        return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
    }
    let expected_end = offset.saturating_add(length.saturating_sub(1));
    if start != offset || end != expected_end || end < start {
        return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
    }
    Ok(())
}

fn classify_range_status(status: reqwest::StatusCode, range_header: &str) -> Option<Aria2Error> {
    let status_code = status.as_u16();
    match status_code {
        200 => Some(Aria2Error::Recoverable(RecoverableError::CannotResume)),
        416 => Some(Aria2Error::Recoverable(
            RecoverableError::RangeNotSatisfiable {
                range: range_header.to_string(),
            },
        )),
        401 | 407 => Some(Aria2Error::Recoverable(RecoverableError::HttpAuthFailed {
            message: format!("authentication failed: HTTP {status}"),
        })),
        404 => Some(Aria2Error::Recoverable(RecoverableError::ResourceNotFound)),
        code if code >= 500 || constants::RETRYABLE_HTTP_CODES.contains(&code) => {
            Some(Aria2Error::Recoverable(RecoverableError::ServerError {
                code,
            }))
        }
        400.. => Some(Aria2Error::Recoverable(
            RecoverableError::HttpProtocolError {
                message: format!("HTTP error: {status}"),
            },
        )),
        _ => None,
    }
}

impl HttpSegmentDownloader {
    /// Create a new `HttpSegmentDownloader`.
    #[must_use]
    pub fn new(client: &reqwest::Client) -> Self {
        Self::new_with_policy(client, HttpRequestPolicy::default())
    }

    #[must_use]
    pub fn new_with_policy(client: &reqwest::Client, request_policy: HttpRequestPolicy) -> Self {
        ensure_rustls_provider();
        Self {
            client: client.clone(),
            request_policy,
            cookie_helper: None,
            auth_options: None,
            netrc_path: None,
            last_peer_addr: std::sync::Mutex::new(None),
        }
    }

    /// Attach the task cookie store used by the concurrent download path.
    /// The helper is optional so the standalone range adapter keeps its
    /// existing small interface.
    pub fn with_cookie_helper(mut self, cookie_helper: CookieHelper) -> Self {
        self.cookie_helper = Some(cookie_helper);
        self
    }

    /// Attach per-download credentials for one bounded HTTP auth retry.
    pub fn with_auth_options(
        mut self,
        auth_options: AuthResolveOptions,
        netrc_path: Option<String>,
    ) -> Self {
        self.auth_options = Some(auth_options);
        self.netrc_path = netrc_path;
        self
    }

    /// Probe whether the server supports byte-range requests.
    pub async fn supports_range(
        &self,
        url: &str,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
    ) -> Result<bool> {
        let authorization = self.auth_options.as_ref().and_then(|auth_options| {
            let mut auth_factory = AuthConfigFactory::new();
            if let Some(path) = &self.netrc_path {
                let _ = auth_factory.load_netrc_file(std::path::Path::new(path));
            }
            let url = reqwest::Url::parse(url).ok()?;
            auth_factory.resolve_basic_authorization(&url, auth_options)
        });
        let req = self.request_policy.apply_with_basic_auth(
            self.client.head(url),
            cookie_header,
            headers,
            authorization.as_deref(),
        );
        let resp = req.send().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HEAD request failed: {}", e),
            })
        })?;

        if let Some(accept_ranges) = resp.headers().get("Accept-Ranges")
            && let Ok(value) = accept_ranges.to_str()
        {
            return Ok(value.to_lowercase().contains("bytes"));
        }

        let status = resp.status();
        if status.as_u16() >= 400 {
            return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                code: status.as_u16(),
            }));
        }

        Ok(false)
    }

    /// Download a byte range from a URL.
    ///
    /// Downloads the specified byte range from the given URL. If `progress_tx` is
    /// provided, periodic progress updates (segment-relative bytes downloaded) will
    /// be sent through the channel, enabling smooth progress reporting for RPC clients.
    pub fn remote_addr(response: &reqwest::Response) -> Option<std::net::SocketAddr> {
        response.remote_addr()
    }

    pub fn last_peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.last_peer_addr.lock().ok().and_then(|peer| *peer)
    }

    fn remember_peer(&self, peer: Option<std::net::SocketAddr>) {
        if let Ok(mut slot) = self.last_peer_addr.lock() {
            *slot = peer;
        }
    }

    /// Clear per-request connection metadata before reusing this downloader.
    pub fn clear_last_peer_addr(&self) {
        self.remember_peer(None);
    }

    /// Send a Range request through the same manual redirect seam as the
    /// sequential downloader. reqwest's client is configured with automatic
    /// redirects disabled so the final URI remains explicit and bounded.
    async fn send_range_request(
        &self,
        url: &str,
        range_header: &str,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
    ) -> Result<(reqwest::Response, reqwest::Url)> {
        let mut current_url = reqwest::Url::parse(url).map_err(|error| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("invalid HTTP Range URL {url}: {error}"),
            })
        })?;
        let mut redirect_count = 0u32;
        let mut auth_factory = self.auth_options.as_ref().map(|_| {
            let mut factory = AuthConfigFactory::new();
            if let Some(path) = &self.netrc_path
                && let Err(error) = factory.load_netrc_file(std::path::Path::new(path))
            {
                tracing::debug!(path, %error, "failed to load netrc for Range auth");
            }
            factory
        });

        loop {
            let dynamic_cookie_header = self
                .cookie_helper
                .as_ref()
                .map(|helper| helper.build_cookie_header_from_url(&current_url));
            let request_cookie_header = dynamic_cookie_header
                .as_deref()
                .filter(|value| !value.is_empty())
                .or_else(|| (redirect_count == 0).then_some(cookie_header).flatten());
            let authorization = self.auth_options.as_ref().and_then(|auth_options| {
                auth_factory.as_mut().and_then(|factory| {
                    factory.resolve_basic_authorization(&current_url, auth_options)
                })
            });
            let request = self.request_policy.apply_with_basic_auth(
                self.client
                    .get(current_url.as_str())
                    .header("Range", range_header),
                request_cookie_header,
                headers,
                authorization.as_deref(),
            );
            let response = request.send().await.map_err(|error| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("HTTP Range request failed: {error}"),
                })
            })?;

            let status_code = response.status().as_u16();
            let authentication_used = authorization.is_some()
                || self.request_policy.has_header("Authorization")
                || headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("Authorization"));
            if status_code == 401 || status_code == 407 {
                let Some(auth_options) = &self.auth_options else {
                    return Ok((response, current_url));
                };
                let is_proxy = status_code == 407;
                if authentication_used
                    || (!is_proxy && !auth_options.http_auth_challenge)
                    || (is_proxy && auth_options.proxy_user.is_none())
                {
                    return Ok((response, current_url));
                }

                let header_name = if is_proxy {
                    reqwest::header::PROXY_AUTHENTICATE
                } else {
                    reqwest::header::WWW_AUTHENTICATE
                };
                let auth_header = response
                    .headers()
                    .get(header_name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let scheme = auth_header
                    .as_deref()
                    .and_then(AuthScheme::from_header)
                    .or_else(|| (!is_proxy).then_some(AuthScheme::Basic));
                let Some(scheme) = scheme else {
                    return Ok((response, current_url));
                };
                let challenge = HttpAuthChallenge {
                    scheme: scheme.clone(),
                    realm: auth_header
                        .as_deref()
                        .map(crate::http::skip_response::HttpSkipResponseHandler::extract_realm)
                        .unwrap_or_default(),
                    is_proxy,
                    digest_challenge: if scheme == AuthScheme::Digest {
                        auth_header.as_deref().and_then(|header| {
                            crate::http::digest_auth::DigestAuthChallenge::parse(header).ok()
                        })
                    } else {
                        None
                    },
                };
                let Some(factory) = auth_factory.as_mut() else {
                    return Ok((response, current_url));
                };
                let result = auth_challenge_handler::handle_auth_challenge(
                    &challenge,
                    factory,
                    &current_url,
                    auth_options,
                    crate::http::request_response::HttpMethod::Get,
                    authentication_used,
                    1,
                );
                let AuthChallengeResult::RetryWithAuth {
                    authorization_header,
                    is_proxy,
                } = result
                else {
                    return Ok((response, current_url));
                };

                let header_name = if is_proxy {
                    "Proxy-Authorization"
                } else {
                    "Authorization"
                };
                let dynamic_cookie_header = self
                    .cookie_helper
                    .as_ref()
                    .map(|helper| helper.build_cookie_header_from_url(&current_url));
                let retry_cookie_header = dynamic_cookie_header
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .or_else(|| cookie_header.filter(|_| redirect_count == 0));
                let retry_request = self.request_policy.apply(
                    self.client
                        .get(current_url.as_str())
                        .header("Range", range_header),
                    retry_cookie_header,
                    &[(header_name.to_string(), authorization_header)],
                );
                let retry_response = retry_request.send().await.map_err(|error| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("HTTP Range auth retry failed: {error}"),
                    })
                })?;
                if let Some(helper) = &self.cookie_helper {
                    helper.extract_and_store_cookies(current_url.as_str(), &retry_response);
                }
                return Ok((retry_response, current_url));
            }
            if !matches!(status_code, 300..=303 | 307 | 308) {
                if let Some(helper) = &self.cookie_helper {
                    helper.extract_and_store_cookies(current_url.as_str(), &response);
                }
                return Ok((response, current_url));
            }

            if let Some(helper) = &self.cookie_helper {
                helper.extract_and_store_cookies(current_url.as_str(), &response);
            }

            if redirect_count >= MAX_REDIRECT_COUNT {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::HttpTooManyRedirects {
                        count: redirect_count,
                    },
                ));
            }

            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
                        message: format!("HTTP {status_code} redirect without Location header"),
                    })
                })?;
            current_url = current_url.join(location).map_err(|error| {
                Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
                    message: format!("failed to resolve redirect URL '{location}': {error}"),
                })
            })?;
            redirect_count += 1;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn download_range(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
        progress_tx: Option<&mpsc::Sender<ProgressUpdate>>,
        expected_entity_length: u64,
    ) -> Result<bytes::Bytes> {
        if length == 0 {
            return Ok(bytes::Bytes::new());
        }

        let range_header = format!("bytes={}-{}", offset, offset + length.saturating_sub(1));
        debug!("HTTP Range request: {} ({})", range_header, url);

        let (response, effective_url) = self
            .send_range_request(url, &range_header, cookie_header, headers)
            .await?;

        self.remember_peer(response.remote_addr());
        let status = response.status();
        if let Some(error) = classify_range_status(status, &range_header) {
            return Err(error);
        }
        if status.as_u16() == 206 {
            validate_content_range(&response, offset, length, expected_entity_length)?;
        }

        // Don't pre-allocate the full segment length — it can be very large
        // (16 MB+) and wastes memory if the download fails early.  Start with
        // a reasonable chunk size and let BytesMut grow organically.
        let initial_cap = (length as usize).min(256 * 1024);
        let mut data = BytesMut::with_capacity(initial_cap);
        let mut stream = response.bytes_stream();
        let mut last_reported_progress = 0u64;

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(bytes) => {
                    if data.len() as u64 + bytes.len() as u64 > length {
                        return Err(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: format!(
                                    "Response exceeded requested range length: expected {}, received more",
                                    length
                                ),
                            },
                        ));
                    }
                    data.extend_from_slice(&bytes);
                    // Report per-chunk progress if a progress channel is provided
                    let downloaded = data.len() as u64;
                    if let Some(tx) = progress_tx
                        && downloaded - last_reported_progress
                            >= constants::PROGRESS_UPDATE_BYTES as u64
                    {
                        let update = ProgressUpdate {
                            completed_bytes: offset + downloaded,
                            download_speed: 0,
                            upload_speed: 0,
                        };
                        // Progress is advisory; avoid stalling the data path
                        // when the bounded snapshot queue is temporarily full.
                        let _ = tx.try_send(update);
                        last_reported_progress = downloaded;
                    }
                }
                Err(e) => {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: format!("Stream read error: {}", e),
                        },
                    ));
                }
            }
        }

        if data.len() as u64 != length {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Incomplete response for range {}-{} from {}: expected {} bytes, received {}",
                        offset,
                        offset + length.saturating_sub(1),
                        effective_url,
                        length,
                        data.len()
                    ),
                },
            ));
        }

        // Freeze BytesMut to immutable Bytes (zero-cost conversion)
        Ok(data.freeze())
    }

    /// Streaming variant of [`download_range`](Self::download_range).
    ///
    /// Instead of accumulating all chunks in memory and returning the full buffer,
    /// this method sends each chunk to `write_tx` as it arrives from the network,
    /// enabling immediate disk writes without the 16 MB per-segment memory overhead.
    ///
    /// Returns the total number of bytes downloaded on success.
    #[allow(clippy::too_many_arguments)]
    pub async fn download_range_streaming(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
        progress_tx: Option<&mpsc::Sender<ProgressUpdate>>,
        write_tx: &mpsc::Sender<WriteChunk>,
        expected_entity_length: u64,
    ) -> Result<u64> {
        self.download_range_streaming_inner(
            url,
            offset,
            length,
            cookie_header,
            headers,
            progress_tx.map(StreamingProgress::Channel),
            write_tx,
            expected_entity_length,
        )
        .await
    }

    /// Streaming range download with lock-free progress aggregation for the
    /// concurrent HTTP scheduler.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn download_range_streaming_with_progress(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
        progress: Option<&SegmentProgress>,
        write_tx: &mpsc::Sender<WriteChunk>,
        expected_entity_length: u64,
    ) -> Result<u64> {
        self.download_range_streaming_inner(
            url,
            offset,
            length,
            cookie_header,
            headers,
            progress.map(StreamingProgress::Segment),
            write_tx,
            expected_entity_length,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_range_streaming_inner(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        cookie_header: Option<&str>,
        headers: &[(String, String)],
        progress: Option<StreamingProgress<'_>>,
        write_tx: &mpsc::Sender<WriteChunk>,
        expected_entity_length: u64,
    ) -> Result<u64> {
        if length == 0 {
            return Ok(0);
        }

        let range_header = format!("bytes={}-{}", offset, offset + length.saturating_sub(1));
        debug!("HTTP Range request (streaming): {} ({})", range_header, url);

        let (response, effective_url) = self
            .send_range_request(url, &range_header, cookie_header, headers)
            .await?;

        self.remember_peer(response.remote_addr());
        let status = response.status();
        if let Some(error) = classify_range_status(status, &range_header) {
            return Err(error);
        }
        if status.as_u16() == 206 {
            validate_content_range(&response, offset, length, expected_entity_length)?;
        }

        let mut stream = response.bytes_stream();
        let mut current_offset = offset;
        let mut total_written = 0u64;
        let mut last_reported_progress = 0u64;

        while let Some(chunk_result) = stream.next().await {
            let bytes = chunk_result.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("Stream read error: {}", e),
                })
            })?;
            let chunk_len = bytes.len() as u64;
            if chunk_len > 0
                && let Some(StreamingProgress::Segment(segment)) = progress
            {
                segment.record_network_activity();
            }
            if total_written.saturating_add(chunk_len) > length {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!(
                            "Response exceeded requested range length: expected {}, received at least {}",
                            length,
                            total_written.saturating_add(chunk_len)
                        ),
                    },
                ));
            }

            // Send chunk to writer immediately — no accumulation
            if write_tx
                .send(WriteChunk {
                    offset: current_offset,
                    data: bytes,
                })
                .await
                .is_err()
            {
                return Err(Aria2Error::DownloadFailed(
                    "download writer channel closed".into(),
                ));
            }

            current_offset += chunk_len;
            total_written += chunk_len;

            // Report progress at the same byte threshold without scheduling a
            // receiver task for every segment.
            if progress.is_some()
                && total_written - last_reported_progress >= constants::PROGRESS_UPDATE_BYTES as u64
            {
                match progress {
                    Some(StreamingProgress::Channel(tx)) => {
                        let update = ProgressUpdate {
                            completed_bytes: offset + total_written,
                            download_speed: 0,
                            upload_speed: 0,
                        };
                        let _ = tx.send(update).await;
                    }
                    Some(StreamingProgress::Segment(segment)) => {
                        segment.record(total_written);
                    }
                    None => unreachable!("progress presence checked above"),
                }
                last_reported_progress = total_written;
            }
        }

        if total_written != length {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Incomplete response for range {}-{} from {}: expected {} bytes, received {}",
                        offset,
                        offset + length.saturating_sub(1),
                        effective_url,
                        length,
                        total_written
                    ),
                },
            ));
        }

        Ok(total_written)
    }
}

#[derive(Clone, Copy)]
enum StreamingProgress<'a> {
    Channel(&'a mpsc::Sender<ProgressUpdate>),
    Segment(&'a SegmentProgress),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::auth::AuthResolveOptions;
    use std::time::Duration;

    fn has_header(request: &str, name: &str, value: &str) -> bool {
        request.lines().any(|line| {
            line.split_once(':').is_some_and(|(key, actual)| {
                key.eq_ignore_ascii_case(name) && actual.trim() == value
            })
        })
    }

    fn has_header_name(request: &str, name: &str) -> bool {
        request.lines().any(|line| {
            line.split_once(':')
                .is_some_and(|(key, _)| key.eq_ignore_ascii_case(name))
        })
    }

    fn header_value<'a>(request: &'a str, name: &str) -> Option<&'a str> {
        request.lines().find_map(|line| {
            line.split_once(':')
                .and_then(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.trim()))
        })
    }

    fn digest_parameter<'a>(header: &'a str, name: &str) -> Option<&'a str> {
        header
            .strip_prefix("Digest ")
            .and_then(|parameters| {
                parameters.split(", ").find_map(|parameter| {
                    parameter
                        .split_once('=')
                        .and_then(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value))
                })
            })
            .map(|value| value.trim_matches('"'))
    }

    #[tokio::test]
    async fn test_supports_range_no_server() {
        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(100))
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let result = dl
            .supports_range("http://127.0.0.1:1/nonexistent", None, &[])
            .await;
        assert!(result.is_err(), "should fail for unreachable host");
    }

    #[tokio::test]
    async fn test_download_range_zero_length() {
        ensure_rustls_provider();
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);
        let result = dl
            .download_range("http://example.com", 0, 0, None, &[], None, 0)
            .await;
        assert!(result.is_ok(), "zero-length range should return empty vec");
        assert!(result.expect("already checked ok").is_empty());
    }

    #[tokio::test]
    async fn test_downloader_creation() {
        ensure_rustls_provider();
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);
        let _dl2 = HttpSegmentDownloader::new(&dl.client);
    }

    #[tokio::test]
    async fn test_download_range_with_mock_http_416() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");

        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut buf = [0u8; 2048];
            // Use read() instead of read_exact() to avoid blocking on exact byte count
            let _n = stream.read(&mut buf).await.expect("read should succeed");
            stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.expect("write should succeed");
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let url = format!("http://{}", addr);
        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);

        let result = dl
            .download_range(&url, 99999, 100, None, &[], None, 0)
            .await;
        assert!(result.is_err(), "416 should be an error");

        // Wait for server with timeout
        let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
    }

    #[tokio::test]
    async fn test_download_range_rejects_server_that_ignores_range() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut request = [0u8; 2048];
            let _ = stream
                .read(&mut request)
                .await
                .expect("read should succeed");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
                )
                .await
                .expect("write should succeed");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let url = format!("http://{addr}/no-range");

        let result = dl.download_range(&url, 5, 5, None, &[], None, 10).await;
        assert!(matches!(
            result,
            Err(Aria2Error::Recoverable(RecoverableError::CannotResume))
        ));

        tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("fixture should finish")
            .expect("fixture task should succeed");
    }

    #[tokio::test]
    async fn test_download_range_rejects_mismatched_content_range() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut request = [0u8; 2048];
            let _ = stream
                .read(&mut request)
                .await
                .expect("read should succeed");
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-4/10\r\nContent-Length: 5\r\nConnection: close\r\n\r\n01234",
                )
                .await
                .expect("write should succeed");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let url = format!("http://{addr}/wrong-range");

        let result = dl.download_range(&url, 5, 5, None, &[], None, 10).await;
        assert!(matches!(
            result,
            Err(Aria2Error::Recoverable(RecoverableError::CannotResume))
        ));

        tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("fixture should finish")
            .expect("fixture task should succeed");
    }

    #[test]
    fn test_classify_range_status_keeps_terminal_and_retryable_http_errors_distinct() {
        assert!(matches!(
            classify_range_status(reqwest::StatusCode::NOT_FOUND, "bytes=0-9"),
            Some(Aria2Error::Recoverable(RecoverableError::ResourceNotFound))
        ));
        assert!(matches!(
            classify_range_status(reqwest::StatusCode::SERVICE_UNAVAILABLE, "bytes=0-9"),
            Some(Aria2Error::Recoverable(RecoverableError::ServerError {
                code: 503
            }))
        ));
        assert!(matches!(
            classify_range_status(reqwest::StatusCode::TOO_MANY_REQUESTS, "bytes=0-9"),
            Some(Aria2Error::Recoverable(RecoverableError::ServerError {
                code: 429
            }))
        ));
        assert!(matches!(
            classify_range_status(reqwest::StatusCode::REQUEST_TIMEOUT, "bytes=0-9"),
            Some(Aria2Error::Recoverable(RecoverableError::ServerError {
                code: 408
            }))
        ));
    }

    #[tokio::test]
    async fn test_download_range_maps_not_found_to_resource_not_found() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut request = [0u8; 2048];
            let _n = stream
                .read(&mut request)
                .await
                .expect("read should succeed");
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write should succeed");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let url = format!("http://{addr}/missing");

        let result = dl.download_range(&url, 0, 10, None, &[], None, 20).await;
        assert!(matches!(
            result,
            Err(Aria2Error::Recoverable(RecoverableError::ResourceNotFound))
        ));

        tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("404 fixture should finish")
            .expect("404 fixture task should succeed");
    }

    #[tokio::test]
    async fn test_download_range_streaming_maps_ordinary_4xx_to_http_protocol_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut request = [0u8; 2048];
            let _n = stream
                .read(&mut request)
                .await
                .expect("read should succeed");
            stream
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write should succeed");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let url = format!("http://{addr}/forbidden");
        let (write_tx, _write_rx) = mpsc::channel(8);

        let result = dl
            .download_range_streaming(&url, 0, 10, None, &[], None, &write_tx, 20)
            .await;
        assert!(matches!(
            result,
            Err(Aria2Error::Recoverable(
                RecoverableError::HttpProtocolError { message }
            )) if message.contains("403")
        ));

        tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("403 fixture should finish")
            .expect("403 fixture task should succeed");
    }

    #[tokio::test]
    async fn test_supports_range_header_parsing() {
        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);

        match dl
            .supports_range(
                "http://invalid-host-name-that-does-not-exist-12345.com/",
                None,
                &[],
            )
            .await
        {
            Ok(supports) => {
                eprintln!(
                    "[WARN] Unexpected success for invalid host, supports={:?}",
                    supports
                );
            }
            Err(e) => {
                println!("Expected network error for invalid host: {:?}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_download_range_streaming_short_body_is_rejected() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await.expect("read should succeed");
            stream
                .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 5\r\nConnection: close\r\n\r\n01234")
                .await
                .expect("write should succeed");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let url = format!("http://{}", addr);
        let (write_tx, _write_rx) = mpsc::channel(8);

        let result = dl
            .download_range_streaming(&url, 0, 10, None, &[], None, &write_tx, 20)
            .await;
        assert!(result.is_err(), "short streaming body should be rejected");
        let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
    }

    #[tokio::test]
    async fn test_download_range_short_body_is_rejected() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await.expect("read should succeed");
            stream
                .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 5\r\nConnection: close\r\n\r\n01234")
                .await
                .expect("write should succeed");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let url = format!("http://{}", addr);

        let result = dl.download_range(&url, 0, 10, None, &[], None, 20).await;
        assert!(result.is_err(), "short 206 body should be rejected");
        let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
    }

    #[tokio::test]
    async fn test_download_range_status_code_handling() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf).await.expect("read should succeed");
            stream
                .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789")
                .await
                .expect("write should succeed");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let url = format!("http://{}", addr);

        let result = dl
            .download_range(&url, 0, 10, None, &[], None, 20)
            .await
            .expect("matching 206 should succeed");
        assert_eq!(result.as_ref(), b"0123456789");
        let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
    }

    #[tokio::test]
    async fn test_download_range_follows_redirect_before_validating_range() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            for response in [
                b"HTTP/1.1 302 Found\r\nLocation: /target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .as_slice(),
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789"
                    .as_slice(),
            ] {
                let (mut stream, _) = listener.accept().await.expect("accept should succeed");
                let mut request = [0u8; 2048];
                let bytes = stream.read(&mut request).await.expect("read should succeed");
                assert!(bytes > 0, "request should not be empty");
                stream.write_all(response).await.expect("write should succeed");
            }
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let url = format!("http://{addr}/source");

        let result = dl
            .download_range(&url, 0, 10, None, &[], None, 20)
            .await
            .expect("range download should follow redirect");
        assert_eq!(result.as_ref(), b"0123456789");

        tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("redirect fixture should finish")
            .expect("redirect fixture task should succeed");
    }

    #[tokio::test]
    async fn test_streaming_range_follows_redirect_before_validating_range() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            for response in [
                b"HTTP/1.1 302 Found\r\nLocation: /target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .as_slice(),
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789"
                    .as_slice(),
            ] {
                let (mut stream, _) = listener.accept().await.expect("accept should succeed");
                let mut request = [0u8; 2048];
                let bytes = stream.read(&mut request).await.expect("read should succeed");
                assert!(bytes > 0, "request should not be empty");
                stream.write_all(response).await.expect("write should succeed");
            }
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let dl = HttpSegmentDownloader::new(&client);
        let url = format!("http://{addr}/source");
        let (write_tx, mut write_rx) = mpsc::channel(8);

        let total = dl
            .download_range_streaming(&url, 0, 10, None, &[], None, &write_tx, 20)
            .await
            .expect("streaming range download should follow redirect");
        drop(write_tx);

        let mut output = Vec::new();
        while let Some(chunk) = write_rx.recv().await {
            output.extend_from_slice(&chunk.data);
        }
        assert_eq!(total, 10);
        assert_eq!(output, b"0123456789");

        tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("redirect fixture should finish")
            .expect("redirect fixture task should succeed");
    }

    #[tokio::test]
    async fn test_range_redirect_propagates_set_cookie_to_next_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut redirect_stream, _) = listener.accept().await.expect("accept redirect");
            let mut redirect_request = [0u8; 2048];
            let bytes = redirect_stream
                .read(&mut redirect_request)
                .await
                .expect("read redirect request");
            assert!(bytes > 0, "redirect request should not be empty");
            redirect_stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: /target\r\nSet-Cookie: sid=abc; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write redirect response");

            let (mut target_stream, _) = listener.accept().await.expect("accept target");
            let mut target_request = [0u8; 2048];
            let bytes = target_stream
                .read(&mut target_request)
                .await
                .expect("read target request");
            let target_request = String::from_utf8_lossy(&target_request[..bytes]);
            assert!(
                target_request
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("Cookie: sid=abc")),
                "redirect target must receive the cookie set by the redirect response: {target_request}"
            );
            target_stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
                )
                .await
                .expect("write target response");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let cookie_helper = CookieHelper::new(
            std::sync::Arc::new(crate::http::cookie::CookieStorage::new()),
            None,
        );
        let dl = HttpSegmentDownloader::new(&client).with_cookie_helper(cookie_helper);
        let url = format!("http://{addr}/source");

        let result = dl
            .download_range(&url, 0, 10, None, &[], None, 20)
            .await
            .expect("range download should retain redirect cookies");
        assert_eq!(result.as_ref(), b"0123456789");

        tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("cookie redirect fixture should finish")
            .expect("cookie redirect fixture task should succeed");
    }

    #[tokio::test]
    async fn test_range_retries_basic_auth_challenge() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut first_stream, _) = listener.accept().await.expect("accept first request");
            let mut first_request = [0u8; 4096];
            let bytes = first_stream
                .read(&mut first_request)
                .await
                .expect("read first request");
            let first_request = String::from_utf8_lossy(&first_request[..bytes]);
            assert!(first_request.starts_with("GET /file.bin HTTP/1.1"));
            assert!(has_header(&first_request, "Range", "bytes=0-9"));
            assert!(!has_header_name(&first_request, "Authorization"));
            first_stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"download\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write auth challenge");

            let (mut second_stream, _) = listener.accept().await.expect("accept retry request");
            let mut second_request = [0u8; 4096];
            let bytes = second_stream
                .read(&mut second_request)
                .await
                .expect("read retry request");
            let second_request = String::from_utf8_lossy(&second_request[..bytes]);
            assert!(has_header(&second_request, "Range", "bytes=0-9"));
            assert!(has_header(
                &second_request,
                "Authorization",
                "Basic dXNlcjpwYXNz"
            ));
            second_stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
                )
                .await
                .expect("write authenticated response");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let auth_options = AuthResolveOptions {
            http_auth_challenge: true,
            http_user: Some("user".to_string()),
            http_passwd: Some("pass".to_string()),
            ..AuthResolveOptions::default()
        };
        let dl = HttpSegmentDownloader::new(&client).with_auth_options(auth_options, None);
        let url = format!("http://{addr}/file.bin");

        let result = dl
            .download_range(&url, 0, 10, None, &[], None, 20)
            .await
            .expect("authenticated range download should succeed");
        assert_eq!(result.as_ref(), b"0123456789");

        tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("auth fixture should finish")
            .expect("auth fixture task should succeed");
    }

    #[tokio::test]
    async fn test_range_sends_preemptive_basic_auth_credentials() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut request = [0u8; 4096];
            let bytes = stream.read(&mut request).await.expect("read request");
            let request = String::from_utf8_lossy(&request[..bytes]);
            assert!(has_header(&request, "Range", "bytes=0-9"));
            assert!(has_header(&request, "Authorization", "Basic dXNlcjpwYXNz"));
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
                )
                .await
                .expect("write authenticated response");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let auth_options = AuthResolveOptions {
            http_user: Some("user".to_string()),
            http_passwd: Some("pass".to_string()),
            ..AuthResolveOptions::default()
        };
        let dl = HttpSegmentDownloader::new(&client).with_auth_options(auth_options, None);
        let url = format!("http://{addr}/file.bin");

        let result = dl
            .download_range(&url, 0, 10, None, &[], None, 20)
            .await
            .expect("preemptively authenticated range download should succeed");
        assert_eq!(result.as_ref(), b"0123456789");

        tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("preemptive auth fixture should finish")
            .expect("preemptive auth fixture task should succeed");
    }

    #[tokio::test]
    async fn test_range_verifies_digest_auth_response() {
        use md5::{Digest, Md5};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        fn md5_hex(value: &str) -> String {
            let mut hasher = Md5::new();
            hasher.update(value.as_bytes());
            hex::encode(hasher.finalize())
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut first_stream, _) = listener.accept().await.expect("accept first request");
            let mut first_request = [0u8; 4096];
            let bytes = first_stream
                .read(&mut first_request)
                .await
                .expect("read first request");
            let first_request = String::from_utf8_lossy(&first_request[..bytes]);
            assert!(!has_header_name(&first_request, "Authorization"));
            first_stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"download\", nonce=\"fixed-nonce\", qop=\"auth\", algorithm=MD5, opaque=\"opaque\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write digest challenge");

            let (mut second_stream, _) = listener.accept().await.expect("accept retry request");
            let mut second_request = [0u8; 4096];
            let bytes = second_stream
                .read(&mut second_request)
                .await
                .expect("read retry request");
            let second_request = String::from_utf8_lossy(&second_request[..bytes]);
            let authorization =
                header_value(&second_request, "Authorization").expect("digest auth header");
            assert!(authorization.starts_with("Digest "));
            assert_eq!(digest_parameter(authorization, "username"), Some("user"));
            assert_eq!(digest_parameter(authorization, "realm"), Some("download"));
            assert_eq!(
                digest_parameter(authorization, "nonce"),
                Some("fixed-nonce")
            );
            assert_eq!(digest_parameter(authorization, "uri"), Some("/file.bin"));
            assert_eq!(digest_parameter(authorization, "qop"), Some("auth"));
            assert_eq!(digest_parameter(authorization, "nc"), Some("00000001"));
            assert_eq!(digest_parameter(authorization, "opaque"), Some("opaque"));

            let cnonce = digest_parameter(authorization, "cnonce").expect("digest cnonce");
            let response = digest_parameter(authorization, "response").expect("digest response");
            let ha1 = md5_hex("user:download:pass");
            let ha2 = md5_hex("GET:/file.bin");
            let expected = md5_hex(&format!("{ha1}:fixed-nonce:00000001:{cnonce}:auth:{ha2}"));
            assert_eq!(response, expected);

            second_stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
                )
                .await
                .expect("write authenticated response");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let auth_options = AuthResolveOptions {
            http_auth_challenge: true,
            http_user: Some("user".to_string()),
            http_passwd: Some("pass".to_string()),
            ..AuthResolveOptions::default()
        };
        let dl = HttpSegmentDownloader::new(&client).with_auth_options(auth_options, None);
        let url = format!("http://{addr}/file.bin");

        let result = dl
            .download_range(&url, 0, 10, None, &[], None, 20)
            .await
            .expect("digest-authenticated range download should succeed");
        assert_eq!(result.as_ref(), b"0123456789");

        tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("digest auth fixture should finish")
            .expect("digest auth fixture task should succeed");
    }

    #[tokio::test]
    async fn test_range_retries_proxy_auth_challenge() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            let (mut first_stream, _) = listener.accept().await.expect("accept first request");
            let mut first_request = [0u8; 4096];
            let bytes = first_stream
                .read(&mut first_request)
                .await
                .expect("read first request");
            let first_request = String::from_utf8_lossy(&first_request[..bytes]);
            assert!(!has_header_name(&first_request, "Proxy-Authorization"));
            first_stream
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write proxy auth challenge");

            let (mut second_stream, _) = listener.accept().await.expect("accept retry request");
            let mut second_request = [0u8; 4096];
            let bytes = second_stream
                .read(&mut second_request)
                .await
                .expect("read retry request");
            let second_request = String::from_utf8_lossy(&second_request[..bytes]);
            assert!(has_header(
                &second_request,
                "Proxy-Authorization",
                "Basic dXNlcjpwYXNz"
            ));
            assert!(has_header(&second_request, "Range", "bytes=0-9"));
            second_stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
                )
                .await
                .expect("write authenticated response");
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let auth_options = AuthResolveOptions {
            proxy_user: Some("user".to_string()),
            proxy_passwd: Some("pass".to_string()),
            ..AuthResolveOptions::default()
        };
        let dl = HttpSegmentDownloader::new(&client).with_auth_options(auth_options, None);
        let url = format!("http://{addr}/file.bin");

        let result = dl
            .download_range(&url, 0, 10, None, &[], None, 20)
            .await
            .expect("proxy-authenticated range download should succeed");
        assert_eq!(result.as_ref(), b"0123456789");

        tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("proxy auth fixture should finish")
            .expect("proxy auth fixture task should succeed");
    }

    #[tokio::test]
    async fn test_range_auth_credentials_are_not_retried_after_failure() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server_handle = tokio::spawn(async move {
            for expected_auth in [None, Some("Authorization: Basic d3Jvbmc6Y3JlZHM=")].iter() {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let mut request = [0u8; 4096];
                let bytes = stream.read(&mut request).await.expect("read request");
                let request = String::from_utf8_lossy(&request[..bytes]);
                match expected_auth {
                    Some(header) => {
                        let (_, value) = header.split_once(':').expect("test header");
                        assert!(has_header(&request, "Authorization", value.trim()));
                    }
                    None => assert!(!has_header_name(&request, "Authorization")),
                }
                stream
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"download\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write auth challenge");
            }
        });

        ensure_rustls_provider();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client build should succeed");
        let auth_options = AuthResolveOptions {
            http_auth_challenge: true,
            http_user: Some("wrong".to_string()),
            http_passwd: Some("creds".to_string()),
            ..AuthResolveOptions::default()
        };
        let dl = HttpSegmentDownloader::new(&client).with_auth_options(auth_options, None);
        let url = format!("http://{addr}/file.bin");

        let result = dl.download_range(&url, 0, 10, None, &[], None, 20).await;
        assert!(matches!(
            result,
            Err(Aria2Error::Recoverable(
                RecoverableError::HttpAuthFailed { .. }
            ))
        ));

        tokio::time::timeout(Duration::from_secs(2), server_handle)
            .await
            .expect("failed-auth fixture should finish")
            .expect("failed-auth fixture task should succeed");
    }
}

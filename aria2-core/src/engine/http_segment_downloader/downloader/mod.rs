//! HTTP segment downloader — range requests and streaming download logic.
//!
//! Contains the core `HttpSegmentDownloader` struct and its methods for
//! probing range support, downloading byte ranges (buffered and streaming),
//! and the `WriteChunk` type used to pipeline data to disk writers.

use crate::constants;
use crate::engine::download_cookie::CookieHelper;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::HttpRequestPolicy;
use crate::http::auth::{AuthConfigFactory, AuthResolveOptions};
use crate::http::client_pool::ensure_rustls_provider;
use crate::http::response_processor::range::parse_content_range_value;

mod range;
mod streaming;

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;

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
        self.request_policy.wait_before_request().await;
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

        let status = resp.status();
        if status.as_u16() >= 400 {
            return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                code: status.as_u16(),
            }));
        }

        if let Some(accept_ranges) = resp.headers().get("Accept-Ranges")
            && let Ok(value) = accept_ranges.to_str()
        {
            return Ok(value.to_lowercase().contains("bytes"));
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
}

//! HTTP segment downloader — range requests and streaming download logic.
//!
//! Contains the core `HttpSegmentDownloader` struct and its methods for
//! probing range support, downloading byte ranges (buffered and streaming),
//! and the `WriteChunk` type used to pipeline data to disk writers.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::Notify;

use crate::constants;
use crate::engine::http::cookie_helper::CookieHelper;
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

#[derive(Debug, Clone)]
pub(crate) struct RangeProbeResult {
    pub(crate) supports_range: bool,
    pub(crate) effective_url: String,
    pub(crate) origin_sensitive_headers_allowed: bool,
    pub(crate) total_length: u64,
    pub(crate) version: Option<reqwest::Version>,
}

pub struct WriteChunk {
    pub offset: u64,
    pub data: bytes::Bytes,
}

/// Default byte budget for queued HTTP write chunks per download task.
pub(crate) const WRITE_QUEUE_BUDGET_BYTES: usize = 4 * 1024 * 1024;

/// Byte-based backpressure for the internal write queue.
///
/// The public `WriteChunk` shape stays unchanged. Producers acquire a permit
/// for the chunk length before enqueueing it, and the permit is released only
/// after the consumer has written that chunk to disk.
#[derive(Clone)]
pub(crate) struct WriteBudget {
    inner: Arc<WriteBudgetInner>,
}

struct WriteBudgetInner {
    limit: usize,
    used: AtomicUsize,
    changed: Notify,
}

pub(crate) struct WriteBudgetPermit {
    budget: WriteBudget,
    bytes: usize,
}

impl Drop for WriteBudgetPermit {
    fn drop(&mut self) {
        self.budget
            .inner
            .used
            .fetch_sub(self.bytes, Ordering::Release);
        self.budget.inner.changed.notify_waiters();
    }
}

impl WriteBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(WriteBudgetInner {
                limit: limit.max(1),
                used: AtomicUsize::new(0),
                changed: Notify::new(),
            }),
        }
    }

    pub(crate) async fn acquire(&self, bytes: usize) -> WriteBudgetPermit {
        let bytes = bytes.max(1);
        loop {
            let notified = self.inner.changed.notified();
            let used = self.inner.used.load(Ordering::Acquire);
            let next = used.saturating_add(bytes);
            // An oversized chunk cannot be split here; admit it alone so the
            // write path cannot deadlock on its own budget.
            if (next <= self.inner.limit || bytes > self.inner.limit)
                && self
                    .inner
                    .used
                    .compare_exchange(used, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                return WriteBudgetPermit {
                    budget: self.clone(),
                    bytes,
                };
            }
            notified.await;
        }
    }
}

/// Queue item that carries the byte-budget permit with the payload.
#[doc(hidden)]
pub struct QueuedWriteChunk {
    pub(crate) offset: u64,
    pub(crate) data: bytes::Bytes,
    permit: WriteBudgetPermit,
}

impl QueuedWriteChunk {
    pub(crate) fn new(offset: u64, data: bytes::Bytes, permit: WriteBudgetPermit) -> Self {
        Self {
            offset,
            data,
            permit,
        }
    }

    pub(crate) fn into_parts(self) -> (WriteChunk, WriteBudgetPermit) {
        (
            WriteChunk {
                offset: self.offset,
                data: self.data,
            },
            self.permit,
        )
    }
}

/// Internal failure metadata used by the concurrent scheduler. The public
/// downloader API continues to expose only the existing `Aria2Error`.
pub(crate) struct RangeDownloadFailure {
    pub(crate) error: Aria2Error,
    pub(crate) explicit_size_rejection: bool,
    /// Retry-After of a 429/503 response.
    pub(crate) retry_after: Option<std::time::Duration>,
}

impl From<Aria2Error> for RangeDownloadFailure {
    fn from(error: Aria2Error) -> Self {
        Self {
            error,
            explicit_size_rejection: false,
            retry_after: None,
        }
    }
}

/// Longest Retry-After honoured; a longer one is capped to this.
const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Parse Retry-After as delay-seconds or an HTTP date, capped.
pub(crate) fn parse_retry_after(
    headers: &reqwest::header::HeaderMap,
) -> Option<std::time::Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let now = i64::try_from(now).ok()?;
    aria2_protocol::http::parse_retry_after(value, now).map(|delay| delay.min(MAX_RETRY_AFTER))
}

#[cfg(test)]
mod write_budget_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn write_budget_waits_until_queued_permits_are_released() {
        let budget = WriteBudget::new(10);
        let first = budget.acquire(6).await;
        let second = budget.acquire(4).await;
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(QueuedWriteChunk::new(
            0,
            bytes::Bytes::from_static(b"123456"),
            first,
        ))
        .await
        .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(50), budget.acquire(1))
                .await
                .is_err()
        );

        drop(rx);
        let third = budget.acquire(1).await;
        drop(second);
        drop(third);
    }
}

pub struct HttpSegmentDownloader {
    pub client: reqwest::Client,
    request_policy: HttpRequestPolicy,
    cookie_helper: Option<CookieHelper>,
    auth_options: Option<AuthResolveOptions>,
    auth_factory: std::sync::Arc<std::sync::Mutex<AuthConfigFactory>>,
    origin_sensitive_headers_allowed: bool,
    last_peer_addr: std::sync::Mutex<Option<std::net::SocketAddr>>,
    last_http_version: std::sync::Mutex<Option<reqwest::Version>>,
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
            auth_factory: std::sync::Arc::new(std::sync::Mutex::new(AuthConfigFactory::new())),
            origin_sensitive_headers_allowed: true,
            last_peer_addr: std::sync::Mutex::new(None),
            last_http_version: std::sync::Mutex::new(None),
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
        let auth_factory = crate::engine::http::auth::factory_from_netrc(
            netrc_path.as_deref(),
            auth_options.no_netrc,
        );
        self.auth_options = Some(auth_options);
        self.auth_factory = std::sync::Arc::new(std::sync::Mutex::new(auth_factory));
        self
    }

    pub(crate) fn with_shared_auth_options(
        mut self,
        auth_options: AuthResolveOptions,
        auth_factory: std::sync::Arc<std::sync::Mutex<AuthConfigFactory>>,
    ) -> Self {
        self.auth_options = Some(auth_options);
        self.auth_factory = auth_factory;
        self
    }

    pub(crate) fn shared_auth_factory(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<AuthConfigFactory>> {
        std::sync::Arc::clone(&self.auth_factory)
    }

    pub(crate) fn with_origin_sensitive_headers_allowed(mut self, allowed: bool) -> Self {
        self.origin_sensitive_headers_allowed = allowed;
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
            let url = reqwest::Url::parse(url).ok()?;
            self.auth_factory
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .resolve_basic_authorization(&url, auth_options)
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

    pub fn last_http_version(&self) -> Option<reqwest::Version> {
        self.last_http_version
            .lock()
            .ok()
            .and_then(|version| *version)
    }

    pub(crate) fn remember_http_version(&self, version: reqwest::Version) {
        if let Ok(mut slot) = self.last_http_version.lock() {
            *slot = Some(version);
        }
    }

    pub fn clear_last_http_version(&self) {
        if let Ok(mut slot) = self.last_http_version.lock() {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod range_download_failure_tests {
    use super::RangeDownloadFailure;
    use crate::error::{Aria2Error, RecoverableError};

    #[test]
    fn ordinary_network_failures_do_not_signal_range_size_rejection() {
        let failure = RangeDownloadFailure::from(Aria2Error::Recoverable(
            RecoverableError::TemporaryNetworkFailure {
                message: "connection reset".into(),
            },
        ));

        assert!(!failure.explicit_size_rejection);
    }
}

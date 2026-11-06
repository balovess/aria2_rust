//! HTTP client for downloading individual BT pieces from a web-seed URL.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tracing::{debug, warn};

use super::stats::WebSeedStats;
use crate::http::client_identity::ClientTlsConfig;
use crate::request::request_group::AtomicProgress;

pub(super) const DEFAULT_WEB_SEED_TIMEOUT_SECS: u64 = 60;

#[derive(Debug)]
pub(super) enum WebSeedError {
    HttpStatus(u16),
    Failure(String),
}

impl fmt::Display for WebSeedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus(status) => {
                write!(formatter, "Unexpected HTTP status {status} from web-seed")
            }
            Self::Failure(message) => formatter.write_str(message),
        }
    }
}

/// HTTP client for downloading individual BT pieces from a single web-seed URL.
///
/// Uses HTTP Range requests (`Range: bytes={start}-{end}`) to fetch specific
/// byte ranges corresponding to torrent pieces.
pub struct WebSeedClient {
    /// Base URL of the web-seed (e.g., `<http://example.com/files/>`).
    base_url: String,
    /// Reusable reqwest HTTP client with connection pooling
    client: reqwest::Client,
    /// Pieces currently being requested (for concurrency control).
    /// Uses std::sync::Mutex because the lock is only held for short synchronous
    /// operations (insert/remove/check) and never across .await points.
    active_requests: Arc<std::sync::Mutex<HashSet<u32>>>,
    /// Statistics for this web seed
    stats: Arc<WebSeedStats>,
    /// Inactivity timeout for response headers and each response-body read.
    timeout: Duration,
}

impl WebSeedClient {
    /// Create a new WebSeedClient for the given base URL.
    ///
    /// # Arguments
    ///
    /// * `base_url` - The root URL for HTTP piece requests
    ///
    /// # Example
    ///
    /// ```
    /// use aria2_core::engine::bittorrent::download::web_seed::WebSeedClient;
    /// let client = WebSeedClient::new("http://cdn.example.com/torrent/");
    /// ```
    pub fn new(base_url: &str) -> Self {
        Self::new_with_tls(base_url, &ClientTlsConfig::default())
            .expect("web-seed HTTP client configuration must be valid")
    }

    pub(crate) fn new_with_tls(base_url: &str, tls: &ClientTlsConfig) -> Result<Self, String> {
        debug!(url = base_url, "Creating WebSeedClient");
        crate::http::client_pool::ensure_rustls_provider();

        // Build client with sensible defaults for large file downloads
        let timeout = Duration::from_secs(DEFAULT_WEB_SEED_TIMEOUT_SECS);
        let client = build_client(tls, None, timeout)?;

        Ok(Self {
            base_url: base_url.to_string(),
            client,
            active_requests: Arc::new(std::sync::Mutex::new(HashSet::new())),
            stats: Arc::new(WebSeedStats::new()),
            timeout,
        })
    }

    /// Create a WebSeedClient with shared stats (for aggregated statistics).
    pub fn with_shared_stats(base_url: &str, stats: Arc<WebSeedStats>) -> Self {
        Self::with_shared_stats_and_tls(base_url, stats, &ClientTlsConfig::default())
            .expect("web-seed HTTP client configuration must be valid")
    }

    pub(crate) fn with_shared_stats_and_tls(
        base_url: &str,
        stats: Arc<WebSeedStats>,
        tls: &ClientTlsConfig,
    ) -> Result<Self, String> {
        debug!(url = base_url, "Creating WebSeedClient with shared stats");
        crate::http::client_pool::ensure_rustls_provider();
        let timeout = Duration::from_secs(DEFAULT_WEB_SEED_TIMEOUT_SECS);
        let client = build_client(tls, None, timeout)?;

        Ok(Self {
            base_url: base_url.to_string(),
            client,
            active_requests: Arc::new(std::sync::Mutex::new(HashSet::new())),
            stats,
            timeout,
        })
    }

    pub(crate) fn with_shared_http_client(
        base_url: &str,
        stats: Arc<WebSeedStats>,
        client: reqwest::Client,
        timeout: Duration,
    ) -> Self {
        Self {
            base_url: base_url.to_string(),
            client,
            active_requests: Arc::new(std::sync::Mutex::new(HashSet::new())),
            stats,
            timeout,
        }
    }

    /// Check if a piece can be requested (not already active).
    pub fn can_request(&self, piece_index: u32) -> bool {
        let active = self
            .active_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        !active.contains(&piece_index)
    }

    /// Mark a piece as being requested.
    pub fn mark_requesting(&self, piece_index: u32) {
        let mut active = self
            .active_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        active.insert(piece_index);
    }

    /// Mark a piece as no longer being requested.
    pub fn clear_request(&self, piece_index: u32) {
        let mut active = self
            .active_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        active.remove(&piece_index);
    }

    /// Get the number of active requests.
    pub fn active_request_count(&self) -> usize {
        let active = self
            .active_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        active.len()
    }

    /// Get reference to the stats.
    pub fn stats(&self) -> &WebSeedStats {
        &self.stats
    }

    /// Download a specific piece range via HTTP GET with Range header.
    ///
    /// Constructs an HTTP request to fetch bytes `[piece_offset, piece_offset+length)`
    /// from the web-seed server using the `Range` header.
    ///
    /// # Arguments
    ///
    /// * `piece_index` - Logical index of the piece (for logging)
    /// * `piece_length` - Total length of this piece (unused in request but for context)
    /// * `piece_offset` - Byte offset within the full file where this piece starts
    /// * `length` - Number of bytes to download
    ///
    /// # Returns
    ///
    /// * `Ok(Vec<u8>)` - Raw piece data on success (HTTP 206 Partial Content or 200 OK)
    /// * `Err(String)` - Network error or non-success HTTP status
    pub async fn download_piece(
        &self,
        piece_index: u32,
        piece_length: u64,
        piece_offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, String> {
        self.download_piece_with_activity(piece_index, piece_length, piece_offset, length, None)
            .await
    }

    pub(crate) async fn download_piece_with_activity(
        &self,
        piece_index: u32,
        _piece_length: u64,
        piece_offset: u64,
        length: u64,
        network_activity: Option<&AtomicProgress>,
    ) -> Result<Vec<u8>, String> {
        self.download_piece_result_with_activity(
            piece_index,
            _piece_length,
            piece_offset,
            length,
            network_activity,
        )
        .await
        .map_err(|error| error.to_string())
    }

    pub(super) async fn download_piece_result_with_activity(
        &self,
        piece_index: u32,
        _piece_length: u64,
        piece_offset: u64,
        length: u64,
        network_activity: Option<&AtomicProgress>,
    ) -> Result<Vec<u8>, WebSeedError> {
        self.download_piece_result_with_response_status(
            piece_index,
            _piece_length,
            piece_offset,
            length,
            network_activity,
            |_| {},
        )
        .await
    }

    pub(super) async fn download_piece_result_with_response_status(
        &self,
        piece_index: u32,
        _piece_length: u64,
        piece_offset: u64,
        length: u64,
        network_activity: Option<&AtomicProgress>,
        on_response_status: impl FnOnce(u16),
    ) -> Result<Vec<u8>, WebSeedError> {
        let buffer_length = usize::try_from(length).map_err(|_| {
            WebSeedError::Failure(format!(
                "requested WebSeed range is too large: {length} bytes"
            ))
        })?;
        let mut data = vec![0; buffer_length];
        let received = self
            .download_piece_into_with_response_status(
                piece_index,
                piece_offset,
                &mut data,
                network_activity,
                on_response_status,
            )
            .await?;
        data.truncate(received);
        Ok(data)
    }

    pub(super) async fn download_piece_into_with_response_status(
        &self,
        piece_index: u32,
        piece_offset: u64,
        destination: &mut [u8],
        network_activity: Option<&AtomicProgress>,
        on_response_status: impl FnOnce(u16),
    ) -> Result<usize, WebSeedError> {
        let length = destination.len() as u64;
        if length == 0 {
            return Ok(0);
        }
        let range_end = piece_offset.saturating_add(length - 1);
        let range_header = format!("bytes={}-{}", piece_offset, range_end);

        debug!(
            piece_index,
            offset = piece_offset,
            length,
            url = self.base_url,
            range = %range_header,
            "Web-seed HTTP Range request"
        );

        let response = tokio::time::timeout(
            self.timeout,
            self.client
                .get(&self.base_url)
                .header("Range", &range_header)
                .header("User-Agent", crate::constants::USER_AGENT)
                .send(),
        )
        .await
        .map_err(|_| {
            WebSeedError::Failure(format!(
                "Web-seed response headers timed out after {} seconds",
                self.timeout.as_secs()
            ))
        })?
        .map_err(|e| WebSeedError::Failure(format!("HTTP request failed: {}", e)))?;

        let status = response.status().as_u16();
        on_response_status(status);

        // Accept 200 OK or 206 Partial Content
        if status != 200 && status != 206 {
            return Err(WebSeedError::HttpStatus(status));
        }

        let mut received = 0usize;
        let mut stream = response.bytes_stream();
        loop {
            let next_chunk = tokio::time::timeout(self.timeout, stream.next())
                .await
                .map_err(|_| {
                    WebSeedError::Failure(format!(
                        "Web-seed response body stalled for {} seconds",
                        self.timeout.as_secs()
                    ))
                })?;
            let Some(chunk) = next_chunk else {
                break;
            };
            let chunk = chunk.map_err(|e| {
                WebSeedError::Failure(format!("Failed to read response body: {}", e))
            })?;
            if received.saturating_add(chunk.len()) > destination.len() {
                return Err(WebSeedError::Failure(format!(
                    "Web-seed response exceeded requested range: expected at most {length} bytes"
                )));
            }
            if !chunk.is_empty()
                && let Some(progress) = network_activity
            {
                progress.record_download_payload(chunk.len() as u64);
            }
            let next = received + chunk.len();
            destination[received..next].copy_from_slice(&chunk);
            received = next;
        }

        // Record statistics
        self.stats.record_bytes(received as u64);

        if received as u64 != length {
            warn!(
                expected = length,
                actual = received,
                piece_index,
                "Web-seed response size mismatch"
            );
        }

        Ok(received)
    }

    /// Request a piece from this web seed with concurrency control.
    ///
    /// This method:
    /// 1. Checks if the piece is already being requested
    /// 2. Marks the piece as active
    /// 3. Downloads the piece
    /// 4. Clears the active flag
    ///
    /// # Arguments
    ///
    /// * `piece_index` - Index of the piece to download
    /// * `piece_length` - Length of each piece
    /// * `total_length` - Total file length (for calculating the last piece size)
    ///
    /// # Returns
    ///
    /// * `Ok(Vec<u8>)` - Piece data
    /// * `Err(String)` - Error or "already active"
    pub async fn request_piece(
        &self,
        piece_index: u32,
        piece_length: u32,
        total_length: u64,
    ) -> Result<Vec<u8>, String> {
        self.request_piece_with_activity(piece_index, piece_length, total_length, None)
            .await
    }

    pub(crate) async fn request_piece_with_activity(
        &self,
        piece_index: u32,
        piece_length: u32,
        total_length: u64,
        network_activity: Option<&AtomicProgress>,
    ) -> Result<Vec<u8>, String> {
        // Check if already requesting
        if !self.can_request(piece_index) {
            return Err(format!("Piece {} already being requested", piece_index));
        }

        // Mark as active
        self.mark_requesting(piece_index);

        // Calculate offset and length
        let piece_offset = piece_index as u64 * piece_length as u64;
        let remaining = total_length.saturating_sub(piece_offset);
        let actual_length = std::cmp::min(piece_length as u64, remaining);

        // Download
        let result = self
            .download_piece_with_activity(
                piece_index,
                piece_length as u64,
                piece_offset,
                actual_length,
                network_activity,
            )
            .await;

        // Clear active flag
        self.clear_request(piece_index);

        result
    }

    /// Check whether this web-seed appears to be available.
    ///
    /// Currently returns `true` unconditionally; a future implementation
    /// could perform a lightweight HEAD request or health check.
    pub fn is_available(&self) -> bool {
        true
    }

    /// Get the base URL of this web-seed (for display/logging).
    pub fn url(&self) -> &str {
        &self.base_url
    }
}

pub(crate) fn build_client(
    tls: &ClientTlsConfig,
    local_address: Option<std::net::IpAddr>,
    connect_timeout: Duration,
) -> Result<reqwest::Client, String> {
    crate::http::client_pool::ensure_rustls_provider();
    let mut builder = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .pool_max_idle_per_host(4)
        .gzip(false);
    if let Some(address) = local_address {
        builder = builder.local_address(address);
    }
    let builder =
        crate::http::client_identity::apply(builder, tls).map_err(|error| error.to_string())?;
    crate::http::client_pool::configure_http2_download_client(builder)
        .build()
        .map_err(|error| format!("web-seed HTTP client build failed: {error}"))
}

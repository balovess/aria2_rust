//! HTTP client for downloading individual BT pieces from a web-seed URL.

use std::collections::HashSet;
use std::sync::Arc;

use tracing::{debug, warn};

use super::stats::WebSeedStats;

/// HTTP client for downloading individual BT pieces from a single web-seed URL.
///
/// Uses HTTP Range requests (`Range: bytes={start}-{end}`) to fetch specific
/// byte ranges corresponding to torrent pieces.
pub struct WebSeedClient {
    /// Base URL of the web-seed (e.g., "http://example.com/files/")
    base_url: String,
    /// Reusable reqwest HTTP client with connection pooling
    client: reqwest::Client,
    /// Pieces currently being requested (for concurrency control).
    /// Uses std::sync::Mutex because the lock is only held for short synchronous
    /// operations (insert/remove/check) and never across .await points.
    active_requests: Arc<std::sync::Mutex<HashSet<u32>>>,
    /// Statistics for this web seed
    stats: Arc<WebSeedStats>,
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
    /// use aria2_core::engine::bt_web_seed::WebSeedClient;
    /// let client = WebSeedClient::new("http://cdn.example.com/torrent/");
    /// ```
    pub fn new(base_url: &str) -> Self {
        debug!(url = base_url, "Creating WebSeedClient");
        crate::http::client_pool::ensure_rustls_provider();

        // Build client with sensible defaults for large file downloads
        let client = build_client();

        Self {
            base_url: base_url.to_string(),
            client,
            active_requests: Arc::new(std::sync::Mutex::new(HashSet::new())),
            stats: Arc::new(WebSeedStats::new()),
        }
    }

    /// Create a WebSeedClient with shared stats (for aggregated statistics).
    pub fn with_shared_stats(base_url: &str, stats: Arc<WebSeedStats>) -> Self {
        debug!(url = base_url, "Creating WebSeedClient with shared stats");
        crate::http::client_pool::ensure_rustls_provider();

        let client = build_client();

        Self {
            base_url: base_url.to_string(),
            client,
            active_requests: Arc::new(std::sync::Mutex::new(HashSet::new())),
            stats,
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
        _piece_length: u64,
        piece_offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, String> {
        let range_end = piece_offset + length.saturating_sub(1);
        let range_header = format!("bytes={}-{}", piece_offset, range_end);

        debug!(
            piece_index,
            offset = piece_offset,
            length,
            url = self.base_url,
            range = %range_header,
            "Web-seed HTTP Range request"
        );

        let response = self
            .client
            .get(&self.base_url)
            .header("Range", &range_header)
            .header("User-Agent", crate::constants::USER_AGENT)
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        let status = response.status().as_u16();

        // Accept 200 OK or 206 Partial Content
        if status != 200 && status != 206 {
            return Err(format!("Unexpected HTTP status {} from web-seed", status));
        }

        let data = response
            .bytes()
            .await
            .map_err(|e| format!("Failed to read response body: {}", e))?
            .to_vec();

        // Record statistics
        self.stats.record_bytes(data.len() as u64);

        if data.len() != length as usize {
            warn!(
                expected = length,
                actual = data.len(),
                piece_index,
                "Web-seed response size mismatch"
            );
        }

        Ok(data)
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
            .download_piece(
                piece_index,
                piece_length as u64,
                piece_offset,
                actual_length,
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

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_max_idle_per_host(4)
        .gzip(false)
        .build()
        .expect("web-seed HTTP client configuration must be valid")
}

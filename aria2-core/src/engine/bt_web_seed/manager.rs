//! Multi-seed endpoint manager with automatic fallback.

use std::sync::Arc;

use tracing::{debug, warn};

use super::client::WebSeedClient;
use super::stats::WebSeedStats;

/// Manages multiple web-seed endpoints with automatic fallback.
///
/// When downloading a piece, tries each configured web-seed URL in order
/// until one succeeds. If all fail, returns an aggregated error.
pub struct WebSeedManager {
    /// Ordered list of web-seed clients
    clients: Vec<WebSeedClient>,
    /// Shared statistics across all web seeds
    stats: Arc<WebSeedStats>,
    /// Piece length for calculating offsets
    piece_length: u32,
    /// Total file length
    total_length: u64,
}

impl WebSeedManager {
    /// Create a new WebSeedManager from a list of web-seed URLs.
    ///
    /// # Arguments
    ///
    /// * `urls` - List of HTTP(S) URLs serving the torrent content
    /// * `piece_length` - Length of each piece in the torrent
    /// * `total_length` - Total file length
    ///
    /// # Example
    ///
    /// ```
    /// use aria2_core::engine::bt_web_seed::WebSeedManager;
    /// let manager = WebSeedManager::new(
    ///     vec![
    ///         "http://mirror1.example.com/file.bin".to_string(),
    ///         "http://mirror2.example.com/file.bin".to_string(),
    ///     ],
    ///     16384,  // piece_length
    ///     1048576 // total_length
    /// );
    /// ```
    pub fn new(urls: Vec<String>, piece_length: u32, total_length: u64) -> Self {
        debug!(
            count = urls.len(),
            "Creating WebSeedManager with {} seed(s)",
            urls.len()
        );

        let stats = Arc::new(WebSeedStats::new());

        let clients = urls
            .into_iter()
            .map(|url| WebSeedClient::with_shared_stats(&url, stats.clone()))
            .collect();

        Self {
            clients,
            stats,
            piece_length,
            total_length,
        }
    }

    /// Get the shared statistics.
    pub fn stats(&self) -> &WebSeedStats {
        &self.stats
    }

    /// Request a piece from any available web seed.
    ///
    /// This method uses the new `request_piece` API with concurrency control.
    ///
    /// # Arguments
    ///
    /// * `piece_index` - Index of the piece to download
    ///
    /// # Returns
    ///
    /// * `Ok(Vec<u8>)` - Piece data from first successful web-seed
    /// * `Err(String)` - All web-seeds failed
    pub async fn request_piece(&self, piece_index: u32) -> Result<Vec<u8>, String> {
        if self.clients.is_empty() {
            return Err("No web-seeds configured".to_string());
        }

        let mut last_error = String::new();

        for (i, client) in self.clients.iter().enumerate() {
            if !client.is_available() || !client.can_request(piece_index) {
                debug!(
                    index = i,
                    url = client.url(),
                    "Skipping unavailable or busy web-seed"
                );
                continue;
            }

            match client
                .request_piece(piece_index, self.piece_length, self.total_length)
                .await
            {
                Ok(data) => {
                    debug!(
                        piece_index,
                        seed_index = i,
                        url = client.url(),
                        size = data.len(),
                        "Piece downloaded from web-seed"
                    );
                    return Ok(data);
                }
                Err(e) => {
                    warn!(
                        piece_index,
                        seed_index = i,
                        url = client.url(),
                        error = %e,
                        "Web-seed download failed, trying next"
                    );
                    last_error = format!("seed[{}]={}: {}", i, client.url(), e);
                }
            }
        }

        Err(format!("All web-seeds failed: {}", last_error))
    }

    /// Attempt to download a piece from any available web-seed.
    ///
    /// Tries each web-seed in order; returns data from the first successful
    /// response. Collects errors from all failed attempts if all fail.
    ///
    /// # Arguments
    ///
    /// * `piece_index` - Logical index of the piece
    /// * `piece_length` - Total length of this piece
    /// * `piece_offset` - Byte offset within the file
    /// * `length` - Number of bytes to download
    ///
    /// # Returns
    ///
    /// * `Ok(Vec<u8>)` - Piece data from first successful web-seed
    /// * `Err(String)` - All web-seeds failed (contains error details)
    pub async fn try_download_piece(
        &self,
        piece_index: u32,
        piece_length: u64,
        piece_offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, String> {
        if self.clients.is_empty() {
            return Err("No web-seeds configured".to_string());
        }

        let mut last_error = String::new();

        for (i, client) in self.clients.iter().enumerate() {
            if !client.is_available() {
                debug!(
                    index = i,
                    url = client.url(),
                    "Skipping unavailable web-seed"
                );
                continue;
            }

            match client
                .download_piece(piece_index, piece_length, piece_offset, length)
                .await
            {
                Ok(data) => {
                    debug!(
                        piece_index,
                        seed_index = i,
                        url = client.url(),
                        size = data.len(),
                        "Piece downloaded from web-seed"
                    );
                    return Ok(data);
                }
                Err(e) => {
                    warn!(
                        piece_index,
                        seed_index = i,
                        url = client.url(),
                        error = %e,
                        "Web-seed download failed, trying next"
                    );
                    last_error = format!("seed[{}]={}: {}", i, client.url(), e);
                }
            }
        }

        Err(format!("All web-seeds failed: {}", last_error))
    }

    /// Get the number of configured web-seed URLs.
    pub fn len(&self) -> usize {
        self.clients.len()
    }

    /// Check if any web-seeds are configured.
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Get reference to the underlying web-seed clients.
    pub fn clients(&self) -> &[WebSeedClient] {
        &self.clients
    }
}

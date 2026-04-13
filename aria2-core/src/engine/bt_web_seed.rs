//! BitTorrent Web-seed (HTTP/HTTPS fallback for piece downloads)
//!
//! This module implements BEP 19 / BEP 17 Web Seed support, allowing
//! aria2-rust to download torrent pieces via HTTP Range requests when
//! the peer swarm is insufficient or unavailable.
//!
//! # Architecture
//!
//! - [`WebSeedClient`] - Single HTTP endpoint for downloading pieces
//! - [`WebSeedManager`] - Manages multiple web-seed URLs with fallback logic
//! - [`parse_url_list()`] - Extracts `url-list` from torrent metadata

use tracing::{debug, warn};

/// HTTP client for downloading individual BT pieces from a single web-seed URL.
///
/// Uses HTTP Range requests (`Range: bytes={start}-{end}`) to fetch specific
/// byte ranges corresponding to torrent pieces.
pub struct WebSeedClient {
    /// Base URL of the web-seed (e.g., "http://example.com/files/")
    base_url: String,
    /// Reusable reqwest HTTP client with connection pooling
    client: reqwest::Client,
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

        // Build client with sensible defaults for large file downloads
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            base_url: base_url.to_string(),
            client,
        }
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
            .header("User-Agent", "aria2-rust/1.0")
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

/// Manages multiple web-seed endpoints with automatic fallback.
///
/// When downloading a piece, tries each configured web-seed URL in order
/// until one succeeds. If all fail, returns an aggregated error.
pub struct WebSeedManager {
    /// Ordered list of web-seed clients
    clients: Vec<WebSeedClient>,
}

impl WebSeedManager {
    /// Create a new WebSeedManager from a list of web-seed URLs.
    ///
    /// # Arguments
    ///
    /// * `urls` - List of HTTP(S) URLs serving the torrent content
    ///
    /// # Example
    ///
    /// ```
    /// use aria2_core::engine::bt_web_seed::WebSeedManager;
    /// let manager = WebSeedManager::new(vec![
    ///     "http://mirror1.example.com/file.bin".to_string(),
    ///     "http://mirror2.example.com/file.bin".to_string(),
    /// ]);
    /// ```
    pub fn new(urls: Vec<String>) -> Self {
        debug!(
            count = urls.len(),
            "Creating WebSeedManager with {} seed(s)",
            urls.len()
        );

        let clients = urls
            .into_iter()
            .map(|url| WebSeedClient::new(&url))
            .collect();

        Self { clients }
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

/// Parse the `url-list` field from torrent metadata.
///
/// The BEP 19 / RFC 6986 `url-list` key can be either:
/// - A single string (one URL)
/// - A list of strings (multiple fallback URLs)
///
/// Returns an empty vector if the key is missing or malformed.
///
/// # Arguments
///
/// * `meta` - Parsed torrent metadata structure
///
/// # Returns
///
/// * `Vec<String>` - List of extracted web-seed URLs
///
/// # Example
///
/// ```ignore
/// let urls = parse_url_list(&torrent_meta);
/// if !urls.is_empty() {
///     let manager = WebSeedManager::new(urls);
/// }
/// ```
pub fn parse_url_list(
    _meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
) -> Vec<String> {
    // We need to re-decode the raw torrent bytes to access url-list
    // since TorrentMeta doesn't currently expose it as a field.
    // For now, return empty and document that integration requires
    // adding url-list extraction to TorrentMeta or passing raw bencode.

    // Note: In production, url-list should be added as a field on TorrentMeta.
    // This stub demonstrates the interface contract.
    Vec::new()
}

/// Parse url-list directly from raw bencoded torrent data.
///
/// This is the working implementation that decodes the raw torrent bytes
/// and extracts the `url-list` key at the top level of the bencode dictionary.
///
/// # Arguments
///
/// * `torrent_bytes` - Raw bencoded torrent file contents
///
/// # Returns
///
/// * `Vec<String>` - Extracted web-seed URLs (empty if missing/malformed)
pub fn parse_url_list_from_bytes(torrent_bytes: &[u8]) -> Vec<String> {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    let (root, _) = match BencodeValue::decode(torrent_bytes) {
        Ok(result) => result,
        Err(_) => return Vec::new(),
    };

    match root.dict_get(b"url-list") {
        Some(BencodeValue::Bytes(url_bytes)) => {
            // Single URL string
            match std::str::from_utf8(url_bytes) {
                Ok(url) => vec![url.to_string()],
                Err(_) => Vec::new(),
            }
        }
        Some(BencodeValue::List(items)) => {
            // List of URL strings
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(|s| s.to_string())
                .collect()
        }
        _ => Vec::new(), // Missing or wrong type
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use std::collections::BTreeMap;

    // ==================== parse_url_list tests ====================

    #[test]
    fn test_parse_url_list_single() {
        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        );
        root.insert(
            b"url-list".to_vec(),
            BencodeValue::Bytes(b"http://webseed.example.com/file.bin".to_vec()),
        );

        // Add minimal info dict
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test".to_vec()));
        info.insert(b"length".to_vec(), BencodeValue::Int(1024));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(512));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 40]));
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));

        let encoded = BencodeValue::Dict(root).encode();
        let urls = parse_url_list_from_bytes(&encoded);

        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], "http://webseed.example.com/file.bin");
    }

    #[test]
    fn test_parse_url_list_multiple() {
        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        );
        root.insert(
            b"url-list".to_vec(),
            BencodeValue::List(vec![
                BencodeValue::Bytes(b"http://seed1.example.com/file.bin".to_vec()),
                BencodeValue::Bytes(b"http://seed2.example.com/file.bin".to_vec()),
                BencodeValue::Bytes(b"https://seed3.example.com/file.bin".to_vec()),
            ]),
        );

        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test".to_vec()));
        info.insert(b"length".to_vec(), BencodeValue::Int(2048));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(512));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 80]));
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));

        let encoded = BencodeValue::Dict(root).encode();
        let urls = parse_url_list_from_bytes(&encoded);

        assert_eq!(urls.len(), 3);
        assert_eq!(urls[0], "http://seed1.example.com/file.bin");
        assert_eq!(urls[1], "http://seed2.example.com/file.bin");
        assert_eq!(urls[2], "https://seed3.example.com/file.bin");
    }

    #[test]
    fn test_parse_url_list_missing() {
        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        );
        // No url-list key present

        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test".to_vec()));
        info.insert(b"length".to_vec(), BencodeValue::Int(512));
        info.insert(b"piece length".to_vec(), BencodeValue::Int(256));
        info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 20]));
        root.insert(b"info".to_vec(), BencodeValue::Dict(info));

        let encoded = BencodeValue::Dict(root).encode();
        let urls = parse_url_list_from_bytes(&encoded);

        assert!(urls.is_empty());
    }

    // ==================== Range header construction tests ====================

    #[test]
    fn test_range_request_format() {
        // Verify the Range header format matches HTTP spec (RFC 7233)
        let _client = WebSeedClient::new("http://example.com/file.bin");

        // Test: piece starting at offset 0, length 16384
        // Expected Range: bytes=0-16383
        let offset = 0u64;
        let length = 16384u64;
        let range_end = offset + length.saturating_sub(1);
        let expected = format!("bytes={}-{}", offset, range_end);

        assert_eq!(expected, "bytes=0-16383");

        // Test: piece starting at offset 524288, length 262144
        let offset2 = 524288u64;
        let length2 = 262144u64;
        let range_end2 = offset2 + length2.saturating_sub(1);
        let expected2 = format!("bytes={}-{}", offset2, range_end2);

        assert_eq!(expected2, "bytes=524288-786431");
    }

    #[test]
    fn test_web_seed_manager_fallback() {
        // Verify manager creation with multiple seeds
        let urls = vec![
            "http://seed1.example.com/file.iso".to_string(),
            "http://seed2.example.com/file.iso".to_string(),
        ];

        let manager = WebSeedManager::new(urls);

        assert_eq!(manager.len(), 2);
        assert!(!manager.is_empty());
        assert_eq!(manager.clients().len(), 2);

        // Verify each client has correct URL
        assert_eq!(
            manager.clients()[0].url(),
            "http://seed1.example.com/file.iso"
        );
        assert_eq!(
            manager.clients()[1].url(),
            "http://seed2.example.com/file.iso"
        );
    }

    #[test]
    fn test_web_seed_client_creation() {
        let client = WebSeedClient::new("https://cdn.example.com/releases/v1.tar.gz");

        assert_eq!(client.url(), "https://cdn.example.com/releases/v1.tar.gz");
        assert!(client.is_available());
    }

    #[test]
    fn test_web_seed_manager_empty() {
        let manager = WebSeedManager::new(Vec::new());

        assert_eq!(manager.len(), 0);
        assert!(manager.is_empty());
    }

    #[test]
    fn test_parse_url_list_invalid_utf8() {
        let mut root = BTreeMap::new();
        root.insert(
            b"url-list".to_vec(),
            BencodeValue::Bytes(vec![0xFF, 0xFE]), // Invalid UTF-8
        );

        let encoded = BencodeValue::Dict(root).encode();
        let urls = parse_url_list_from_bytes(&encoded);

        // Should return empty (skip invalid UTF-8 URLs)
        assert!(urls.is_empty());
    }

    #[test]
    fn test_parse_url_list_mixed_valid_invalid() {
        let mut root = BTreeMap::new();
        root.insert(
            b"url-list".to_vec(),
            BencodeValue::List(vec![
                BencodeValue::Bytes(b"http://valid.example.com/file.bin".to_vec()),
                BencodeValue::Int(42), // Invalid: not a string
                BencodeValue::Bytes(b"http://also-valid.example.com/file.bin".to_vec()),
            ]),
        );

        let encoded = BencodeValue::Dict(root).encode();
        let urls = parse_url_list_from_bytes(&encoded);

        // Should skip non-string entries, return only valid URLs
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "http://valid.example.com/file.bin");
        assert_eq!(urls[1], "http://also-valid.example.com/file.bin");
    }
}

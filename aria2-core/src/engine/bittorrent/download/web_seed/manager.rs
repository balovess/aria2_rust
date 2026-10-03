//! Multi-seed endpoint manager with automatic fallback.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::{debug, warn};

use super::client::WebSeedClient;
use super::stats::WebSeedStats;
use crate::http::client_identity::ClientTlsConfig;
use crate::network::OutboundNetworkPolicy;
use crate::request::request_group::{AtomicProgress, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

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
    /// BT tasks read file URI queues at request time so `changeUri` updates
    /// become visible without rebuilding the piece session.
    live_group: Option<Arc<std::sync::RwLock<RequestGroup>>>,
    tls: ClientTlsConfig,
    network_policy: Option<Arc<OutboundNetworkPolicy>>,
    /// HTTP pools are shared by origin, not by file URL, so multi-file
    /// torrents do not allocate one connection pool per file.
    http_clients: tokio::sync::Mutex<HashMap<String, reqwest::Client>>,
    unavailable: std::sync::Mutex<UnavailableWebSeeds>,
}

#[derive(Default)]
struct UnavailableWebSeeds {
    uri_generation: Option<u64>,
    uris: HashSet<String>,
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
    /// use aria2_core::engine::bittorrent::download::web_seed::WebSeedManager;
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
        Self::new_with_tls(
            urls,
            piece_length,
            total_length,
            &ClientTlsConfig::default(),
        )
        .expect("web-seed HTTP client configuration must be valid")
    }

    pub(crate) fn new_with_tls(
        urls: Vec<String>,
        piece_length: u32,
        total_length: u64,
        tls: &ClientTlsConfig,
    ) -> Result<Self, String> {
        debug!(
            count = urls.len(),
            "Creating WebSeedManager with {} seed(s)",
            urls.len()
        );

        let stats = Arc::new(WebSeedStats::new());

        let clients = urls
            .into_iter()
            .map(|url| WebSeedClient::with_shared_stats_and_tls(&url, Arc::clone(&stats), tls))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            clients,
            stats,
            piece_length,
            total_length,
            live_group: None,
            tls: tls.clone(),
            network_policy: None,
            http_clients: tokio::sync::Mutex::new(HashMap::new()),
            unavailable: std::sync::Mutex::new(UnavailableWebSeeds::default()),
        })
    }

    /// Build a live per-file WebSeed view for a BT task. URI queues remain
    /// owned by `RequestGroup`; this manager snapshots only the files touched
    /// by each piece and reuses HTTP clients by origin.
    pub(crate) fn for_request_group(
        group: Arc<std::sync::RwLock<RequestGroup>>,
        piece_length: u32,
        total_length: u64,
        tls: ClientTlsConfig,
        policy: Arc<OutboundNetworkPolicy>,
    ) -> Self {
        Self {
            clients: Vec::new(),
            stats: Arc::new(WebSeedStats::new()),
            piece_length,
            total_length,
            live_group: Some(group),
            tls,
            network_policy: Some(policy),
            http_clients: tokio::sync::Mutex::new(HashMap::new()),
            unavailable: std::sync::Mutex::new(UnavailableWebSeeds::default()),
        }
    }

    /// Whether all file-backed bytes in a piece currently have a WebSeed.
    ///
    /// A piece is eligible for exclusive background scheduling only when
    /// every non-padding range can be supplied. Partially covered pieces stay
    /// with the peer path, which may still use the existing fallback.
    pub(crate) fn has_complete_sources_for_piece(
        &self,
        piece_index: u32,
        piece_data_length: u32,
    ) -> bool {
        let Some(group) = &self.live_group else {
            return !self.clients.is_empty();
        };
        let piece_start = piece_index as u64 * self.piece_length as u64;
        let piece_end = piece_start.saturating_add(piece_data_length as u64);
        let group = group.recover();
        let Some(context) = group.get_download_context() else {
            return false;
        };
        let entries = context.get_file_entries();
        let first = entries.partition_point(|entry| entry.last_offset() <= piece_start);
        let mut has_range = false;
        for entry in entries[first..]
            .iter()
            .take_while(|entry| entry.offset() < piece_end)
        {
            if entry.offset().max(piece_start) >= entry.last_offset().min(piece_end) {
                continue;
            }
            has_range = true;
            if !entry.has_uri_sources() {
                return false;
            }
        }
        has_range
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
        self.request_piece_with_activity(piece_index, None).await
    }

    pub(crate) async fn request_piece_with_activity(
        &self,
        piece_index: u32,
        network_activity: Option<&AtomicProgress>,
    ) -> Result<Vec<u8>, String> {
        let piece_offset = piece_index as u64 * self.piece_length as u64;
        let length = self
            .total_length
            .saturating_sub(piece_offset)
            .min(self.piece_length as u64);
        self.request_piece_with_length_and_activity(piece_index, length, network_activity)
            .await
    }

    pub(crate) async fn request_piece_with_length_and_activity(
        &self,
        piece_index: u32,
        piece_data_length: u64,
        network_activity: Option<&AtomicProgress>,
    ) -> Result<Vec<u8>, String> {
        if let Some(group) = &self.live_group {
            return self
                .request_live_piece(group, piece_index, piece_data_length, network_activity)
                .await;
        }
        if self.clients.is_empty() {
            return Err("No web-seeds configured".to_string());
        }

        let mut last_error = String::new();

        for (i, client) in self.clients.iter().enumerate() {
            if !client.is_available()
                || !client.can_request(piece_index)
                || self.is_uri_unavailable(client.url(), None)
            {
                debug!(
                    index = i,
                    url = client.url(),
                    "Skipping unavailable or busy web-seed"
                );
                continue;
            }

            match client
                .download_piece_result_with_activity(
                    piece_index,
                    self.piece_length as u64,
                    piece_index as u64 * self.piece_length as u64,
                    piece_data_length,
                    network_activity,
                )
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
                    if e.is_not_found() {
                        self.mark_uri_unavailable(client.url(), None);
                    }
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

    async fn request_live_piece(
        &self,
        group: &Arc<std::sync::RwLock<RequestGroup>>,
        piece_index: u32,
        piece_data_length: u64,
        network_activity: Option<&AtomicProgress>,
    ) -> Result<Vec<u8>, String> {
        if piece_data_length == 0 {
            return Err("Cannot request an empty BitTorrent piece".to_string());
        }
        let piece_start = piece_index as u64 * self.piece_length as u64;
        let piece_end = piece_start.saturating_add(piece_data_length);
        let (uri_generation, file_ranges) = {
            let group = group.recover();
            let uri_generation = group.uri_generation();
            let context = group
                .get_download_context()
                .ok_or_else(|| "BitTorrent download context is unavailable".to_string())?;
            let entries = context.get_file_entries();
            let first = entries.partition_point(|entry| entry.last_offset() <= piece_start);
            let file_ranges = entries[first..]
                .iter()
                .take_while(|entry| entry.offset() < piece_end)
                .filter_map(|entry| {
                    let start = piece_start.max(entry.offset());
                    let end = piece_end.min(entry.last_offset());
                    (start < end).then(|| (start, end, entry.offset(), entry.uris()))
                })
                .collect::<Vec<_>>();
            (uri_generation, file_ranges)
        };
        self.observe_uri_generation(uri_generation);

        if file_ranges.is_empty() {
            return Err(format!("Piece {piece_index} has no file-backed byte range"));
        }

        let piece_length = usize::try_from(piece_data_length)
            .map_err(|_| "WebSeed piece exceeds addressable memory".to_string())?;
        let mut piece = vec![0; piece_length];
        let mut last_error = None;
        for (start, end, file_offset, uris) in file_ranges {
            if uris.is_empty() {
                return Err(format!(
                    "No WebSeed URI is configured for piece {piece_index} file range {start}..{end}"
                ));
            }
            let length = end - start;
            let output_start = usize::try_from(start - piece_start)
                .map_err(|_| "WebSeed piece offset exceeds addressable memory".to_string())?;
            let output_length = usize::try_from(length)
                .map_err(|_| "WebSeed file range exceeds addressable memory".to_string())?;
            let output_end = output_start
                .checked_add(output_length)
                .filter(|&end| end <= piece.len())
                .ok_or_else(|| "WebSeed file range exceeds piece buffer".to_string())?;
            let mut range_received = false;
            for uri in &uris {
                if self.is_uri_unavailable(uri, Some(uri_generation)) {
                    continue;
                }
                let client = match self.live_client(uri).await {
                    Ok(client) => client,
                    Err(error) => {
                        last_error = Some(format!("{uri}: {error}"));
                        continue;
                    }
                };
                match client
                    .download_piece_into(
                        piece_index,
                        start - file_offset,
                        &mut piece[output_start..output_end],
                        network_activity,
                    )
                    .await
                {
                    Ok(received) if received as u64 == length => {
                        range_received = true;
                        break;
                    }
                    Ok(received) => {
                        last_error = Some(format!(
                            "{uri}: expected {length} bytes, received {}",
                            received
                        ));
                    }
                    Err(error) => {
                        if error.is_not_found() {
                            self.mark_uri_unavailable(uri, Some(uri_generation));
                        }
                        last_error = Some(format!("{uri}: {error}"));
                    }
                }
            }

            if !range_received {
                return Err(format!(
                    "All WebSeeds failed for piece {piece_index} range {start}..{end}: {}",
                    last_error.unwrap_or_else(|| "no usable endpoint".to_string())
                ));
            }
        }
        Ok(piece)
    }

    async fn live_client(&self, uri: &str) -> Result<WebSeedClient, String> {
        let parsed = reqwest::Url::parse(uri).map_err(|error| format!("invalid URI: {error}"))?;
        let scheme = parsed.scheme();
        if !matches!(scheme, "http" | "https") {
            return Err(format!("unsupported WebSeed scheme: {scheme}"));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| "WebSeed URI has no host".to_string())?;
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| "WebSeed URI has no port".to_string())?;
        let policy = self
            .network_policy
            .as_ref()
            .ok_or_else(|| "WebSeed network policy is unavailable".to_string())?;
        let local_address = if policy.is_direct() {
            None
        } else {
            policy
                .source_for_host(host, port)
                .await
                .map_err(|error| format!("WebSeed source selection failed: {error}"))?
        };
        let origin = format!("{scheme}://{host}:{port}/{local_address:?}");
        let client = {
            let mut clients = self.http_clients.lock().await;
            if let Some(client) = clients.get(&origin) {
                client.clone()
            } else {
                let client = super::client::build_client(&self.tls, local_address)?;
                clients.insert(origin, client.clone());
                client
            }
        };
        Ok(WebSeedClient::with_shared_http_client(
            uri,
            Arc::clone(&self.stats),
            client,
        ))
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
            if !client.is_available() || self.is_uri_unavailable(client.url(), None) {
                debug!(
                    index = i,
                    url = client.url(),
                    "Skipping unavailable web-seed"
                );
                continue;
            }

            match client
                .download_piece_result_with_activity(
                    piece_index,
                    piece_length,
                    piece_offset,
                    length,
                    None,
                )
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
                    if e.is_not_found() {
                        self.mark_uri_unavailable(client.url(), None);
                    }
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
        if let Some(group) = &self.live_group {
            return group
                .recover()
                .get_download_context()
                .is_none_or(|context| {
                    context
                        .get_file_entries()
                        .iter()
                        .all(|entry| !entry.has_uri_sources())
                });
        }
        self.clients.is_empty()
    }

    /// Get reference to the underlying web-seed clients.
    pub fn clients(&self) -> &[WebSeedClient] {
        &self.clients
    }

    fn observe_uri_generation(&self, generation: u64) {
        let mut unavailable = self
            .unavailable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if unavailable
            .uri_generation
            .is_none_or(|observed| generation > observed)
        {
            unavailable.uris.clear();
            unavailable.uri_generation = Some(generation);
        }
    }

    fn is_uri_unavailable(&self, uri: &str, generation: Option<u64>) -> bool {
        let unavailable = self
            .unavailable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unavailable.uri_generation == generation && unavailable.uris.contains(uri)
    }

    fn mark_uri_unavailable(&self, uri: &str, generation: Option<u64>) {
        let mut unavailable = self
            .unavailable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if unavailable.uri_generation == generation {
            unavailable.uris.insert(uri.to_string());
            warn!(
                url = uri,
                "Web-seed returned HTTP 404; skipping it until URI configuration changes"
            );
        }
    }
}

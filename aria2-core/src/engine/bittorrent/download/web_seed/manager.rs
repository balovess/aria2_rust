//! Multi-seed endpoint manager with automatic fallback.

mod live_sources;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, warn};

use super::client::{WebSeedClient, WebSeedError};
use super::stats::WebSeedStats;
use super::uri_state::{UriProbeAdmission, WebSeedUriState};
use crate::engine::http::auth;
use crate::http::client_identity::ClientTlsConfig;
use crate::network::OutboundNetworkPolicy;
use crate::request::request_group::{AtomicProgress, HttpProxyOptions, RequestGroup};
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
    proxy_options: HttpProxyOptions,
    user_agent: String,
    request_headers: Vec<(String, String)>,
    auth_factory: Arc<std::sync::Mutex<crate::http::AuthConfigFactory>>,
    auth_options: crate::http::AuthResolveOptions,
    request_timeout: Duration,
    connect_timeout: Duration,
    network_policy: Option<Arc<OutboundNetworkPolicy>>,
    dns_resolver: Option<crate::dns::DnsResolver>,
    /// HTTP pools are shared by origin, not by file URL, so multi-file
    /// torrents do not allocate one connection pool per file.
    http_clients: tokio::sync::Mutex<HashMap<String, reqwest::Client>>,
    uri_state: WebSeedUriState,
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
        let request_timeout = Duration::from_secs(super::client::DEFAULT_WEB_SEED_TIMEOUT_SECS);
        let connect_timeout = request_timeout;

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
            proxy_options: HttpProxyOptions::default(),
            user_agent: crate::constants::USER_AGENT.to_string(),
            request_headers: Vec::new(),
            auth_factory: Arc::new(std::sync::Mutex::new(crate::http::AuthConfigFactory::new())),
            auth_options: crate::http::AuthResolveOptions::default(),
            request_timeout,
            connect_timeout,
            network_policy: None,
            dns_resolver: None,
            http_clients: tokio::sync::Mutex::new(HashMap::new()),
            uri_state: WebSeedUriState::default(),
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
        let (
            request_timeout,
            connect_timeout,
            user_agent,
            request_headers,
            proxy_options,
            auth_options,
            auth_factory,
        ) = {
            let group = group.recover();
            let options = group.options();
            (
                Duration::from_secs(
                    options
                        .timeout
                        .unwrap_or(super::client::DEFAULT_WEB_SEED_TIMEOUT_SECS),
                ),
                Duration::from_secs(
                    options
                        .connect_timeout
                        .unwrap_or(super::client::DEFAULT_WEB_SEED_TIMEOUT_SECS),
                ),
                options
                    .http_user_agent()
                    .unwrap_or(crate::constants::USER_AGENT)
                    .to_string(),
                options.parsed_headers(),
                options.http_proxy_options(),
                auth::auth_options_from_download_options(options, "http"),
                Arc::new(std::sync::Mutex::new(auth::factory_from_options(options))),
            )
        };
        Self {
            clients: Vec::new(),
            stats: Arc::new(WebSeedStats::new()),
            piece_length,
            total_length,
            live_group: Some(group),
            tls,
            proxy_options,
            user_agent,
            request_headers,
            auth_factory,
            auth_options,
            request_timeout,
            connect_timeout,
            network_policy: Some(policy),
            dns_resolver: None,
            http_clients: tokio::sync::Mutex::new(HashMap::new()),
            uri_state: WebSeedUriState::default(),
        }
    }

    pub(crate) fn set_dns_resolver(&mut self, resolver: crate::dns::DnsResolver) {
        self.dns_resolver = Some(resolver);
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
            if !client.is_available() || !client.can_request(piece_index) {
                debug!(
                    index = i,
                    url = %aria2_protocol::http::redirect::url_origin_for_logging(client.url()),
                    "Skipping unavailable or busy web-seed"
                );
                continue;
            }

            let mut probe_guard = match self.uri_state.admit_request(client.url(), None).await {
                UriProbeAdmission::Unavailable => continue,
                UriProbeAdmission::Proceed => None,
                UriProbeAdmission::Probe(guard) => Some(guard),
            };

            let result = client
                .download_piece_result_with_response_status(
                    piece_index,
                    self.piece_length as u64,
                    piece_index as u64 * self.piece_length as u64,
                    piece_data_length,
                    network_activity,
                    |status| {
                        if status == 404 {
                            self.uri_state.mark_unavailable(client.url(), None);
                        } else if probe_guard.is_some() {
                            self.uri_state.mark_probed(client.url(), None);
                        }
                        drop(probe_guard.take());
                    },
                )
                .await;
            match result {
                Ok(data) => {
                    if probe_guard.is_some() {
                        self.uri_state.mark_probed(client.url(), None);
                    }
                    debug!(
                        piece_index,
                        seed_index = i,
                        url = %aria2_protocol::http::redirect::url_origin_for_logging(client.url()),
                        size = data.len(),
                        "Piece downloaded from web-seed"
                    );
                    return Ok(data);
                }
                Err(e) => {
                    if probe_guard.take().is_some() {
                        self.uri_state.mark_probed(client.url(), None);
                    }
                    warn!(
                        piece_index,
                        seed_index = i,
                        url = %aria2_protocol::http::redirect::url_origin_for_logging(client.url()),
                        error_kind = web_seed_error_kind(&e),
                        http_status = web_seed_error_status(&e),
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
                    url = %aria2_protocol::http::redirect::url_origin_for_logging(client.url()),
                    "Skipping unavailable web-seed"
                );
                continue;
            }

            let mut probe_guard = match self.uri_state.admit_request(client.url(), None).await {
                UriProbeAdmission::Unavailable => continue,
                UriProbeAdmission::Proceed => None,
                UriProbeAdmission::Probe(guard) => Some(guard),
            };

            let result = client
                .download_piece_result_with_response_status(
                    piece_index,
                    piece_length,
                    piece_offset,
                    length,
                    None,
                    |status| {
                        if status == 404 {
                            self.uri_state.mark_unavailable(client.url(), None);
                        } else if probe_guard.is_some() {
                            self.uri_state.mark_probed(client.url(), None);
                        }
                        drop(probe_guard.take());
                    },
                )
                .await;
            match result {
                Ok(data) => {
                    if probe_guard.is_some() {
                        self.uri_state.mark_probed(client.url(), None);
                    }
                    debug!(
                        piece_index,
                        seed_index = i,
                        url = %aria2_protocol::http::redirect::url_origin_for_logging(client.url()),
                        size = data.len(),
                        "Piece downloaded from web-seed"
                    );
                    return Ok(data);
                }
                Err(e) => {
                    if probe_guard.take().is_some() {
                        self.uri_state.mark_probed(client.url(), None);
                    }
                    warn!(
                        piece_index,
                        seed_index = i,
                        url = %aria2_protocol::http::redirect::url_origin_for_logging(client.url()),
                        error_kind = web_seed_error_kind(&e),
                        http_status = web_seed_error_status(&e),
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
}

fn web_seed_error_kind(error: &WebSeedError) -> &'static str {
    match error {
        WebSeedError::HttpStatus(_) => "http_status",
        WebSeedError::Failure(_) => "request_failure",
    }
}

fn web_seed_error_status(error: &WebSeedError) -> Option<u16> {
    match error {
        WebSeedError::HttpStatus(status) => Some(*status),
        WebSeedError::Failure(_) => None,
    }
}

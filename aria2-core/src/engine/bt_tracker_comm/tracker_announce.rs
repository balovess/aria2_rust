//! Unified tracker announce dispatcher integrating BtAnnounce state machine
//! with HTTP, WebSocket, and UDP tracker backends.
//!
//! # C++ Reference
//!
//! In C++ aria2, `DefaultBtAnnounce` creates either an `HttpRequestCommand`
//! or a `UDPTrackerRequest` depending on the tracker URL scheme. The dispatch
//! happens inside `DefaultBtAnnounce::getAnnounceUrl()` (HTTP) and
//! `DefaultBtAnnounce::createUDPTrackerRequest()` (UDP). WebSocket trackers
//! use the same announce state machine but a JSON WebTorrent-compatible
//! transport.
//!
//! This module unifies both paths through a single `TrackerAnnouncer` that
//! uses the `BtAnnounce` state machine to decide *when* and *what* to
//! announce, then routes to the correct backend based on URL scheme.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use super::bt_announce::{BtAnnounce, is_udp_tracker};
use super::types::AnnounceEvent;
use crate::engine::udp_tracker_client::SharedUdpClient;
use crate::engine::udp_tracker_manager::UdpTrackerManager;
use crate::http::client_identity::ClientTlsConfig;
use crate::network::OutboundNetworkPolicy;
use crate::request::request_group::DownloadOptions;
use aria2_protocol::bittorrent::tracker::public_list::{PublicTrackerList, TrackerFailureKind};
use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::UdpError;

/// Result of a tracker announce operation (HTTP, WebSocket, or UDP).
#[derive(Debug, Clone)]
pub struct AnnounceResult {
    /// Peer addresses discovered from the tracker.
    pub peers: Vec<(String, u16)>,
    /// Recommended announce interval from the tracker response.
    pub interval: Duration,
    /// Number of seeders reported by the tracker, when supplied.
    pub seeders: Option<i64>,
    /// Number of leechers reported by the tracker, when supplied.
    pub leechers: Option<i64>,
    /// The announce event that was sent.
    pub event: AnnounceEvent,
    /// The tracker URL that was used for this announce.
    pub tracker_url: String,
}

/// Shared tracker state exposed to the application/RPC layer.
///
/// The registry keeps this snapshot separate from the immutable compatibility
/// `BtAnnounce` handle. The download command owns the live `TrackerAnnouncer`,
/// so it publishes here whenever that state changes.
#[derive(Debug, Clone, Default)]
pub struct TrackerRuntimeSnapshot {
    /// Tracker URLs grouped by announce tier in their current failover order.
    pub tracker_tiers: Vec<Vec<String>>,
    /// Tracker selected for the next announce attempt.
    pub current_url: Option<String>,
    /// Tracker URL used by the most recent announce attempt.
    pub last_attempt_url: Option<String>,
    pub announce_ready: bool,
    pub all_failed: bool,
    /// Failure category from the most recent announce attempt, if it failed.
    pub last_failure_kind: Option<TrackerFailureKind>,
    pub in_flight: u32,
    pub interval_secs: u64,
    pub min_interval_secs: u64,
    pub seeders: Option<i64>,
    pub leechers: Option<i64>,
    pub tracker_id: String,
    pub seconds_since_last_success: Option<u64>,
    /// Live state for each tracker URL, in announce-list order.
    pub trackers: Vec<TrackerRuntimeInfo>,
}

/// Live state for one tracker URL returned by `aria2.getTrackers`.
#[derive(Debug, Clone, Default)]
pub struct TrackerRuntimeInfo {
    pub uri: String,
    pub tier: usize,
    pub current: bool,
    pub last_attempt: bool,
    pub announce_ready: bool,
    pub all_failed: bool,
    pub in_flight: u32,
    pub interval_secs: u64,
    pub min_interval_secs: u64,
    pub seeders: Option<i64>,
    pub leechers: Option<i64>,
    pub tracker_id: String,
    pub seconds_since_last_success: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct TrackerState {
    in_flight: bool,
    interval_secs: u64,
    min_interval_secs: u64,
    seeders: Option<i64>,
    leechers: Option<i64>,
    tracker_id: String,
    last_success_at: Option<Instant>,
    last_failure_kind: Option<TrackerFailureKind>,
}

/// Registry-safe handle for the live tracker snapshot.
pub type SharedTrackerRuntime = Arc<std::sync::RwLock<TrackerRuntimeSnapshot>>;

impl TrackerRuntimeSnapshot {
    /// Build an initial snapshot from the compatibility announce state.
    pub fn from_bt_announce(announce: &BtAnnounce) -> Self {
        let announce_list = announce.announce_list();
        let tracker_tiers: Vec<Vec<String>> = (0..announce_list.tier_count())
            .map(|tier| {
                let mut urls = Vec::new();
                let mut entry = 0;
                while let Some(url) = announce_list.get_tracker_url(tier, entry) {
                    urls.push(url.clone());
                    entry += 1;
                }
                urls
            })
            .collect();

        let trackers = tracker_tiers
            .iter()
            .enumerate()
            .flat_map(|(tier, uris)| {
                uris.iter().map(move |uri| TrackerRuntimeInfo {
                    uri: uri.clone(),
                    tier: tier + 1,
                    current: announce
                        .announce_list()
                        .get_announce()
                        .is_some_and(|current| current == uri),
                    ..TrackerRuntimeInfo::default()
                })
            })
            .collect();

        Self {
            tracker_tiers,
            current_url: announce.announce_list().get_announce().map(str::to_owned),
            last_attempt_url: None,
            last_failure_kind: None,
            announce_ready: announce.is_announce_ready(),
            all_failed: announce.is_all_announce_failed(),
            in_flight: announce.in_flight_announces(),
            interval_secs: announce.interval().as_secs(),
            min_interval_secs: announce.min_interval().as_secs(),
            seeders: announce.complete(),
            leechers: announce.incomplete(),
            tracker_id: announce.tracker_id().to_string(),
            seconds_since_last_success: announce.seconds_since_last_success(),
            trackers,
        }
    }
}

/// Unified tracker announcer that dispatches HTTP, WebSocket, and UDP tracker announces
/// through the `BtAnnounce` state machine.
///
/// This replaces the ad-hoc UDP tracker usage in `discover_peers()` with
/// a proper state-machine-driven approach that:
/// - Uses `BtAnnounce::adjust_announce_list()` to determine event and timing
/// - Routes HTTP URLs through the existing HTTP announce path
/// - Routes `ws://` and `wss://` URLs through the WebSocket tracker path
/// - Routes UDP URLs through `UdpTrackerManager`
/// - Processes responses through `BtAnnounce::process_*_response()`
/// - Tracks announce success/failure for tier rotation
pub struct TrackerAnnouncer {
    /// The core announce state machine.
    announce: BtAnnounce,
    /// Prevent duplicate stopped events after a successful announce.
    stopped_sent: bool,
    /// UDP tracker manager (created lazily when first UDP URL is seen).
    udp_manager: Option<UdpTrackerManager>,
    /// Shared UDP client for the UDP tracker manager.
    udp_client: Option<SharedUdpClient>,
    /// URL selected for the most recent announce attempt, including failures.
    last_attempt_tracker_url: Option<String>,
    /// Shared process-wide catalog for public tracker health feedback.
    public_tracker_catalog: Option<Arc<PublicTrackerList>>,
    /// Public URLs appended to this command's announce list.
    public_tracker_urls: HashSet<String>,
    /// Tracker URLs excluded by the download options, including `*`.
    excluded_tracker_urls: Vec<String>,
    /// Failure classification from the most recent announce attempt.
    last_failure_kind: Option<TrackerFailureKind>,
    /// Existing download TLS settings used by HTTPS tracker announces.
    http_tls: ClientTlsConfig,
    /// Shared source-selection policy for every tracker transport.
    outbound_network_policy: Arc<OutboundNetworkPolicy>,
    /// Reusable HTTP clients keyed by the selected local source address.
    http_clients: HashMap<Option<std::net::IpAddr>, reqwest::Client>,
    /// Download options used by WebSocket tracker announces for proxy and
    /// timeout selection.
    websocket_options: DownloadOptions,
    /// Per-download tracker request timeout.
    tracker_timeout_secs: u64,
    /// Per-download tracker connection timeout.
    tracker_connect_timeout_secs: u64,
    /// Total shutdown budget for all stopped announce attempts.
    stopped_timeout: Duration,
    /// Optional registry-visible mirror of the live announcer state.
    runtime_state: Option<SharedTrackerRuntime>,
    /// Per-URL state retained independently of the task-level announce state.
    tracker_states: HashMap<String, TrackerState>,
}

impl TrackerAnnouncer {
    /// Create a new tracker announcer from an announce list and optional single URL.
    pub fn new(announce_list: &[Vec<String>], announce: &Option<String>) -> Self {
        Self {
            announce: BtAnnounce::new(announce_list, announce),
            stopped_sent: false,
            udp_manager: None,
            udp_client: None,
            last_attempt_tracker_url: None,
            public_tracker_catalog: None,
            public_tracker_urls: HashSet::new(),
            excluded_tracker_urls: Vec::new(),
            last_failure_kind: None,
            http_tls: ClientTlsConfig::default(),
            outbound_network_policy: Arc::new(OutboundNetworkPolicy::direct()),
            http_clients: HashMap::new(),
            websocket_options: DownloadOptions::default(),
            tracker_timeout_secs: 60,
            tracker_connect_timeout_secs: 60,
            stopped_timeout: Duration::from_secs(crate::constants::BT_TRACKER_STOPPED_TIMEOUT_SECS),
            runtime_state: None,
            tracker_states: HashMap::new(),
        }
    }

    /// Create from an existing `BtAnnounce` and optional shared UDP client.
    pub fn with_udp_client(announce: BtAnnounce, udp_client: Option<SharedUdpClient>) -> Self {
        Self {
            announce,
            stopped_sent: false,
            udp_manager: None,
            udp_client,
            last_attempt_tracker_url: None,
            public_tracker_catalog: None,
            public_tracker_urls: HashSet::new(),
            excluded_tracker_urls: Vec::new(),
            last_failure_kind: None,
            http_tls: ClientTlsConfig::default(),
            outbound_network_policy: Arc::new(OutboundNetworkPolicy::direct()),
            http_clients: HashMap::new(),
            websocket_options: DownloadOptions::default(),
            tracker_timeout_secs: 60,
            tracker_connect_timeout_secs: 60,
            stopped_timeout: Duration::from_secs(crate::constants::BT_TRACKER_STOPPED_TIMEOUT_SECS),
            runtime_state: None,
            tracker_states: HashMap::new(),
        }
    }

    /// Attach the registry-visible mirror of this announcer's live state.
    pub fn set_runtime_snapshot(&mut self, state: SharedTrackerRuntime) {
        self.runtime_state = Some(state);
        self.publish_runtime_snapshot();
    }

    /// Return the complete live tracker state for RPC or diagnostics.
    pub fn runtime_snapshot(&self) -> TrackerRuntimeSnapshot {
        let mut snapshot = TrackerRuntimeSnapshot::from_bt_announce(&self.announce);
        snapshot.last_attempt_url = self.last_attempt_tracker_url.clone();
        snapshot.last_failure_kind = self.last_failure_kind;
        snapshot.trackers = self.tracker_runtime_infos();
        snapshot
    }

    fn tracker_runtime_infos(&self) -> Vec<TrackerRuntimeInfo> {
        let current = self.announce.announce_list().get_announce();
        let last_attempt = self.last_attempt_tracker_url.as_deref();
        let ready = self.announce.is_announce_ready();
        let mut trackers = Vec::new();

        for tier in 0..self.announce.announce_list().tier_count() {
            let mut entry = 0;
            while let Some(uri) = self.announce.announce_list().get_tracker_url(tier, entry) {
                let state = self.tracker_states.get(uri);
                trackers.push(TrackerRuntimeInfo {
                    uri: uri.clone(),
                    tier: tier + 1,
                    current: current == Some(uri.as_str()),
                    last_attempt: last_attempt == Some(uri.as_str()),
                    announce_ready: ready && current == Some(uri.as_str()),
                    all_failed: state.is_some_and(|state| state.last_failure_kind.is_some()),
                    in_flight: state.map_or(0, |state| u32::from(state.in_flight)),
                    interval_secs: state.map_or(0, |state| state.interval_secs),
                    min_interval_secs: state.map_or(0, |state| state.min_interval_secs),
                    seeders: state.and_then(|state| state.seeders),
                    leechers: state.and_then(|state| state.leechers),
                    tracker_id: state.map_or_else(String::new, |state| state.tracker_id.clone()),
                    seconds_since_last_success: state.and_then(|state| {
                        state.last_success_at.map(|time| time.elapsed().as_secs())
                    }),
                });
                entry += 1;
            }
        }
        trackers
    }

    fn tracker_attempt_started(&mut self, tracker_url: &str) {
        self.tracker_states
            .entry(tracker_url.to_string())
            .or_default()
            .in_flight = true;
    }

    fn tracker_attempt_finished(&mut self, tracker_url: &str, succeeded: bool) {
        let state = self
            .tracker_states
            .entry(tracker_url.to_string())
            .or_default();
        state.in_flight = false;
        if succeeded {
            state.last_failure_kind = None;
            state.last_success_at = Some(Instant::now());
        } else {
            state.last_failure_kind = self.last_failure_kind;
        }
    }

    fn update_tracker_stats(
        &mut self,
        tracker_url: &str,
        interval_secs: u64,
        min_interval_secs: u64,
        seeders: Option<i64>,
        leechers: Option<i64>,
        tracker_id: Option<&str>,
    ) {
        let state = self
            .tracker_states
            .entry(tracker_url.to_string())
            .or_default();
        state.interval_secs = interval_secs;
        state.min_interval_secs = min_interval_secs;
        state.seeders = seeders;
        state.leechers = leechers;
        if let Some(tracker_id) = tracker_id {
            state.tracker_id = tracker_id.to_string();
        }
    }

    /// Publish the current live state without holding a lock across callers.
    pub fn publish_runtime_snapshot(&self) {
        let Some(state) = self.runtime_state.as_ref() else {
            return;
        };
        let snapshot = self.runtime_snapshot();
        if let Ok(mut current) = state.write() {
            *current = snapshot;
        }
    }

    /// Set the shared UDP client for UDP tracker announces.
    pub fn set_udp_client(&mut self, client: SharedUdpClient) {
        self.udp_client = Some(client);
    }

    /// Apply the existing aria2-compatible TLS options to HTTP tracker calls.
    pub(crate) fn set_http_tls_config(&mut self, config: ClientTlsConfig) {
        self.http_tls = config;
        self.http_clients.clear();
    }

    /// Set the shared source policy used by HTTP, WebSocket, and UDP trackers.
    pub fn set_outbound_network_policy(&mut self, policy: Arc<OutboundNetworkPolicy>) {
        self.outbound_network_policy = policy;
        self.http_clients.clear();
    }

    /// Apply per-download proxy and timeout options to WebSocket trackers.
    pub fn set_websocket_options(&mut self, options: &DownloadOptions) {
        self.websocket_options = options.clone();
    }

    /// Set the per-download tracker request and connection timeouts.
    pub fn set_timeouts(&mut self, request: Duration, connect: Duration) {
        self.tracker_timeout_secs = request.as_secs().max(1);
        self.tracker_connect_timeout_secs = connect.as_secs().max(1);
    }

    /// Set the total time allowed for stopped announce cleanup.
    pub fn set_stopped_timeout(&mut self, timeout: Duration) {
        self.stopped_timeout = timeout.max(Duration::from_millis(1));
    }

    /// Apply the user-defined announce interval from `bt-tracker-interval`.
    pub fn set_user_defined_interval(&mut self, interval: Duration) {
        self.announce.set_user_defined_interval(interval);
    }

    /// Apply the announce encryption and external-IP options.
    pub fn set_announce_options(&mut self, force_encryption: bool, external_ip: Option<String>) {
        self.announce.set_force_encryption(force_encryption);
        self.announce.set_external_ip(external_ip);
    }

    /// Returns true if any announce is ready (stopped, completed, or periodic).
    pub fn is_announce_ready(&self) -> bool {
        self.announce.is_announce_ready()
    }

    /// Returns true if a periodic announce is ready.
    pub fn is_default_announce_ready(&self) -> bool {
        self.announce.is_default_announce_ready()
    }

    /// Return the delay until the next protocol-defined periodic announce.
    pub fn next_default_announce_delay(&self) -> Option<std::time::Duration> {
        self.announce.next_default_announce_delay()
    }

    /// Return the tracker selected for the next announce attempt.
    pub fn current_tracker_url(&self) -> Option<&str> {
        self.announce.announce_list().get_announce()
    }

    /// Return the URL used by the most recent announce attempt.
    pub fn last_attempt_tracker_url(&self) -> Option<&str> {
        self.last_attempt_tracker_url.as_deref()
    }

    /// Attach the shared catalog and the public URLs owned by this command.
    pub fn set_public_tracker_catalog(
        &mut self,
        catalog: Arc<PublicTrackerList>,
        public_tracker_urls: HashSet<String>,
    ) {
        self.public_tracker_catalog = Some(catalog);
        self.public_tracker_urls = public_tracker_urls;
    }

    /// Apply the download's tracker exclusion policy to future catalog merges.
    pub fn set_excluded_tracker_urls(&mut self, excluded: Vec<String>) {
        self.excluded_tracker_urls = excluded;
    }

    /// Merge newly available public trackers into this running download.
    ///
    /// The announce list owns the ordering rule: torrent and explicit user
    /// trackers remain ahead of public trackers, and duplicate URLs are
    /// ignored. The limit is per download, so a refresh cannot grow an
    /// announce list without bound.
    pub async fn sync_public_trackers(&mut self, max_trackers: usize) -> usize {
        if self.public_tracker_urls.len() >= max_trackers {
            return 0;
        }
        let Some(catalog) = self.public_tracker_catalog.as_ref().cloned() else {
            return 0;
        };
        let available = catalog.available_snapshot().await;
        let remaining = max_trackers - self.public_tracker_urls.len();
        let mut tiers = Vec::with_capacity(remaining);
        for entry in available.iter() {
            if self
                .excluded_tracker_urls
                .iter()
                .any(|excluded| excluded == "*" || excluded == &entry.url)
                || self.public_tracker_urls.contains(&entry.url)
                || self.announce.announce_list().contains_url(&entry.url)
            {
                continue;
            }
            tiers.push(vec![entry.url.clone()]);
            if tiers.len() == remaining {
                break;
            }
        }
        let added = self.announce.append_tracker_tiers(&tiers);
        let added_count = added.len();
        self.public_tracker_urls.extend(added);
        if added_count > 0 {
            self.publish_runtime_snapshot();
        }
        added_count
    }

    /// Execute a tracker announce, dispatching to HTTP or UDP as appropriate.
    ///
    /// This is the main entry point called from the download loop. It:
    /// 1. Checks if an announce is ready via the state machine
    /// 2. Determines the current tracker URL and event
    /// 3. Dispatches to the appropriate backend
    /// 4. Processes the response through the state machine
    /// 5. Returns the result with discovered peers
    ///
    /// Returns `None` if no announce is ready or the state machine decides
    /// not to announce (e.g., all tiers failed).
    pub async fn announce(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
    ) -> Option<AnnounceResult> {
        // Check if announce is ready
        if !self.announce.is_announce_ready() {
            return None;
        }
        if !self.announce.adjust_announce_list() {
            return None;
        }

        let tracker_url = self.announce.announce_list().get_announce()?.to_string();
        self.last_attempt_tracker_url = Some(tracker_url.clone());
        self.last_failure_kind = None;
        self.tracker_attempt_started(&tracker_url);
        self.publish_runtime_snapshot();
        let is_udp = is_udp_tracker(&tracker_url);
        let is_websocket = reqwest::Url::parse(&tracker_url)
            .ok()
            .is_some_and(|url| matches!(url.scheme(), "ws" | "wss"));
        let event = self.announce.announce_list().get_event();

        // Determine whether this is a UDP, WebSocket, or HTTP tracker.
        let result = if is_udp {
            self.announce_udp(info_hash, peer_id, downloaded, left, uploaded, &tracker_url)
                .await
        } else if is_websocket {
            self.announce_websocket(info_hash, peer_id, downloaded, left, uploaded, &tracker_url)
                .await
        } else {
            // HTTP announce — build URL and dispatch.
            self.announce_http(
                info_hash,
                peer_id,
                downloaded,
                left,
                uploaded,
                event,
                &tracker_url,
            )
            .await
        };

        self.tracker_attempt_finished(&tracker_url, result.is_some());
        self.publish_runtime_snapshot();

        if self.public_tracker_urls.contains(&tracker_url)
            && let Some(catalog) = self.public_tracker_catalog.as_ref()
        {
            if result.is_some() {
                catalog.record_success(&tracker_url).await;
            } else {
                catalog
                    .record_failure_kind(
                        &tracker_url,
                        self.last_failure_kind
                            .unwrap_or(TrackerFailureKind::MalformedResponse),
                    )
                    .await;
            }
        }
        result
    }

    /// Execute a WebSocket tracker announce using the same lifecycle state
    /// machine as HTTP and UDP trackers.
    async fn announce_websocket(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
        tracker_url: &str,
    ) -> Option<AnnounceResult> {
        let event = self.announce.announce_list().get_event();
        self.announce.announce_start();
        self.publish_runtime_snapshot();

        let response = crate::engine::websocket_tracker::announce_with_policy(
            tracker_url,
            crate::engine::websocket_tracker::AnnounceRequest {
                info_hash,
                peer_id,
                downloaded,
                left,
                uploaded,
                numwant: self.announce.numwant(),
                port: self.announce.tcp_port(),
                event,
                options: &self.websocket_options,
            },
            &self.outbound_network_policy,
        )
        .await;

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                warn!(tracker = %tracker_url, %error, "WebSocket tracker announce failed");
                self.last_failure_kind = Some(TrackerFailureKind::Network);
                self.announce.announce_failure();
                return None;
            }
        };

        self.announce.process_announce_stats(
            response.interval,
            response.min_interval,
            response.seeders,
            response.leechers,
        );
        self.update_tracker_stats(
            tracker_url,
            response.interval.unwrap_or_default(),
            response.min_interval.unwrap_or_default(),
            response.seeders,
            response.leechers,
            None,
        );
        self.announce.announce_success();

        let peers = response
            .peers
            .into_iter()
            .map(|peer| (peer.ip().to_string(), peer.port()))
            .collect();
        Some(AnnounceResult {
            peers,
            interval: self.announce.interval(),
            seeders: self.announce.complete(),
            leechers: self.announce.incomplete(),
            event,
            tracker_url: tracker_url.to_string(),
        })
    }

    /// Execute a UDP tracker announce.
    async fn announce_udp(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
        tracker_url: &str, // must be &str for lifetime flexibility
    ) -> Option<AnnounceResult> {
        let event = self.announce.announce_list().get_event();
        let udp_event = self.announce.current_udp_event();
        let numwant = self.announce.numwant();
        // Initialize UDP manager lazily
        if self.udp_manager.is_none() {
            if let Some(ref client) = self.udp_client {
                let mgr = UdpTrackerManager::new(Arc::clone(client)).await;
                self.udp_manager = Some(mgr);
            } else {
                // No UDP client configured — try to create one
                match crate::engine::udp_tracker_client::UdpTrackerClient::new_with_policy(
                    0,
                    &self.outbound_network_policy,
                )
                .await
                {
                    Ok(client) => {
                        let shared = Arc::new(tokio::sync::Mutex::new(client));
                        self.udp_client = Some(Arc::clone(&shared));
                        let mgr = UdpTrackerManager::new(shared).await;
                        self.udp_manager = Some(mgr);
                    }
                    Err(e) => {
                        warn!("[BT] Failed to create UDP tracker client: {}", e);
                        self.last_failure_kind = Some(TrackerFailureKind::Network);
                        self.announce.announce_failure();
                        return None;
                    }
                }
            }
        }

        {
            let mgr = self.udp_manager.as_mut()?;
            mgr.set_request_timeout(Duration::from_secs(self.tracker_timeout_secs));

            // The state machine selects one URL per attempt. Keep the UDP manager
            // scoped to that URL so responses cannot be attributed to a different
            // public tracker accumulated by an earlier attempt.
            mgr.use_tracker_url(tracker_url).await;
        }

        // Signal announce start
        self.announce.announce_start();
        self.publish_runtime_snapshot();

        // The event and numwant were captured before network I/O so the
        // request and result describe the same state-machine transition.

        debug!(
            "[BT] Announcing to UDP tracker {} (event={:?}, udp_event={})",
            tracker_url, event, udp_event
        );

        let responses = self
            .udp_manager
            .as_mut()?
            .announce(
                info_hash,
                peer_id,
                downloaded as i64,
                left as i64,
                uploaded as i64,
                udp_event,
                numwant as i32,
            )
            .await;

        if responses.is_empty() {
            warn!("[BT] UDP tracker {} returned no response", tracker_url);
            let last_error = self.udp_manager.as_mut()?.last_announce_error().await;
            self.last_failure_kind = Some(match last_error {
                Some(UdpError::TrackerError) => TrackerFailureKind::TrackerRejected,
                Some(UdpError::MalformedResponse) => TrackerFailureKind::MalformedResponse,
                Some(UdpError::Network) => TrackerFailureKind::Network,
                Some(UdpError::Timeout) | None => TrackerFailureKind::Timeout,
                Some(UdpError::Success | UdpError::Shutdown) => {
                    TrackerFailureKind::MalformedResponse
                }
            });
            self.announce.announce_failure();
            return None;
        }

        // Process the first response through the state machine
        let response = &responses[0];
        let peers = self.announce.process_udp_announce_response(response);
        self.update_tracker_stats(
            tracker_url,
            response.interval as u64,
            response.interval as u64,
            Some(response.seeders as i64),
            Some(response.leechers as i64),
            None,
        );
        self.announce.announce_success();

        // Collect additional peers from subsequent responses
        let mut all_peers = peers;
        if responses.len() > 1 {
            for resp in &responses[1..] {
                all_peers.extend_from_slice(&resp.peers);
            }
        }

        // Deduplicate peers
        all_peers.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        all_peers.dedup();

        Some(AnnounceResult {
            peers: all_peers,
            interval: self.announce.interval(),
            seeders: self.announce.complete(),
            leechers: self.announce.incomplete(),
            event,
            tracker_url: tracker_url.to_string(),
        })
    }

    /// Execute an HTTP tracker announce.
    #[allow(clippy::too_many_arguments)]
    async fn announce_http(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
        event: AnnounceEvent,
        tracker_url: &str,
    ) -> Option<AnnounceResult> {
        // Build the announce URL through BtAnnounce state machine
        let url = self.announce.get_announce_url_without_adjustment(
            info_hash, peer_id, uploaded, downloaded, left, None,
        )?;

        // Signal announce start
        self.announce.announce_start();
        self.publish_runtime_snapshot();

        debug!(
            "[BT] Announcing to HTTP tracker {} (event={:?})",
            tracker_url, event
        );

        // Send HTTP request
        let parsed_url = reqwest::Url::parse(&url).ok();
        let tracker_port = parsed_url
            .as_ref()
            .and_then(reqwest::Url::port_or_known_default)
            .unwrap_or(80);
        let local_address = match parsed_url.as_ref().and_then(reqwest::Url::host_str) {
            Some(host) => {
                match self
                    .outbound_network_policy
                    .source_for_host(host, tracker_port)
                    .await
                {
                    Ok(address) => address,
                    Err(error) => {
                        warn!(tracker = %tracker_url, %error, "HTTP tracker has no compatible outbound source");
                        self.last_failure_kind = Some(TrackerFailureKind::Network);
                        self.announce.announce_failure();
                        return None;
                    }
                }
            }
            None if self.outbound_network_policy.is_direct() => None,
            None => {
                warn!(tracker = %tracker_url, "HTTP tracker URL has no host");
                self.last_failure_kind = Some(TrackerFailureKind::Network);
                self.announce.announce_failure();
                return None;
            }
        };

        let client = if let Some(client) = self.http_clients.get(&local_address) {
            client.clone()
        } else {
            let client = match crate::engine::http_tracker_client::build_tracker_client_with_source(
                self.tracker_timeout_secs,
                self.tracker_connect_timeout_secs,
                &self.http_tls,
                local_address,
            ) {
                Ok(client) => client,
                Err(error) => {
                    warn!(tracker = %tracker_url, %error, "Failed to build HTTP tracker client");
                    self.last_failure_kind = Some(TrackerFailureKind::Network);
                    self.announce.announce_failure();
                    return None;
                }
            };
            self.http_clients.insert(local_address, client.clone());
            client
        };

        match client.get(&url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    warn!(
                        "[BT] HTTP tracker {} returned status {}",
                        tracker_url,
                        resp.status()
                    );
                    self.last_failure_kind = Some(
                        if resp.status().is_server_error()
                            || matches!(resp.status().as_u16(), 408 | 425 | 429)
                        {
                            TrackerFailureKind::RemoteTemporary
                        } else {
                            TrackerFailureKind::TrackerRejected
                        },
                    );
                    self.announce.announce_failure();
                    return None;
                }

                match resp.bytes().await {
                    Ok(body) => {
                        match aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(
                            &body,
                        ) {
                            Ok(tracker_resp) => {
                                if tracker_resp.is_failure() {
                                    let reason = tracker_resp
                                        .failure_reason
                                        .unwrap_or_else(|| "tracker failure".to_string());
                                    warn!("[BT] HTTP tracker {} failure: {}", tracker_url, reason);
                                    self.last_failure_kind =
                                        Some(TrackerFailureKind::TrackerRejected);
                                    self.announce.announce_failure();
                                    return None;
                                }

                                // Process through BtAnnounce state machine
                                match self.announce.process_announce_response(&tracker_resp) {
                                    Ok(peers) => {
                                        self.update_tracker_stats(
                                            tracker_url,
                                            tracker_resp.interval as u64,
                                            tracker_resp.min_interval.map_or(0, u64::from),
                                            tracker_resp.seeders.map(i64::from),
                                            tracker_resp.leechers.map(i64::from),
                                            tracker_resp.tracker_id.as_deref(),
                                        );
                                        self.announce.announce_success();
                                        let interval = self.announce.interval();
                                        let seeders = self.announce.complete();
                                        let leechers = self.announce.incomplete();
                                        Some(AnnounceResult {
                                            peers,
                                            interval,
                                            seeders,
                                            leechers,
                                            event,
                                            tracker_url: tracker_url.to_string(),
                                        })
                                    }
                                    Err(e) => {
                                        warn!(
                                            "[BT] HTTP tracker {} response processing failed: {}",
                                            tracker_url, e
                                        );
                                        self.last_failure_kind =
                                            Some(TrackerFailureKind::MalformedResponse);
                                        self.announce.announce_failure();
                                        None
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "[BT] HTTP tracker {} response parse failed: {}",
                                    tracker_url, e
                                );
                                self.last_failure_kind =
                                    Some(TrackerFailureKind::MalformedResponse);
                                self.announce.announce_failure();
                                None
                            }
                        }
                    }
                    Err(e) => {
                        warn!("[BT] HTTP tracker {} body read failed: {}", tracker_url, e);
                        self.last_failure_kind = Some(if e.is_timeout() {
                            TrackerFailureKind::Timeout
                        } else {
                            TrackerFailureKind::Network
                        });
                        self.announce.announce_failure();
                        None
                    }
                }
            }
            Err(e) => {
                warn!("[BT] HTTP tracker {} request failed: {}", tracker_url, e);
                self.last_failure_kind = Some(if e.is_timeout() {
                    TrackerFailureKind::Timeout
                } else {
                    TrackerFailureKind::Network
                });
                self.announce.announce_failure();
                None
            }
        }
    }

    /// Send a "stopped" event to all trackers before shutdown.
    ///
    /// C++ aria2 sends stopped events during `DownloadEngine::setHaltRequested()`.
    /// This should be called before the download command exits.
    pub async fn announce_stopped(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
    ) {
        if self.stopped_sent {
            return;
        }
        self.announce.set_runtime_halted(true);
        self.publish_runtime_snapshot();

        // Try to send stopped event to all applicable tiers
        let mut attempts = 0;
        const MAX_STOPPED_ATTEMPTS: usize = 5;
        let mut sent_successfully = false;

        let deadline = Instant::now() + self.stopped_timeout;
        while self.announce.is_stopped_announce_ready() && attempts < MAX_STOPPED_ATTEMPTS {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warn!(
                    "[BT] Stopped announce budget exhausted after {} attempts",
                    attempts
                );
                break;
            }
            let result = match tokio::time::timeout(
                remaining,
                self.announce(info_hash, peer_id, downloaded, left, uploaded),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    warn!(
                        "[BT] Stopped announce timed out after {:?}",
                        self.stopped_timeout
                    );
                    break;
                }
            };
            if let Some(result) = result {
                info!(
                    "[BT] Sent stopped event to {} ({} peers in response)",
                    result.tracker_url,
                    result.peers.len()
                );
                sent_successfully = true;
            }
            attempts += 1;
        }
        self.stopped_sent = sent_successfully;
    }

    /// Send a "completed" event to all applicable trackers.
    ///
    /// Called when the download finishes all pieces.
    pub async fn announce_completed(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        uploaded: u64,
    ) {
        self.announce.set_download_complete(true);
        self.publish_runtime_snapshot();

        if let Some(result) = self
            .announce(info_hash, peer_id, downloaded, 0, uploaded)
            .await
        {
            info!(
                "[BT] Sent completed event to {} ({:?} seeders, {:?} leechers)",
                result.tracker_url, result.seeders, result.leechers
            );
        }
    }

    /// Get the current announce interval.
    pub fn interval(&self) -> Duration {
        self.announce.interval()
    }

    /// Get the current minimum interval.
    pub fn min_interval(&self) -> Duration {
        self.announce.min_interval()
    }

    /// Get access to the inner BtAnnounce for advanced state queries.
    pub fn bt_announce(&self) -> &BtAnnounce {
        &self.announce
    }

    /// Get mutable access to the inner BtAnnounce for state updates.
    pub fn bt_announce_mut(&mut self) -> &mut BtAnnounce {
        &mut self.announce
    }

    /// Check if all tracker tiers have failed.
    pub fn is_all_announce_failed(&self) -> bool {
        self.announce.is_all_announce_failed()
    }

    /// Reset the announce state (e.g., after a long pause).
    pub fn reset_announce(&mut self) {
        self.announce.reset_announce();
        self.publish_runtime_snapshot();
    }

    /// Set whether the download has fewer than minimum peers.
    pub fn set_less_than_min_peers(&mut self, less: bool) {
        self.announce.set_less_than_min_peers(less);
        self.publish_runtime_snapshot();
    }

    /// Set the TCP port for announce URL construction.
    pub fn set_tcp_port(&mut self, port: u16) {
        self.announce.set_tcp_port(port);
    }

    /// Return the TCP port currently advertised to trackers.
    pub fn tcp_port(&self) -> u16 {
        self.announce.tcp_port()
    }

    /// Set whether the download is complete.
    pub fn set_download_complete(&mut self, complete: bool) {
        self.announce.set_download_complete(complete);
        self.publish_runtime_snapshot();
    }

    /// Set whether the runtime is halted (stopping).
    pub fn set_runtime_halted(&mut self, halted: bool) {
        self.announce.set_runtime_halted(halted);
        self.publish_runtime_snapshot();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use futures::{SinkExt, StreamExt};
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn runtime_snapshot_publishes_live_tracker_state() {
        let first = "http://tracker.example.com/one".to_string();
        let second = "udp://tracker.example.com/two".to_string();
        let shared = Arc::new(std::sync::RwLock::new(TrackerRuntimeSnapshot::default()));
        let mut announcer = TrackerAnnouncer::new(&[vec![first.clone(), second.clone()]], &None);

        announcer.set_runtime_snapshot(Arc::clone(&shared));
        announcer.last_attempt_tracker_url = Some(second.clone());
        announcer.publish_runtime_snapshot();

        {
            let snapshot = shared.read().expect("tracker runtime snapshot lock");
            assert_eq!(
                snapshot.tracker_tiers,
                vec![vec![first.clone(), second.clone()]]
            );
            assert_eq!(
                snapshot.current_url.as_deref(),
                Some("http://tracker.example.com/one")
            );
            assert_eq!(snapshot.last_attempt_url.as_deref(), Some(second.as_str()));
            assert!(snapshot.announce_ready);
            assert!(!snapshot.all_failed);
            assert_eq!(snapshot.last_failure_kind, None);
            assert_eq!(snapshot.trackers.len(), 2);
            assert!(!snapshot.trackers[0].last_attempt);
            assert!(snapshot.trackers[1].last_attempt);
            assert_eq!(snapshot.trackers[0].seeders, None);
            assert_eq!(snapshot.trackers[1].seeders, None);
        }

        announcer
            .tracker_states
            .entry(first.clone())
            .or_default()
            .seeders = Some(11);
        announcer.last_failure_kind = Some(TrackerFailureKind::Timeout);
        announcer
            .tracker_states
            .entry(second.clone())
            .or_default()
            .last_failure_kind = Some(TrackerFailureKind::Timeout);
        announcer.publish_runtime_snapshot();
        let snapshot = shared.read().expect("tracker runtime snapshot lock");
        assert_eq!(
            snapshot.last_failure_kind,
            Some(TrackerFailureKind::Timeout)
        );
        assert_eq!(snapshot.trackers[0].seeders, Some(11));
        assert!(!snapshot.trackers[0].all_failed);
        assert_eq!(snapshot.trackers[1].seeders, None);
        assert!(snapshot.trackers[1].all_failed);
    }

    #[test]
    fn tracker_timeout_options_are_stored_for_both_transports() {
        let mut announcer = TrackerAnnouncer::new(
            &[vec!["http://tracker.example.com/announce".to_string()]],
            &None,
        );
        announcer.set_timeouts(Duration::from_secs(7), Duration::from_secs(11));

        assert_eq!(announcer.tracker_timeout_secs, 7);
        assert_eq!(announcer.tracker_connect_timeout_secs, 11);
    }

    #[test]
    fn stopped_timeout_has_a_clear_default_and_can_be_configured() {
        let mut announcer = TrackerAnnouncer::new(&[], &None);
        assert_eq!(
            announcer.stopped_timeout,
            Duration::from_secs(crate::constants::BT_TRACKER_STOPPED_TIMEOUT_SECS)
        );
        announcer.set_stopped_timeout(Duration::ZERO);
        assert_eq!(announcer.stopped_timeout, Duration::from_millis(1));
    }

    #[tokio::test]
    async fn stopped_announce_respects_total_shutdown_budget() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local tracker test listener");
        let address = listener.local_addr().expect("local tracker address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept tracker request");
            tokio::time::sleep(Duration::from_secs(2)).await;
            use tokio::io::AsyncWriteExt;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
        });

        let url = format!("http://{address}/announce");
        let mut announcer = TrackerAnnouncer::new(&[vec![url]], &None);
        announcer.set_timeouts(Duration::from_secs(10), Duration::from_secs(10));
        announcer.set_stopped_timeout(Duration::from_millis(100));
        announcer.announce.set_runtime_halted(true);
        announcer.announce.announce_list_mut().tiers[0].event = AnnounceEvent::Downloading;

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            announcer.announce_stopped(&[0u8; 20], &[1u8; 20], 0, 1, 0),
        )
        .await;
        assert!(result.is_ok(), "stopped announce exceeded shutdown budget");
        assert!(!announcer.stopped_sent);

        server.await.expect("tracker test server should exit");
    }

    #[tokio::test]
    async fn tracker_request_timeout_aborts_a_slow_http_announce() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local tracker test listener");
        let address = listener.local_addr().expect("local tracker address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept tracker request");
            tokio::time::sleep(Duration::from_secs(2)).await;
            use tokio::io::AsyncWriteExt;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
        });

        let url = format!("http://{address}/announce");
        let mut announcer = TrackerAnnouncer::new(&[vec![url]], &None);
        announcer.set_timeouts(Duration::from_secs(1), Duration::from_secs(1));

        let result = tokio::time::timeout(
            Duration::from_secs(4),
            announcer.announce(&[0u8; 20], &[1u8; 20], 0, 1, 0),
        )
        .await
        .expect("tracker request should finish within the test deadline");
        assert!(result.is_none(), "slow tracker request should time out");

        server.await.expect("tracker test server should exit");
    }

    #[tokio::test]
    async fn local_http_tracker_returns_dynamic_announce_list() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local HTTP tracker fixture");
        let address = listener.local_addr().expect("local tracker address");
        let dynamic_a = "http://127.0.0.1:1/dynamic-a".to_string();
        let dynamic_b = "http://127.0.0.1:1/dynamic-b".to_string();
        let response_body = {
            let mut response = BTreeMap::new();
            response.insert(b"complete".to_vec(), BencodeValue::Int(7));
            response.insert(b"incomplete".to_vec(), BencodeValue::Int(3));
            response.insert(b"interval".to_vec(), BencodeValue::Int(60));
            response.insert(
                b"announce-list".to_vec(),
                BencodeValue::List(vec![
                    BencodeValue::List(vec![BencodeValue::Bytes(dynamic_a.clone().into_bytes())]),
                    BencodeValue::List(vec![BencodeValue::Bytes(dynamic_b.clone().into_bytes())]),
                ]),
            );
            response.insert(
                b"peers".to_vec(),
                BencodeValue::Bytes(vec![192, 0, 2, 10, 0x1A, 0xE1]),
            );
            BencodeValue::Dict(response).encode()
        };
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept tracker request");
            let mut request = vec![0u8; 4096];
            let _ = socket
                .read(&mut request)
                .await
                .expect("read tracker request");
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write tracker headers");
            socket
                .write_all(&response_body)
                .await
                .expect("write tracker response");
        });

        let initial = format!("http://{address}/announce");
        let mut announcer = TrackerAnnouncer::new(&[vec![initial.clone()]], &None);
        announcer.set_timeouts(Duration::from_secs(2), Duration::from_secs(2));

        let result = announcer
            .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
            .await
            .expect("HTTP tracker fixture should return an announce result");
        assert_eq!(result.peers, vec![("192.0.2.10".to_string(), 6881)]);
        assert_eq!(result.seeders, Some(7));
        assert_eq!(result.leechers, Some(3));
        assert!(announcer.announce.announce_list().contains_url(&initial));
        assert!(announcer.announce.announce_list().contains_url(&dynamic_a));
        assert!(announcer.announce.announce_list().contains_url(&dynamic_b));
        assert_eq!(announcer.announce.announce_list().tier_count(), 3);

        server.await.expect("HTTP tracker fixture should exit");
    }

    #[tokio::test]
    async fn http_tracker_reuses_one_policy_bound_connection_for_stopped_announce() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tracker reuse fixture");
        let address = listener
            .local_addr()
            .expect("tracker reuse fixture address");
        let response_body = {
            let mut response = BTreeMap::new();
            response.insert(b"interval".to_vec(), BencodeValue::Int(1));
            response.insert(b"peers".to_vec(), BencodeValue::Bytes(Vec::new()));
            BencodeValue::Dict(response).encode()
        };
        let server = tokio::spawn(async move {
            let (mut socket, peer) = listener.accept().await.expect("tracker should accept once");
            for _ in 0..2 {
                let mut request = Vec::new();
                loop {
                    let mut byte = [0u8; 1];
                    socket
                        .read_exact(&mut byte)
                        .await
                        .expect("read tracker request");
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                    response_body.len()
                );
                socket
                    .write_all(headers.as_bytes())
                    .await
                    .expect("write tracker response headers");
                socket
                    .write_all(&response_body)
                    .await
                    .expect("write tracker response body");
            }
            peer.ip()
        });

        let tracker_url = format!("http://{address}/announce");
        let mut announcer = TrackerAnnouncer::new(&[vec![tracker_url]], &None);
        announcer.set_outbound_network_policy(Arc::new(OutboundNetworkPolicy::single(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        )));
        announcer.set_timeouts(Duration::from_secs(2), Duration::from_secs(2));
        announcer
            .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
            .await
            .expect("initial tracker announce should succeed");
        announcer
            .announce_stopped(&[0u8; 20], &[1u8; 20], 0, 1, 0)
            .await;

        assert_eq!(
            server.await.expect("tracker reuse fixture should finish"),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
    }

    #[tokio::test]
    async fn local_udp_tracker_fixture_supports_bep15_announce() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind local UDP tracker fixture");
        let address = socket.local_addr().expect("local UDP tracker address");
        let server = tokio::spawn(async move {
            let mut request = [0u8; 256];
            let (length, peer) = socket
                .recv_from(&mut request)
                .await
                .expect("receive BEP 15 connect request");
            assert_eq!(length, 16);
            assert_eq!(&request[0..8], &0x41727101980u64.to_be_bytes());
            assert_eq!(&request[8..12], &0i32.to_be_bytes());
            let transaction = u32::from_be_bytes(request[12..16].try_into().unwrap());
            let connection_id = 0x0102_0304_0506_0708u64;
            let mut connect_response = Vec::with_capacity(16);
            connect_response.extend_from_slice(&0i32.to_be_bytes());
            connect_response.extend_from_slice(&transaction.to_be_bytes());
            connect_response.extend_from_slice(&connection_id.to_be_bytes());
            socket
                .send_to(&connect_response, peer)
                .await
                .expect("send BEP 15 connect response");

            let (length, peer) = socket
                .recv_from(&mut request)
                .await
                .expect("receive BEP 15 announce request");
            assert!(length >= 98);
            assert_eq!(&request[0..8], &connection_id.to_be_bytes());
            assert_eq!(&request[8..12], &1i32.to_be_bytes());
            let transaction = u32::from_be_bytes(request[12..16].try_into().unwrap());
            let mut announce_response = Vec::with_capacity(26);
            announce_response.extend_from_slice(&1i32.to_be_bytes());
            announce_response.extend_from_slice(&transaction.to_be_bytes());
            announce_response.extend_from_slice(&60u32.to_be_bytes());
            announce_response.extend_from_slice(&3u32.to_be_bytes());
            announce_response.extend_from_slice(&7u32.to_be_bytes());
            announce_response.extend_from_slice(&[192, 0, 2, 11, 0x1A, 0xE1]);
            socket
                .send_to(&announce_response, peer)
                .await
                .expect("send BEP 15 announce response");
        });

        let mut announcer =
            TrackerAnnouncer::new(&[vec![format!("udp://{address}/announce")]], &None);
        announcer.set_timeouts(Duration::from_secs(2), Duration::from_secs(2));
        let result = announcer
            .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
            .await
            .expect("UDP tracker fixture should return an announce result");

        assert_eq!(result.peers, vec![("192.0.2.11".to_string(), 6881)]);
        assert_eq!(result.interval, Duration::from_secs(60));
        assert_eq!(result.seeders, Some(7));
        assert_eq!(result.leechers, Some(3));
        server.await.expect("UDP tracker fixture should exit");
    }

    #[tokio::test]
    async fn refreshed_public_trackers_are_appended_after_torrent_trackers() {
        let catalog = Arc::new(PublicTrackerList::new());
        let existing_torrent_tracker = catalog
            .snapshot()
            .await
            .first()
            .expect("embedded catalog should contain a tracker")
            .url
            .clone();
        let mut announcer = TrackerAnnouncer::new(&[vec![existing_torrent_tracker.clone()]], &None);
        announcer.set_public_tracker_catalog(catalog, HashSet::new());

        let added = announcer.sync_public_trackers(3).await;

        assert_eq!(added, 3);
        assert_eq!(
            announcer
                .announce
                .announce_list()
                .get_tracker_url(0, 0)
                .map(String::as_str),
            Some(existing_torrent_tracker.as_str())
        );
        assert_eq!(announcer.announce.announce_list().tier_count(), 4);
    }

    #[tokio::test]
    async fn excluded_public_trackers_are_not_added_after_refresh() {
        let catalog = Arc::new(PublicTrackerList::new());
        let mut announcer = TrackerAnnouncer::new(&[], &None);
        announcer.set_public_tracker_catalog(catalog, HashSet::new());
        announcer.set_excluded_tracker_urls(vec!["*".to_string()]);

        assert_eq!(announcer.sync_public_trackers(3).await, 0);
        assert_eq!(announcer.announce.announce_list().tier_count(), 0);
    }

    #[tokio::test]
    async fn websocket_tracker_announces_started_and_stopped_events() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local WebSocket tracker test listener");
        let address = listener.local_addr().expect("local tracker address");
        let server = tokio::spawn(async move {
            for expected_event in ["started", "stopped"] {
                let (stream, _) = listener.accept().await.expect("accept tracker request");
                let mut websocket = tokio_tungstenite::accept_async(stream)
                    .await
                    .expect("complete WebSocket tracker handshake");
                let Some(Ok(Message::Text(request))) = websocket.next().await else {
                    panic!("tracker did not receive a WebSocket announce")
                };
                let request: serde_json::Value =
                    serde_json::from_str(request.as_ref()).expect("valid announce JSON");
                assert_eq!(request["action"], "announce");
                assert_eq!(request["event"], expected_event);
                assert_eq!(request["port"], 51413);

                websocket
                    .send(Message::Text(
                        serde_json::json!({
                            "interval": 60,
                            "complete": 2,
                            "incomplete": 3,
                            "peers": [{"ip": "192.0.2.20", "port": 6881}]
                        })
                        .to_string(),
                    ))
                    .await
                    .expect("send tracker response");
            }
        });

        let mut announcer =
            TrackerAnnouncer::new(&[vec![format!("ws://{address}/announce")]], &None);
        let options = DownloadOptions {
            bt_tracker_timeout: 2,
            bt_tracker_connect_timeout: 2,
            ..DownloadOptions::default()
        };
        announcer.set_websocket_options(&options);
        announcer.set_tcp_port(51413);

        let result = announcer
            .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
            .await
            .expect("started WebSocket announce should succeed");
        assert_eq!(result.event, AnnounceEvent::Started);
        assert_eq!(result.peers, vec![("192.0.2.20".to_string(), 6881)]);
        assert_eq!(result.interval, Duration::from_secs(60));
        assert_eq!(result.seeders, Some(2));
        assert_eq!(result.leechers, Some(3));

        announcer
            .announce_stopped(&[0u8; 20], &[1u8; 20], 0, 1, 0)
            .await;
        assert!(announcer.stopped_sent);

        server
            .await
            .expect("WebSocket tracker test server should exit");
    }

    #[test]
    fn test_tracker_announcer_creation() {
        let announcer = TrackerAnnouncer::new(&[], &None);
        assert!(!announcer.is_announce_ready());
        assert!(announcer.is_all_announce_failed());
    }

    #[test]
    fn test_tracker_announcer_with_announce_url() {
        let urls = vec![vec!["http://tracker.example.com:6969/announce".to_string()]];
        let announcer = TrackerAnnouncer::new(&urls, &None);
        // Announce should be ready initially (no prev_announce_time)
        assert!(announcer.is_announce_ready());
    }

    #[test]
    fn test_tracker_announcer_udp_detection() {
        let urls = vec![vec!["udp://tracker.example.com:6969/announce".to_string()]];
        let _announcer = TrackerAnnouncer::new(&urls, &None);
        // BtAnnounce should detect the UDP URL
        assert!(is_udp_tracker("udp://tracker.example.com:6969/announce"));
        assert!(!is_udp_tracker("http://tracker.example.com:6969/announce"));
    }

    #[test]
    fn test_announce_result_fields() {
        let result = AnnounceResult {
            peers: vec![("10.0.0.1".to_string(), 6881)],
            interval: Duration::from_secs(300),
            seeders: Some(5),
            leechers: Some(10),
            event: AnnounceEvent::Started,
            tracker_url: "udp://tracker.example.com:6969/announce".to_string(),
        };
        assert_eq!(result.peers.len(), 1);
        assert_eq!(result.seeders, Some(5));
        assert_eq!(result.leechers, Some(10));
    }
}

//! Torrent-owned tracker announce dispatcher and lifecycle coordinator.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use super::super::bt_announce::{BtAnnounce, is_udp_tracker};
use super::super::types::AnnounceEvent;
use crate::engine::bittorrent::tracker::udp_client::{
    UdpAnnounceParams, UdpError, UdpTrackerClient, resolve_udp_tracker_addr,
};
use crate::http::client_identity::ClientTlsConfig;
use crate::network::OutboundNetworkPolicy;
use crate::request::request_group::DownloadOptions;
use aria2_protocol::bittorrent::tracker::public_list::{PublicTrackerList, TrackerFailureKind};

#[path = "catalog.rs"]
mod catalog;
#[path = "http.rs"]
mod http;
#[path = "runtime_snapshot.rs"]
mod runtime_snapshot;
#[path = "stopped.rs"]
mod stopped;
#[path = "udp.rs"]
mod udp;
#[path = "websocket.rs"]
mod websocket;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use runtime_snapshot::TrackerState;
pub use runtime_snapshot::{SharedTrackerRuntime, TrackerRuntimeInfo, TrackerRuntimeSnapshot};

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

/// Unified torrent-scoped tracker lifecycle and announce coordinator.
pub struct TrackerAnnouncer {
    announce: BtAnnounce,
    stopped_sent: bool,
    udp_client: Option<UdpTrackerClient>,
    udp_tracker_endpoint: Option<(String, SocketAddr)>,
    udp_family_ipv6: Option<bool>,
    last_attempt_tracker_url: Option<String>,
    public_tracker_catalog: Option<Arc<PublicTrackerList>>,
    public_tracker_urls: HashSet<String>,
    excluded_tracker_urls: Vec<String>,
    last_failure_kind: Option<TrackerFailureKind>,
    http_tls: ClientTlsConfig,
    outbound_network_policy: Arc<OutboundNetworkPolicy>,
    http_clients: HashMap<Option<std::net::IpAddr>, reqwest::Client>,
    websocket_options: DownloadOptions,
    tracker_timeout_secs: u64,
    tracker_connect_timeout_secs: u64,
    user_defined_interval: Duration,
    force_encryption: bool,
    external_ip: Option<String>,
    stopped_timeout: Duration,
    runtime_state: Option<SharedTrackerRuntime>,
    tracker_states: HashMap<String, TrackerState>,
}

impl TrackerAnnouncer {
    /// Create a new tracker announcer from an announce list and optional single URL.
    pub fn new(announce_list: &[Vec<String>], announce: &Option<String>) -> Self {
        Self {
            announce: BtAnnounce::new(announce_list, announce),
            stopped_sent: false,
            udp_client: None,
            udp_tracker_endpoint: None,
            udp_family_ipv6: None,
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
            user_defined_interval: Duration::ZERO,
            force_encryption: false,
            external_ip: None,
            stopped_timeout: Duration::from_secs(crate::constants::BT_TRACKER_STOPPED_TIMEOUT_SECS),
            runtime_state: None,
            tracker_states: HashMap::new(),
        }
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
        self.user_defined_interval = interval;
        self.announce.set_user_defined_interval(interval);
    }

    /// Apply the announce encryption and external-IP options.
    pub fn set_announce_options(&mut self, force_encryption: bool, external_ip: Option<String>) {
        self.force_encryption = force_encryption;
        self.external_ip.clone_from(&external_ip);
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
    pub fn next_default_announce_delay(&self) -> Option<Duration> {
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

    /// Execute one announce through the selected tracker transport.
    pub async fn announce(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
    ) -> Option<AnnounceResult> {
        if !self.announce.is_announce_ready() || !self.announce.adjust_announce_list() {
            return None;
        }

        let tracker_url = self.announce.announce_list().get_announce()?.to_string();
        self.last_attempt_tracker_url = Some(tracker_url.clone());
        self.last_failure_kind = None;
        self.tracker_attempt_started(&tracker_url);
        self.publish_runtime_snapshot();
        let event = self.announce.announce_list().get_event();
        let result = if is_udp_tracker(&tracker_url) {
            self.announce_udp(info_hash, peer_id, downloaded, left, uploaded, &tracker_url)
                .await
        } else if reqwest::Url::parse(&tracker_url)
            .ok()
            .is_some_and(|url| matches!(url.scheme(), "ws" | "wss"))
        {
            self.announce_websocket(info_hash, peer_id, downloaded, left, uploaded, &tracker_url)
                .await
        } else {
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

    /// Clear in-flight state when the owner cancels an announce future.
    pub fn cancel_pending_announce(&mut self) -> bool {
        let Some(tracker_url) = self.last_attempt_tracker_url.clone() else {
            return false;
        };
        if !self
            .tracker_states
            .get(&tracker_url)
            .is_some_and(|state| state.in_flight)
        {
            return false;
        }

        self.announce.announce_cancelled();
        self.tracker_attempt_finished(&tracker_url, false);
        self.publish_runtime_snapshot();
        true
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

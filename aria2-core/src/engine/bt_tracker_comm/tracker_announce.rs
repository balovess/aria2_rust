//! Unified tracker announce dispatcher integrating BtAnnounce state machine
//! with both HTTP and UDP tracker backends.
//!
//! # C++ Reference
//!
//! In C++ aria2, `DefaultBtAnnounce` creates either an `HttpRequestCommand`
//! or a `UDPTrackerRequest` depending on the tracker URL scheme. The dispatch
//! happens inside `DefaultBtAnnounce::getAnnounceUrl()` (HTTP) and
//! `DefaultBtAnnounce::createUDPTrackerRequest()` (UDP).
//!
//! This module unifies both paths through a single `TrackerAnnouncer` that
//! uses the `BtAnnounce` state machine to decide *when* and *what* to
//! announce, then routes to the correct backend based on URL scheme.

use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use super::bt_announce::{BtAnnounce, is_udp_tracker};
use super::types::AnnounceEvent;
use crate::engine::udp_tracker_client::SharedUdpClient;
use crate::engine::udp_tracker_manager::UdpTrackerManager;

/// Result of a tracker announce operation (HTTP or UDP).
#[derive(Debug, Clone)]
pub struct AnnounceResult {
    /// Peer addresses discovered from the tracker.
    pub peers: Vec<(String, u16)>,
    /// Recommended announce interval from the tracker response.
    pub interval: Duration,
    /// Number of seeders reported by the tracker.
    pub seeders: i64,
    /// Number of leechers reported by the tracker.
    pub leechers: i64,
    /// The announce event that was sent.
    pub event: AnnounceEvent,
    /// The tracker URL that was used for this announce.
    pub tracker_url: String,
}

/// Unified tracker announcer that dispatches HTTP and UDP tracker announces
/// through the `BtAnnounce` state machine.
///
/// This replaces the ad-hoc UDP tracker usage in `discover_peers()` with
/// a proper state-machine-driven approach that:
/// - Uses `BtAnnounce::adjust_announce_list()` to determine event and timing
/// - Routes HTTP URLs through the existing HTTP announce path
/// - Routes UDP URLs through `UdpTrackerManager`
/// - Processes responses through `BtAnnounce::process_*_response()`
/// - Tracks announce success/failure for tier rotation
pub struct TrackerAnnouncer {
    /// The core announce state machine.
    announce: BtAnnounce,
    /// UDP tracker manager (created lazily when first UDP URL is seen).
    udp_manager: Option<UdpTrackerManager>,
    /// Shared UDP client for the UDP tracker manager.
    udp_client: Option<SharedUdpClient>,
}

impl TrackerAnnouncer {
    /// Create a new tracker announcer from an announce list and optional single URL.
    pub fn new(announce_list: &[Vec<String>], announce: &Option<String>) -> Self {
        Self {
            announce: BtAnnounce::new(announce_list, announce),
            udp_manager: None,
            udp_client: None,
        }
    }

    /// Create from an existing `BtAnnounce` and optional shared UDP client.
    pub fn with_udp_client(announce: BtAnnounce, udp_client: Option<SharedUdpClient>) -> Self {
        Self {
            announce,
            udp_manager: None,
            udp_client,
        }
    }

    /// Set the shared UDP client for UDP tracker announces.
    pub fn set_udp_client(&mut self, client: SharedUdpClient) {
        self.udp_client = Some(client);
    }

    /// Returns true if any announce is ready (stopped, completed, or periodic).
    pub fn is_announce_ready(&self) -> bool {
        self.announce.is_announce_ready()
    }

    /// Returns true if a periodic announce is ready.
    pub fn is_default_announce_ready(&self) -> bool {
        self.announce.is_default_announce_ready()
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

        // Adjust the announce list (sets event, moves to appropriate tier)
        if !self.announce.adjust_announce_list() {
            return None;
        }

        // Get the current tracker URL and event, cloning to avoid borrow conflicts
        let tracker_url = self.announce.announce_list().get_announce()?.to_string();
        let event = self.announce.announce_list().get_event();
        let is_udp = is_udp_tracker(&tracker_url);

        // Determine if this is a UDP or HTTP tracker
        if is_udp {
            self.announce_udp(
                info_hash,
                peer_id,
                downloaded,
                left,
                uploaded,
                event,
                &tracker_url,
            )
            .await
        } else {
            // HTTP announce — build URL and dispatch
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
        }
    }

    /// Execute a UDP tracker announce.
    async fn announce_udp(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
        event: AnnounceEvent,
        tracker_url: &str, // must be &str for lifetime flexibility
    ) -> Option<AnnounceResult> {
        // Initialize UDP manager lazily
        if self.udp_manager.is_none() {
            if let Some(ref client) = self.udp_client {
                let mgr = UdpTrackerManager::new(Arc::clone(client)).await;
                self.udp_manager = Some(mgr);
            } else {
                // No UDP client configured — try to create one
                match crate::engine::udp_tracker_client::UdpTrackerClient::new(0).await {
                    Ok(client) => {
                        let shared = Arc::new(tokio::sync::Mutex::new(client));
                        self.udp_client = Some(Arc::clone(&shared));
                        let mgr = UdpTrackerManager::new(shared).await;
                        self.udp_manager = Some(mgr);
                    }
                    Err(e) => {
                        warn!("[BT] Failed to create UDP tracker client: {}", e);
                        self.announce.announce_failure();
                        return None;
                    }
                }
            }
        }

        let mgr = self.udp_manager.as_mut()?;

        // Parse the tracker URL into an endpoint if not already tracked
        if mgr.endpoint_count() == 0 {
            let urls = vec![tracker_url.to_string()];
            mgr.parse_tracker_urls(&urls);
        }

        // Signal announce start
        self.announce.announce_start();

        // Convert event to UDP event
        let udp_event = self.announce.current_udp_event();

        // Determine numwant
        let numwant = if self.announce.announce_list().get_event() == AnnounceEvent::Stopped {
            0
        } else {
            50
        };

        debug!(
            "[BT] Announcing to UDP tracker {} (event={:?}, udp_event={})",
            tracker_url, event, udp_event
        );

        let responses = mgr
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
            self.announce.announce_failure();
            return None;
        }

        // Process the first response through the state machine
        let response = &responses[0];
        let peers = self.announce.process_udp_announce_response(response);

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
        let url = self
            .announce
            .get_announce_url(info_hash, peer_id, uploaded, downloaded, left, None)?;

        // Signal announce start
        self.announce.announce_start();

        debug!(
            "[BT] Announcing to HTTP tracker {} (event={:?})",
            tracker_url, event
        );

        // Send HTTP request
        match crate::engine::http_tracker_client::build_tracker_client(5) {
            Ok(client) => {
                match client.get(&url).send().await {
                    Ok(resp) => {
                        if !resp.status().is_success() {
                            warn!(
                                "[BT] HTTP tracker {} returned status {}",
                                tracker_url,
                                resp.status()
                            );
                            self.announce.announce_failure();
                            return None;
                        }

                        match resp.bytes().await {
                            Ok(body) => {
                                match aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(&body) {
                                    Ok(tracker_resp) => {
                                        if tracker_resp.is_failure() {
                                            let reason = tracker_resp.failure_reason
                                                .unwrap_or_else(|| "tracker failure".to_string());
                                            warn!("[BT] HTTP tracker {} failure: {}", tracker_url, reason);
                                            self.announce.announce_failure();
                                            return None;
                                        }

                                        // Process through BtAnnounce state machine
                                        match self.announce.process_announce_response(&tracker_resp) {
                                            Ok(peers) => {
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
                                        self.announce.announce_failure();
                                        None
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("[BT] HTTP tracker {} body read failed: {}", tracker_url, e);
                                self.announce.announce_failure();
                                None
                            }
                        }
                    }
                    Err(e) => {
                        warn!("[BT] HTTP tracker {} request failed: {}", tracker_url, e);
                        self.announce.announce_failure();
                        None
                    }
                }
            }
            Err(e) => {
                warn!(
                    "[BT] Failed to build HTTP tracker client for {}: {}",
                    tracker_url, e
                );
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
        self.announce.set_runtime_halted(true);

        // Try to send stopped event to all applicable tiers
        let mut attempts = 0;
        const MAX_STOPPED_ATTEMPTS: usize = 5;

        while self.announce.is_stopped_announce_ready() && attempts < MAX_STOPPED_ATTEMPTS {
            if let Some(result) = self
                .announce(info_hash, peer_id, downloaded, left, uploaded)
                .await
            {
                info!(
                    "[BT] Sent stopped event to {} ({} peers in response)",
                    result.tracker_url,
                    result.peers.len()
                );
            }
            attempts += 1;
        }
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

        if let Some(result) = self
            .announce(info_hash, peer_id, downloaded, 0, uploaded)
            .await
        {
            info!(
                "[BT] Sent completed event to {} ({} seeders, {} leechers)",
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
    }

    /// Set whether the download has fewer than minimum peers.
    pub fn set_less_than_min_peers(&mut self, less: bool) {
        self.announce.set_less_than_min_peers(less);
    }

    /// Set the TCP port for announce URL construction.
    pub fn set_tcp_port(&mut self, port: u16) {
        self.announce.set_tcp_port(port);
    }

    /// Set whether the download is complete.
    pub fn set_download_complete(&mut self, complete: bool) {
        self.announce.set_download_complete(complete);
    }

    /// Set whether the runtime is halted (stopping).
    pub fn set_runtime_halted(&mut self, halted: bool) {
        self.announce.set_runtime_halted(halted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            seeders: 5,
            leechers: 10,
            event: AnnounceEvent::Started,
            tracker_url: "udp://tracker.example.com:6969/announce".to_string(),
        };
        assert_eq!(result.peers.len(), 1);
        assert_eq!(result.seeders, 5);
        assert_eq!(result.leechers, 10);
    }
}

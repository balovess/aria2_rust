//! Main tracker announce client and HTTP tracker communication functions.
//!
//! Contains [`BtAnnounce`] (the core announce orchestrator), the public
//! helper [`urlencode_infohash`], and free functions for HTTP/HTTPS tracker
//! announce requests.

#![allow(clippy::empty_line_after_doc_comments)]

use super::announce_list::AnnounceList;
use super::types::AnnounceEvent;
use crate::engine::http_tracker_client::{TrackerEvent, build_tracker_client, is_https_tracker};
use crate::error::{Aria2Error, RecoverableError, Result};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Tracker request timeout (seconds)
const TRACKER_REQUEST_TIMEOUT_SECS: u64 = 5;

/// Default announce interval (2 minutes, matching C++ DEFAULT_ANNOUNCE_INTERVAL)
const DEFAULT_ANNOUNCE_INTERVAL_SECS: u64 = 120;

/// Default number of peers to request from tracker
const DEFAULT_NUMWANT: u32 = 50;

// ======================================================================
// URL Encoding Helpers
// ======================================================================

/// URL-encodes a 20-byte info hash or peer ID for use in tracker URLs.
///
/// Each byte is encoded as `%XX` where XX is the uppercase hex representation.
/// This is required by the BitTorrent tracker protocol specification.
pub fn urlencode_infohash(hash: &[u8; 20]) -> String {
    hash.iter().map(|b| format!("%{:02X}", b)).collect()
}

/// URL-encodes an arbitrary byte slice for use in tracker URLs.
pub(crate) fn urlencode_bytes(data: &[u8]) -> String {
    data.iter().map(|b| format!("%{:02X}", b)).collect()
}

// ======================================================================
// BtAnnounce (from C++ DefaultBtAnnounce)
// ======================================================================

/// Main announce orchestrator matching C++ DefaultBtAnnounce behavior.
///
/// Manages the tracker announce lifecycle including:
/// - When to announce (interval-based timing)
/// - What event to send (started, completed, stopped, regular)
/// - How to handle success/failure (tier rotation, event state machine)
/// - Processing tracker responses (interval, peer counts, tracker ID)
pub struct BtAnnounce {
    /// Number of in-flight announce requests
    trackers: u32,
    /// Time of last successful announce
    pub(crate) prev_announce_time: Option<Instant>,
    /// Interval from tracker response (seconds)
    pub(crate) interval: Duration,
    /// Minimum interval from tracker response (seconds)
    pub(crate) min_interval: Duration,
    /// User-defined interval override (0 = use tracker interval)
    user_defined_interval: Duration,
    /// Number of complete seeders from tracker
    complete: i64,
    /// Number of incomplete leechers from tracker
    incomplete: i64,
    /// Tracker ID from tracker response (sent back in subsequent announces)
    tracker_id: String,
    /// The announce list with tier management
    announce_list: AnnounceList,
    /// Whether download is complete (all pieces downloaded)
    download_complete: bool,
    /// Whether the runtime is halted (stopping)
    runtime_halted: bool,
    /// Whether we have fewer than minimum peers
    less_than_min_peers: bool,
    /// TCP port for announce
    tcp_port: u16,
}

impl BtAnnounce {
    /// Create a new BtAnnounce from an announce list and optional single announce URL.
    pub fn new(announce_list: &[Vec<String>], announce: &Option<String>) -> Self {
        Self {
            trackers: 0,
            prev_announce_time: None,
            interval: Duration::from_secs(DEFAULT_ANNOUNCE_INTERVAL_SECS),
            min_interval: Duration::from_secs(DEFAULT_ANNOUNCE_INTERVAL_SECS),
            user_defined_interval: Duration::ZERO,
            complete: 0,
            incomplete: 0,
            tracker_id: String::new(),
            announce_list: AnnounceList::new(announce_list, announce),
            download_complete: false,
            runtime_halted: false,
            less_than_min_peers: true,
            tcp_port: 0,
        }
    }

    /// Returns true if a default (periodic) announce is ready.
    ///
    /// Conditions (matching C++ isDefaultAnnounceReady):
    /// - No in-flight announce requests
    /// - Interval has elapsed since last announce
    /// - Not all tiers have failed
    pub fn is_default_announce_ready(&self) -> bool {
        if self.trackers != 0 {
            return false;
        }

        let effective_interval = if self.user_defined_interval > Duration::ZERO {
            self.user_defined_interval
        } else {
            self.min_interval
        };

        let elapsed = match self.prev_announce_time {
            Some(t) => t.elapsed(),
            None => return !self.announce_list.all_tiers_failed(),
        };

        elapsed >= effective_interval && !self.announce_list.all_tiers_failed()
    }

    /// Returns true if a "stopped" announce is ready.
    ///
    /// Conditions (matching C++ isStoppedAnnounceReady):
    /// - No in-flight requests
    /// - Runtime is halted
    /// - At least one tier accepts the stopped event
    pub fn is_stopped_announce_ready(&self) -> bool {
        self.trackers == 0
            && self.runtime_halted
            && self.announce_list.count_stopped_allowed_tier() > 0
    }

    /// Returns true if a "completed" announce is ready.
    ///
    /// Conditions (matching C++ isCompletedAnnounceReady):
    /// - No in-flight requests
    /// - Download is complete
    /// - At least one tier accepts the completed event
    pub fn is_completed_announce_ready(&self) -> bool {
        self.trackers == 0
            && self.download_complete
            && self.announce_list.count_completed_allowed_tier() > 0
    }

    /// Returns true if any announce is ready (matching C++ isAnnounceReady).
    ///
    /// Priority order: stopped > completed > default
    pub fn is_announce_ready(&self) -> bool {
        self.is_stopped_announce_ready()
            || self.is_completed_announce_ready()
            || self.is_default_announce_ready()
    }

    /// Adjust the announce list for the next announce (matching C++ adjustAnnounceList).
    ///
    /// This is the core state machine logic:
    /// - If stopped ready: move to stopped-allowed tier, set STOPPED event
    /// - If completed ready: move to completed-allowed tier, set COMPLETED event
    /// - If default ready and download complete and event is STARTED, set STARTED_AFTER_COMPLETION
    ///
    /// Returns true if an announce should be made, false otherwise.
    pub fn adjust_announce_list(&mut self) -> bool {
        if self.is_stopped_announce_ready() {
            if !self.announce_list.current_tier_accepts_stopped_event() {
                self.announce_list.move_to_stopped_allowed_tier();
            }
            self.announce_list.set_event(AnnounceEvent::Stopped);
        } else if self.is_completed_announce_ready() {
            if !self.announce_list.current_tier_accepts_completed_event() {
                self.announce_list.move_to_completed_allowed_tier();
            }
            self.announce_list.set_event(AnnounceEvent::Completed);
        } else if self.is_default_announce_ready() {
            // If download completed before "started" event is sent to a tracker,
            // we change the event to STARTED_AFTER_COMPLETION to prevent sending
            // a "completed" event later (which would be incorrect since we
            // already had all pieces when we started).
            if self.download_complete
                && self.announce_list.get_event() == AnnounceEvent::Started
            {
                self.announce_list.set_event(AnnounceEvent::StartedAfterCompletion);
            }
        } else {
            return false;
        }
        true
    }

    /// Build the full announce URL with all required parameters
    /// (matching C++ getAnnounceUrl).
    ///
    /// # Arguments
    /// * `info_hash` - 20-byte SHA-1 hash of the torrent info dictionary
    /// * `peer_id` - 20-byte unique identifier for this client
    /// * `uploaded` - Total bytes uploaded in this session
    /// * `downloaded` - Total bytes downloaded in this session
    /// * `left` - Bytes remaining to download
    /// * `key` - Optional key bytes (last 8 bytes of peer ID if not provided)
    #[allow(clippy::too_many_arguments)]
    pub fn get_announce_url(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        uploaded: u64,
        downloaded: u64,
        left: u64,
        key: Option<&[u8]>,
    ) -> Option<String> {
        if !self.adjust_announce_list() {
            return None;
        }

        let base_url = self.announce_list.get_announce()?;
        let separator = if base_url.contains('?') { "&" } else { "?" };

        // Use last 8 bytes of peer ID as key if not explicitly provided
        let key_bytes = key.unwrap_or(&peer_id[12..20]);

        // numwant: 50 if we need peers, 0 if we have enough or are halting
        let numwant = if self.less_than_min_peers && !self.runtime_halted {
            DEFAULT_NUMWANT
        } else {
            0
        };

        let mut url = format!(
            "{}{}info_hash={}&peer_id={}&uploaded={}&downloaded={}&left={}&compact=1&key={}&numwant={}&no_peer_id=1",
            base_url,
            separator,
            urlencode_infohash(info_hash),
            urlencode_infohash(peer_id),
            uploaded,
            downloaded,
            left,
            urlencode_bytes(key_bytes),
            numwant,
        );

        if self.tcp_port != 0 {
            url.push_str(&format!("&port={}", self.tcp_port));
        }

        let event_str = self.announce_list.get_event_string();
        if !event_str.is_empty() {
            url.push_str("&event=");
            url.push_str(event_str);
        }

        if !self.tracker_id.is_empty() {
            url.push_str("&trackerid=");
            url.push_str(&urlencode_bytes(self.tracker_id.as_bytes()));
        }

        // TODO: Add supportcrypto=1 / requirecrypto=1 based on config
        url.push_str("&supportcrypto=1");

        Some(url)
    }

    /// Signal that an announce request has been sent (matching C++ announceStart).
    pub fn announce_start(&mut self) {
        self.trackers += 1;
    }

    /// Handle successful announce (matching C++ announceSuccess).
    ///
    /// - Resets in-flight counter to 0
    /// - Calls announceList.announceSuccess() (moves URL to front, resets to first tier)
    /// - Updates prev_announce_time
    pub fn announce_success(&mut self) {
        self.trackers = 0;
        self.announce_list.announce_success();
        self.prev_announce_time = Some(Instant::now());
    }

    /// Handle failed announce (matching C++ announceFailure).
    ///
    /// - Resets in-flight counter to 0
    /// - Calls announceList.announceFailure() (advances to next tracker/tier)
    pub fn announce_failure(&mut self) {
        self.trackers = 0;
        self.announce_list.announce_failure();
    }

    /// Returns true if all announce attempts have failed (matching C++ isAllAnnounceFailed).
    pub fn is_all_announce_failed(&self) -> bool {
        self.announce_list.all_tiers_failed()
    }

    /// Reset announce state (matching C++ resetAnnounce).
    ///
    /// - Updates prev_announce_time to now
    /// - Resets the announce list iterator to the beginning
    pub fn reset_announce(&mut self) {
        self.prev_announce_time = Some(Instant::now());
        self.announce_list.reset_tier();
    }

    /// Process a tracker response and update internal state
    /// (matching C++ processAnnounceResponse).
    ///
    /// Returns peer addresses on success, or an error string on failure.
    pub fn process_announce_response(
        &mut self,
        response: &aria2_protocol::bittorrent::tracker::response::TrackerResponse,
    ) -> std::result::Result<Vec<(String, u16)>, String> {
        // Check for failure reason
        if let Some(ref reason) = response.failure_reason {
            return Err(reason.clone());
        }

        // Log warning if present
        if let Some(ref msg) = response.warning_message {
            warn!("[BT] Tracker warning: {}", msg);
        }

        // Store tracker_id from response for subsequent announces
        // (matching C++ BtAnnounce::processAnnounceResponse)
        if let Some(ref tid) = response.tracker_id {
            if !tid.is_empty() {
                debug!("[BT] Tracker ID: {}", tid);
                self.tracker_id = tid.clone();
            }
        }

        // Update interval
        let interval_secs = response.interval;
        if interval_secs > 0 {
            self.interval = Duration::from_secs(interval_secs as u64);
            debug!("[BT] Announce interval: {}s", interval_secs);
        }

        // Update min_interval (capped at interval)
        if let Some(min_iv) = response.min_interval {
            if min_iv > 0 {
                let min_dur = Duration::from_secs(min_iv as u64);
                self.min_interval = min_dur.min(self.interval);
                debug!("[BT] Min interval: {}s", min_iv);
            }
        } else {
            // Use interval as minInterval if minInterval is not supplied (matching C++)
            self.min_interval = self.interval;
        }

        // Update complete/incomplete counts
        self.complete = response.seeders as i64;
        self.incomplete = response.leechers as i64;
        debug!(
            "[BT] Tracker stats: complete={}, incomplete={}",
            self.complete, self.incomplete
        );

        // Extract peer addresses
        let peers = response
            .peers
            .iter()
            .map(|p| (p.ip.clone(), p.port))
            .collect();

        Ok(peers)
    }

    /// Returns true if no more announces are needed
    /// (matching C++ noMoreAnnounce).
    ///
    /// This means: no in-flight requests, runtime is halted,
    /// and no tiers accept the stopped event.
    pub fn no_more_announce(&self) -> bool {
        self.trackers == 0
            && self.runtime_halted
            && self.announce_list.count_stopped_allowed_tier() == 0
    }

    /// Shuffle all URLs in each tier (matching C++ shuffleAnnounce).
    pub fn shuffle_announce(&mut self) {
        self.announce_list.shuffle();
    }

    /// Override the minimum interval (matching C++ overrideMinInterval).
    pub fn override_min_interval(&mut self, interval: Duration) {
        self.min_interval = interval;
    }

    /// Set the TCP port for announce URL (matching C++ setTcpPort).
    pub fn set_tcp_port(&mut self, port: u16) {
        self.tcp_port = port;
    }

    /// Set whether the download is complete.
    pub fn set_download_complete(&mut self, complete: bool) {
        self.download_complete = complete;
    }

    /// Set whether the runtime is halted (stopping).
    pub fn set_runtime_halted(&mut self, halted: bool) {
        self.runtime_halted = halted;
    }

    /// Set whether we have fewer than minimum peers.
    pub fn set_less_than_min_peers(&mut self, less: bool) {
        self.less_than_min_peers = less;
    }

    /// Set user-defined announce interval override.
    /// Duration::ZERO means use tracker's interval.
    pub fn set_user_defined_interval(&mut self, interval: Duration) {
        self.user_defined_interval = interval;
    }

    /// Get the current announce interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Get the current minimum interval.
    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }

    /// Get the number of complete seeders.
    pub fn complete(&self) -> i64 {
        self.complete
    }

    /// Get the number of incomplete leechers.
    pub fn incomplete(&self) -> i64 {
        self.incomplete
    }

    /// Get the tracker ID from the last response.
    pub fn tracker_id(&self) -> &str {
        &self.tracker_id
    }

    /// Get a reference to the announce list.
    pub fn announce_list(&self) -> &AnnounceList {
        &self.announce_list
    }

    /// Get a mutable reference to the announce list.
    pub fn announce_list_mut(&mut self) -> &mut AnnounceList {
        &mut self.announce_list
    }
}

// ======================================================================
// HTTP Tracker Communication (Free Functions)
// ======================================================================

/// Announce to a public tracker and collect peer addresses.
///
/// Sends an HTTP/HTTPS GET request to the tracker with standard announce parameters
/// and parses the response to extract peer information.
///
/// This function automatically detects HTTPS URLs and uses TLS when required.
/// reqwest supports HTTPS natively via its default features (native-tls).
///
/// # Arguments
/// * `tracker_url` - The announce URL of the public tracker (http:// or https://)
/// * `info_hash` - 20-byte SHA-1 hash of the torrent's info dictionary
/// * `peer_id` - 20-byte unique identifier for this client
/// * `total_size` - Total size of the torrent content in bytes
///
/// # Returns
/// A vector of `(ip_address, port)` tuples on success.
///
/// # Errors
/// Returns error string if HTTP request fails, response parsing fails,
/// or tracker reports failure.
pub async fn announce_to_public_tracker(
    tracker_url: &str,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    total_size: u64,
) -> std::result::Result<Vec<(String, u16)>, String> {
    announce_to_public_tracker_with_event(
        tracker_url,
        info_hash,
        peer_id,
        total_size,
        TrackerEvent::Started, // Default event type
    )
    .await
}

/// Announce to a public tracker with explicit event control.
///
/// Extended version of [`announce_to_public_tracker`] that accepts a specific
/// [`TrackerEvent`] for state machine integration.
///
/// # Arguments
/// * `tracker_url` - The announce URL of the public tracker
/// * `info_hash` - 20-byte SHA-1 hash of the torrent's info dictionary
/// * `peer_id` - 20-byte unique identifier for this client
/// * `total_size` - Total size of the torrent content in bytes
/// * `event` - The tracker event to send
pub async fn announce_to_public_tracker_with_event(
    tracker_url: &str,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    total_size: u64,
    event: TrackerEvent,
) -> std::result::Result<Vec<(String, u16)>, String> {
    // Detect HTTPS scheme for logging and configuration purposes
    let is_https = is_https_tracker(tracker_url);
    if is_https {
        debug!("HTTPS tracker detected: {} (using native-tls)", tracker_url);
    }

    let event_param = if event == TrackerEvent::None {
        String::new()
    } else {
        format!("&event={}", event.as_str())
    };

    let url = format!(
        "{}?info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left={}{}&compact=1",
        tracker_url,
        urlencode_infohash(info_hash),
        urlencode_infohash(peer_id),
        total_size,
        event_param,
    );

    let client = build_tracker_client(TRACKER_REQUEST_TIMEOUT_SECS)
        .map_err(|e| format!("build client: {}", e))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("request failed: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("read body: {}", e))?;

    let tracker_resp = aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(&body)
        .map_err(|e| format!("parse response: {}", e))?;

    if tracker_resp.is_failure() {
        return Err(tracker_resp
            .failure_reason
            .unwrap_or_else(|| "tracker failure".to_string()));
    }

    Ok(tracker_resp
        .peers
        .into_iter()
        .map(|p| (p.ip, p.port))
        .collect())
}

// ======================================================================
// Tracker Peer Discovery Functions
// ======================================================================

/// Perform initial HTTP tracker announce and collect peers.
///
/// This is the first step in peer discovery after torrent metadata is parsed.
/// Sends a "started" event to inform the tracker we're beginning download.
///
/// Automatically detects HTTPS URLs and uses TLS when required.
///
/// # Arguments
/// * `announce_url` - The primary tracker announce URL from torrent metadata
/// * `info_hash_raw` - Raw 20-byte info hash
/// * `my_peer_id` - Our 20-byte peer ID
/// * `total_size` - Total torrent size in bytes
///
/// # Returns
/// Vector of peer addresses from the tracker response.
///
/// # Errors
/// Returns error if HTTP request fails, response parsing fails,
/// or tracker indicates failure.
pub async fn perform_http_tracker_announce(
    announce_url: &str,
    info_hash_raw: &[u8; 20],
    my_peer_id: &[u8; 20],
    total_size: u64,
) -> Result<Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>> {
    // Detect HTTPS for logging
    let is_https = is_https_tracker(announce_url);
    if is_https {
        debug!("[BT] HTTPS tracker detected for announce: {}", announce_url);
    }

    let url = format!(
        "{}?info_hash={}&peer_id={}&port=6881&uploaded=0&downloaded=0&left={}&event=started&compact=1",
        announce_url,
        urlencode_infohash(info_hash_raw),
        urlencode_infohash(my_peer_id),
        total_size,
    );

    info!("[BT] Announcing to tracker: {}", url);
    let client = build_tracker_client(TRACKER_REQUEST_TIMEOUT_SECS).map_err(|e| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Failed to build tracker client: {}", e),
        })
    })?;

    let resp = client.get(&url).send().await.map_err(|e| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Tracker HTTP failed: {}", e),
        })
    })?;
    info!("[BT] Tracker response status: {}", resp.status());
    let body = resp.bytes().await.map_err(|e| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Tracker body read failed: {}", e),
        })
    })?;
    debug!("[BT] Tracker body: {:?}", String::from_utf8_lossy(&body));

    let tracker_resp = aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(&body)
        .map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Tracker parse failed: {}", e),
            })
        })?;

    info!("[BT] Tracker response: {} peers", tracker_resp.peer_count());
    for peer in &tracker_resp.peers {
        debug!("[BT]   Peer: {}:{}", peer.ip, peer.port);
    }

    if tracker_resp.is_failure() {
        return Err(Aria2Error::Recoverable(
            RecoverableError::TemporaryNetworkFailure {
                message: tracker_resp.failure_reason.unwrap_or_default(),
            },
        ));
    }

    Ok(tracker_resp
        .peers
        .iter()
        .map(|p| aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&p.ip, p.port))
        .collect())
}

/// Perform an announce with a specific tracker event (for state machine integration).
///
/// Use this for sending Completed and Stopped events at appropriate lifecycle points.
pub async fn perform_announce_with_event(
    announce_url: &str,
    info_hash_raw: &[u8; 20],
    my_peer_id: &[u8; 20],
    downloaded: u64,
    left: u64,
    uploaded: u64,
    event: TrackerEvent,
) -> Result<()> {
    let is_https = is_https_tracker(announce_url);

    let event_str = event.as_str();
    let event_param = if event_str.is_empty() {
        String::new()
    } else {
        format!("&event={}", event_str)
    };

    let url = format!(
        "{}?info_hash={}&peer_id={}&port=6881&uploaded={}&downloaded={}&left={}&{}compact=1",
        announce_url,
        urlencode_infohash(info_hash_raw),
        urlencode_infohash(my_peer_id),
        uploaded,
        downloaded,
        left,
        event_param,
    );

    info!(
        "[BT] Announce to {} (event={}, https={})",
        announce_url, event_str, is_https
    );

    let client = build_tracker_client(TRACKER_REQUEST_TIMEOUT_SECS).map_err(|e| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Failed to build tracker client: {}", e),
        })
    })?;

    let resp = client.get(&url).send().await.map_err(|e| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Tracker HTTP failed: {}", e),
        })
    })?;

    let body = resp.bytes().await.map_err(|e| {
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: format!("Tracker body read failed: {}", e),
        })
    })?;

    let tracker_resp = aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(&body)
        .map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("Tracker parse failed: {}", e),
            })
        })?;

    if tracker_resp.is_failure() {
        return Err(Aria2Error::Recoverable(
            RecoverableError::TemporaryNetworkFailure {
                message: tracker_resp.failure_reason.unwrap_or_default(),
            },
        ));
    }

    info!("[BT] Announce success (event={})", event_str);
    Ok(())
}

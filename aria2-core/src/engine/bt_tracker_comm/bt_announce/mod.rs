//! Main tracker announce client and HTTP/UDP tracker communication functions.
//!
//! Contains [`BtAnnounce`] (the core announce orchestrator), the public
//! helper [`urlencode_infohash`], and free functions for HTTP/HTTPS tracker
//! announce requests.
//!
//! # UDP Tracker Integration
//!
//! The [`BtAnnounce`] state machine supports both HTTP and UDP tracker URLs.
//! When a `udp://` URL is encountered, the announce should be routed through
//! [`crate::engine::udp_tracker_manager::UdpTrackerManager`] instead of the
//! HTTP path. The helper [`is_udp_tracker`] can be used to detect UDP URLs.

#![allow(clippy::empty_line_after_doc_comments)]

mod announce_logic;
mod tracker_url;

#[cfg(test)]
mod tests;

// Re-exports — preserve the original public API surface.
pub use announce_logic::{
    announce_to_public_tracker, announce_to_public_tracker_with_event, perform_announce_with_event,
};
pub(crate) use tracker_url::urlencode_bytes;
pub use tracker_url::{is_udp_tracker, urlencode_infohash};

use super::announce_list::AnnounceList;
use super::types::AnnounceEvent;

use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Default announce interval (2 minutes, matching C++ DEFAULT_ANNOUNCE_INTERVAL)
const DEFAULT_ANNOUNCE_INTERVAL_SECS: u64 = 120;

/// Default number of peers to request from tracker
const DEFAULT_NUMWANT: u32 = 50;

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
    /// Whether to require encryption (PREF_BT_FORCE_ENCRYPTION or PREF_BT_REQUIRE_CRYPTO).
    /// When true, appends `&requirecrypto=1` to announce URL.
    /// When false, appends `&supportcrypto=1` (we support but don't require).
    force_encryption: bool,
    /// External IP address to report in announce URL (PREF_BT_EXTERNAL_IP).
    /// When set, appends `&ip=<addr>` to the announce URL.
    external_ip: Option<String>,
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
            force_encryption: false,
            external_ip: None,
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
            if self.download_complete && self.announce_list.get_event() == AnnounceEvent::Started {
                self.announce_list
                    .set_event(AnnounceEvent::StartedAfterCompletion);
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
        self.get_announce_url_without_adjustment(
            info_hash, peer_id, uploaded, downloaded, left, key,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn get_announce_url_without_adjustment(
        &self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        uploaded: u64,
        downloaded: u64,
        left: u64,
        key: Option<&[u8]>,
    ) -> Option<String> {
        let base_url = self.announce_list.get_announce()?;
        let separator = if base_url.contains('?') { "&" } else { "?" };

        // Use last 8 bytes of peer ID as key if not explicitly provided
        let key_bytes = key.unwrap_or(&peer_id[12..20]);

        let numwant = self.numwant();

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

        // C++: if(PREF_BT_FORCE_ENCRYPTION || PREF_BT_REQUIRE_CRYPTO)
        //   append "&requirecrypto=1", else append "&supportcrypto=1"
        if self.force_encryption {
            url.push_str("&requirecrypto=1");
        } else {
            url.push_str("&supportcrypto=1");
        }

        // C++: if(PREF_BT_EXTERNAL_IP is set) append "&ip=<addr>"
        if let Some(ref ip) = self.external_ip {
            url.push_str("&ip=");
            url.push_str(ip);
        }

        Some(url)
    }

    pub fn numwant(&self) -> u32 {
        if self.less_than_min_peers && !self.runtime_halted {
            DEFAULT_NUMWANT
        } else {
            0
        }
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
        if let Some(ref tid) = response.tracker_id
            && !tid.is_empty()
        {
            debug!("[BT] Tracker ID: {}", tid);
            self.tracker_id = tid.clone();
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

        // Extract peer addresses (both IPv4 and IPv6).
        // Matches C++ DefaultBtAnnounce::processAnnounceResponse which processes
        // BtAnnounce::PEERS (AF_INET) and BtAnnounce::PEERS6 (AF_INET6) separately
        // but adds both to peer storage.
        let mut peers: Vec<(String, u16)> = response
            .peers
            .iter()
            .map(|p| (p.ip.clone(), p.port))
            .collect();

        peers.extend(response.peers6.iter().map(|p| (p.ip.clone(), p.port)));

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

    /// Return the TCP port currently included in announce requests.
    pub fn tcp_port(&self) -> u16 {
        self.tcp_port
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

    /// Set whether to require encryption for peer connections.
    ///
    /// Maps to C++ `PREF_BT_FORCE_ENCRYPTION` and `PREF_BT_REQUIRE_CRYPTO`.
    /// When true, `&requirecrypto=1` is appended to announce URLs instead
    /// of `&supportcrypto=1`.
    pub fn set_force_encryption(&mut self, force: bool) {
        self.force_encryption = force;
    }

    /// Set the external IP address to report in announce URLs.
    ///
    /// Maps to C++ `PREF_BT_EXTERNAL_IP`. When set, `&ip=<addr>` is
    /// appended to announce URLs so the tracker can report our
    /// external address to other peers.
    pub fn set_external_ip(&mut self, ip: Option<String>) {
        self.external_ip = ip;
    }

    // ==================================================================
    // UDP Tracker Integration
    // ==================================================================

    /// Returns `true` if the current tracker URL is a UDP tracker.
    ///
    /// Call this after `adjust_announce_list()` to determine whether
    /// to route the announce through `UdpTrackerManager` or the HTTP path.
    ///
    /// # C++ Reference
    ///
    /// C++ aria2 dispatches to `UdpTrackerRequest` based on the URL scheme
    /// during `BtAnnounce::announce()`.
    pub fn is_current_tracker_udp(&self) -> bool {
        self.announce_list
            .get_announce()
            .map(is_udp_tracker)
            .unwrap_or(false)
    }

    /// Convert the current announce event to a UDP tracker event.
    ///
    /// Maps the tracker state machine event to the appropriate UDP protocol
    /// event for use with `UdpTrackerManager::announce()`.
    ///
    /// # C++ Reference
    ///
    /// C++ `UdpTrackerRequest` uses the same event values:
    /// - 0 = none, 1 = completed, 2 = started, 3 = stopped
    pub fn current_udp_event(
        &self,
    ) -> aria2_protocol::bittorrent::tracker::udp_tracker_protocol::UdpEvent {
        use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::UdpEvent;
        match self.announce_list.get_event() {
            AnnounceEvent::Started | AnnounceEvent::StartedAfterCompletion => UdpEvent::Started,
            AnnounceEvent::Completed => UdpEvent::Completed,
            AnnounceEvent::Stopped => UdpEvent::Stopped,
            // Downloading, Seeding, Halted are periodic announces with no event
            AnnounceEvent::Downloading | AnnounceEvent::Seeding | AnnounceEvent::Halted => {
                UdpEvent::None
            }
        }
    }

    /// Process a UDP tracker announce response and update internal state.
    ///
    /// This is the UDP equivalent of `process_announce_response()`. It
    /// updates the interval, seeder/leecher counts, and announces success.
    ///
    /// Returns peer addresses on success.
    pub fn process_udp_announce_response(
        &mut self,
        response: &aria2_protocol::bittorrent::tracker::udp_tracker_protocol::AnnounceResponse,
    ) -> Vec<(String, u16)> {
        // Update interval from response
        // C++ sets both minInterval_ and interval_ to the reply interval directly
        if response.interval > 0 {
            let new_interval = Duration::from_secs(response.interval as u64);
            self.interval = new_interval;
            self.min_interval = new_interval;
            debug!("[BT] UDP tracker interval: {}s", response.interval);
        }

        // Update complete/incomplete counts
        self.complete = response.seeders as i64;
        self.incomplete = response.leechers as i64;
        debug!(
            "[BT] UDP tracker stats: seeders={}, leechers={}",
            response.seeders, response.leechers
        );

        // Mark announce success (resets in-flight counter, updates timing)
        self.announce_success();

        response.peers.clone()
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

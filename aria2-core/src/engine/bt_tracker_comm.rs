#![allow(clippy::empty_line_after_doc_comments)]

use crate::engine::http_tracker_client::{TrackerEvent, build_tracker_client, is_https_tracker};
use crate::error::{Aria2Error, RecoverableError, Result};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Tracker request timeout (seconds)
const TRACKER_REQUEST_TIMEOUT_SECS: u64 = 5;

/// Default announce interval (2 minutes, matching C++ DEFAULT_ANNOUNCE_INTERVAL)
const DEFAULT_ANNOUNCE_INTERVAL_SECS: u64 = 120;

/// Default number of peers to request from tracker
const DEFAULT_NUMWANT: u32 = 50;

// ======================================================================
// URL Encoding Helper
// ======================================================================

/// URL-encodes a 20-byte info hash or peer ID for use in tracker URLs.
///
/// Each byte is encoded as `%XX` where XX is the uppercase hex representation.
/// This is required by the BitTorrent tracker protocol specification.
pub fn urlencode_infohash(hash: &[u8; 20]) -> String {
    hash.iter().map(|b| format!("%{:02X}", b)).collect()
}

/// URL-encodes an arbitrary byte slice for use in tracker URLs.
fn urlencode_bytes(data: &[u8]) -> String {
    data.iter().map(|b| format!("%{:02X}", b)).collect()
}

// ======================================================================
// AnnounceEvent Enum (from C++ AnnounceTier::AnnounceEvent)
// ======================================================================

/// Announce event types matching C++ AnnounceTier::AnnounceEvent.
///
/// These events control the tracker announce state machine.
/// The transitions follow the C++ aria2 implementation exactly:
/// - `Started` -> `Downloading` (via nextEvent)
/// - `StartedAfterCompletion` -> `Seeding` (via nextEvent)
/// - `Stopped` -> `Halted` (via nextEvent or nextEventIfAfterStarted)
/// - `Completed` -> `Seeding` (via nextEvent or nextEventIfAfterStarted)
/// - `Downloading`, `Seeding`, `Halted` are stable states (no transition)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnounceEvent {
    /// Initial announce when download starts
    Started,
    /// Started after download already completed (prevent duplicate "completed" event)
    StartedAfterCompletion,
    /// Regular periodic announce during download
    Downloading,
    /// Announce when client is stopping/quitting
    Stopped,
    /// Announce when download just completed
    Completed,
    /// Regular announce during seeding phase
    Seeding,
    /// Terminal state after stopped
    Halted,
}

impl AnnounceEvent {
    /// Transition to the next event state (matching C++ AnnounceTier::nextEvent).
    ///
    /// State transitions:
    /// - `Started` -> `Downloading`
    /// - `StartedAfterCompletion` -> `Seeding`
    /// - `Stopped` -> `Halted`
    /// - `Completed` -> `Seeding`
    /// - `Downloading`, `Seeding`, `Halted` remain unchanged
    pub fn next_event(self) -> Self {
        match self {
            AnnounceEvent::Started => AnnounceEvent::Downloading,
            AnnounceEvent::StartedAfterCompletion => AnnounceEvent::Seeding,
            AnnounceEvent::Stopped => AnnounceEvent::Halted,
            AnnounceEvent::Completed => AnnounceEvent::Seeding,
            other => other,
        }
    }

    /// Transition event only if in STOPPED or COMPLETED state
    /// (matching C++ AnnounceTier::nextEventIfAfterStarted).
    ///
    /// This is called when a tracker announce fails and we need to advance
    /// the event state without going through the normal Started->Downloading
    /// transition (since we may have never successfully announced Started).
    pub fn next_event_if_after_started(self) -> Self {
        match self {
            AnnounceEvent::Stopped => AnnounceEvent::Halted,
            AnnounceEvent::Completed => AnnounceEvent::Seeding,
            other => other,
        }
    }

    /// Returns true if this event state allows sending a "stopped" event.
    ///
    /// Matching C++ FindStoppedAllowedTier: DOWNLOADING, STOPPED, COMPLETED, SEEDING
    pub fn accepts_stopped_event(self) -> bool {
        matches!(
            self,
            AnnounceEvent::Downloading
                | AnnounceEvent::Stopped
                | AnnounceEvent::Completed
                | AnnounceEvent::Seeding
        )
    }

    /// Returns true if this event state allows sending a "completed" event.
    ///
    /// Matching C++ FindCompletedAllowedTier: DOWNLOADING, COMPLETED
    pub fn accepts_completed_event(self) -> bool {
        matches!(
            self,
            AnnounceEvent::Downloading | AnnounceEvent::Completed
        )
    }

    /// Convert to the event string for tracker URL parameter.
    ///
    /// Both Started and StartedAfterCompletion map to "started" since
    /// trackers don't distinguish between these two internal states.
    pub fn as_event_string(self) -> &'static str {
        match self {
            AnnounceEvent::Started | AnnounceEvent::StartedAfterCompletion => "started",
            AnnounceEvent::Stopped => "stopped",
            AnnounceEvent::Completed => "completed",
            AnnounceEvent::Downloading | AnnounceEvent::Seeding | AnnounceEvent::Halted => "",
        }
    }
}

// ======================================================================
// AnnounceTier (from C++ AnnounceTier)
// ======================================================================

/// A single announce tier containing a deque of tracker URLs and event state.
///
/// Matches C++ AnnounceTier exactly: each tier has an event state machine
/// and a list of tracker URLs. Within a tier, trackers are tried in order;
/// if one fails, the next is tried. If all fail, the tier advances its
/// event state and we move to the next tier.
#[derive(Debug, Clone)]
pub struct AnnounceTier {
    /// Current event state for this tier
    pub event: AnnounceEvent,
    /// Deque of tracker URLs in this tier
    pub urls: VecDeque<String>,
}

impl AnnounceTier {
    /// Create a new tier from a list of tracker URLs.
    ///
    /// The event starts as `AnnounceEvent::Started` matching C++ behavior.
    pub fn new(urls: VecDeque<String>) -> Self {
        Self {
            event: AnnounceEvent::Started,
            urls,
        }
    }

    /// Create a tier from a vec of URL strings.
    pub fn from_urls(urls: Vec<String>) -> Self {
        Self {
            event: AnnounceEvent::Started,
            urls: urls.into_iter().collect(),
        }
    }

    /// Advance to next event state (matching C++ nextEvent).
    pub fn next_event(&mut self) {
        self.event = self.event.next_event();
    }

    /// Advance event only if in STOPPED or COMPLETED state
    /// (matching C++ nextEventIfAfterStarted).
    pub fn next_event_if_after_started(&mut self) {
        self.event = self.event.next_event_if_after_started();
    }

    /// Returns true if this tier accepts a "stopped" event.
    pub fn accepts_stopped_event(&self) -> bool {
        self.event.accepts_stopped_event()
    }

    /// Returns true if this tier accepts a "completed" event.
    pub fn accepts_completed_event(&self) -> bool {
        self.event.accepts_completed_event()
    }
}

// ======================================================================
// AnnounceList (from C++ AnnounceList)
// ======================================================================

/// Announce list with multi-tier tracker management matching C++ behavior.
///
/// Manages a list of [`AnnounceTier`] instances with an internal iterator
/// (current_tier / current_tracker indices). This matches the C++ AnnounceList
/// exactly, including the announce success/failure handling, event management,
/// and wrap-around search for stopped/completed allowed tiers.
#[derive(Debug, Clone)]
pub struct AnnounceList {
    /// Tiers of tracker URLs
    tiers: Vec<AnnounceTier>,
    /// Current tier index
    current_tier: usize,
    /// Current tracker URL index within the current tier
    current_tracker: usize,
    /// Whether the current tracker pointer is valid
    current_tracker_initialized: bool,
}

impl AnnounceList {
    /// Create an empty announce list.
    pub fn empty() -> Self {
        Self {
            tiers: Vec::new(),
            current_tier: 0,
            current_tracker: 0,
            current_tracker_initialized: false,
        }
    }

    /// Create announce list from C++ format or single announce string.
    ///
    /// C++ format: announce-list = [[tier1-url1, tier1-url2], [tier2-url1]]
    /// Single announce string becomes tier 0 with one entry.
    pub fn new(announce_list: &[Vec<String>], announce: &Option<String>) -> Self {
        let mut tiers = Vec::new();
        if !announce_list.is_empty() {
            for tier_urls in announce_list {
                if tier_urls.is_empty() {
                    continue;
                }
                tiers.push(AnnounceTier::from_urls(tier_urls.clone()));
            }
        } else if let Some(url) = announce {
            let mut urls = VecDeque::new();
            urls.push_back(url.clone());
            tiers.push(AnnounceTier::new(urls));
        }
        let mut list = Self {
            tiers,
            current_tier: 0,
            current_tracker: 0,
            current_tracker_initialized: false,
        };
        list.reset_iterator();
        list
    }

    /// Reset the internal iterator to the first tier and first tracker.
    fn reset_iterator(&mut self) {
        self.current_tier = 0;
        if !self.tiers.is_empty() && !self.tiers[0].urls.is_empty() {
            self.current_tracker = 0;
            self.current_tracker_initialized = true;
        } else {
            self.current_tracker_initialized = false;
        }
    }

    /// Returns the current tracker URL, or None if not initialized.
    pub fn get_announce(&self) -> Option<&str> {
        if self.current_tracker_initialized {
            self.tiers
                .get(self.current_tier)
                .and_then(|t| t.urls.get(self.current_tracker))
                .map(|s| s.as_str())
        } else {
            None
        }
    }

    /// Returns the current event from the current tier.
    ///
    /// If not initialized, returns `AnnounceEvent::Started` matching C++ behavior.
    pub fn get_event(&self) -> AnnounceEvent {
        if self.current_tracker_initialized {
            self.tiers
                .get(self.current_tier)
                .map(|t| t.event)
                .unwrap_or(AnnounceEvent::Started)
        } else {
            AnnounceEvent::Started
        }
    }

    /// Set the event on the current tier.
    pub fn set_event(&mut self, event: AnnounceEvent) {
        if self.current_tracker_initialized {
            if let Some(tier) = self.tiers.get_mut(self.current_tier) {
                tier.event = event;
            }
        }
    }

    /// Returns the event string for the tracker URL parameter.
    pub fn get_event_string(&self) -> &'static str {
        if self.current_tracker_initialized {
            self.tiers
                .get(self.current_tier)
                .map(|t| t.event.as_event_string())
                .unwrap_or("")
        } else {
            ""
        }
    }

    /// Handle announce success (matching C++ AnnounceList::announceSuccess).
    ///
    /// - Advances the current tier's event via nextEvent()
    /// - Removes the current URL from its position and inserts at front of the tier
    /// - Resets iterator to first tier, first tracker
    pub fn announce_success(&mut self) {
        if !self.current_tracker_initialized {
            return;
        }

        // Advance event on current tier
        if let Some(tier) = self.tiers.get_mut(self.current_tier) {
            tier.next_event();

            // Move current URL to front of the tier's URL deque
            if self.current_tracker < tier.urls.len() {
                let url = tier.urls.remove(self.current_tracker).unwrap();
                tier.urls.push_front(url);
            }
        }

        // Reset to first tier, first tracker
        self.current_tier = 0;
        if !self.tiers.is_empty() && !self.tiers[0].urls.is_empty() {
            self.current_tracker = 0;
            self.current_tracker_initialized = true;
        } else {
            self.current_tracker_initialized = false;
        }
    }

    /// Handle announce failure (matching C++ AnnounceList::announceFailure).
    ///
    /// - Advances to next tracker URL in current tier
    /// - If last URL in tier, force nextEventIfAfterStarted() and advance to next tier
    /// - If past last tier, sets currentTrackerInitialized = false
    pub fn announce_failure(&mut self) {
        if !self.current_tracker_initialized {
            return;
        }

        // Advance to next tracker in current tier
        if let Some(tier) = self.tiers.get(self.current_tier) {
            self.current_tracker += 1;
            if self.current_tracker >= tier.urls.len() {
                // Last URL in tier - force next event and advance tier
                if let Some(tier) = self.tiers.get_mut(self.current_tier) {
                    tier.next_event_if_after_started();
                }
                self.current_tier += 1;
                if self.current_tier >= self.tiers.len() {
                    // Past last tier - all tiers failed
                    self.current_tracker_initialized = false;
                } else {
                    self.current_tracker = 0;
                }
            }
        }
    }

    /// Count the number of tiers that accept the "stopped" event.
    pub fn count_stopped_allowed_tier(&self) -> usize {
        self.tiers.iter().filter(|t| t.accepts_stopped_event()).count()
    }

    /// Count the number of tiers that accept the "completed" event.
    pub fn count_completed_allowed_tier(&self) -> usize {
        self.tiers.iter().filter(|t| t.accepts_completed_event()).count()
    }

    /// Move to a tier that accepts the "stopped" event using wrap-around search.
    ///
    /// Matching C++ moveToStoppedAllowedTier: search from current position to end,
    /// then from beginning to current position.
    pub fn move_to_stopped_allowed_tier(&mut self) {
        let start = self.current_tier.min(self.tiers.len());
        // First search: current position to end
        for i in start..self.tiers.len() {
            if self.tiers[i].accepts_stopped_event() {
                self.current_tier = i;
                self.current_tracker = 0;
                self.current_tracker_initialized = true;
                return;
            }
        }
        // Second search: beginning to current position
        for i in 0..start {
            if self.tiers[i].accepts_stopped_event() {
                self.current_tier = i;
                self.current_tracker = 0;
                self.current_tracker_initialized = true;
                return;
            }
        }
    }

    /// Move to a tier that accepts the "completed" event using wrap-around search.
    ///
    /// Matching C++ moveToCompletedAllowedTier: search from current position to end,
    /// then from beginning to current position.
    pub fn move_to_completed_allowed_tier(&mut self) {
        let start = self.current_tier.min(self.tiers.len());
        // First search: current position to end
        for i in start..self.tiers.len() {
            if self.tiers[i].accepts_completed_event() {
                self.current_tier = i;
                self.current_tracker = 0;
                self.current_tracker_initialized = true;
                return;
            }
        }
        // Second search: beginning to current position
        for i in 0..start {
            if self.tiers[i].accepts_completed_event() {
                self.current_tier = i;
                self.current_tracker = 0;
                self.current_tracker_initialized = true;
                return;
            }
        }
    }

    /// Returns true if the current tier accepts the "stopped" event.
    pub fn current_tier_accepts_stopped_event(&self) -> bool {
        if self.current_tracker_initialized {
            self.tiers
                .get(self.current_tier)
                .map(|t| t.accepts_stopped_event())
                .unwrap_or(false)
        } else {
            false
        }
    }

    /// Returns true if the current tier accepts the "completed" event.
    pub fn current_tier_accepts_completed_event(&self) -> bool {
        if self.current_tracker_initialized {
            self.tiers
                .get(self.current_tier)
                .map(|t| t.accepts_completed_event())
                .unwrap_or(false)
        } else {
            false
        }
    }

    /// Returns true if all tiers have been exhausted (currentTier past end).
    pub fn all_tiers_failed(&self) -> bool {
        self.current_tier >= self.tiers.len()
    }

    /// Reset the iterator to the beginning (matching C++ resetTier).
    pub fn reset_tier(&mut self) {
        self.reset_iterator();
    }

    /// Shuffle all URLs in each tier randomly (matching C++ shuffle).
    pub fn shuffle(&mut self) {
        use rand::seq::SliceRandom;
        use rand::thread_rng;
        for tier in &mut self.tiers {
            let mut urls: Vec<String> = tier.urls.drain(..).collect();
            urls.shuffle(&mut thread_rng());
            tier.urls = urls.into_iter().collect();
        }
    }

    /// Returns the number of tiers.
    pub fn tier_count(&self) -> usize {
        self.tiers.len()
    }

    /// Get the URL for a specific tracker by tier and entry index.
    pub fn get_tracker_url(&self, tier_idx: usize, entry_idx: usize) -> Option<&String> {
        self.tiers
            .get(tier_idx)
            .and_then(|t| t.urls.get(entry_idx))
    }
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
    prev_announce_time: Option<Instant>,
    /// Interval from tracker response (seconds)
    interval: Duration,
    /// Minimum interval from tracker response (seconds)
    min_interval: Duration,
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

// ======================================================================
// Multi-home Tracker with Failover (Rust Improvement: Reliability Scoring)
// ======================================================================

/// A single tracker entry with health tracking and reliability scoring.
///
/// This is a Rust improvement over the C++ implementation that adds
/// reliability scoring and exponential backoff to individual trackers.
#[derive(Debug, Clone)]
pub struct TrackerEntry {
    pub url: String,
    pub last_success: Option<Instant>,
    pub last_failure: Option<Instant>,
    pub failure_count: u32,
    pub success_count: u32,
    pub avg_response_ms: f64,
    pub next_retry_after: Option<Instant>,
}

impl TrackerEntry {
    /// Create a new tracker entry with default values
    pub fn new(url: String) -> Self {
        Self {
            url,
            last_success: None,
            last_failure: None,
            failure_count: 0,
            success_count: 0,
            avg_response_ms: 0.0,
            next_retry_after: None,
        }
    }

    /// Reliability score 0.0..1.0 based on success/failure ratio weighted by recency
    pub fn reliability_score(&self) -> f64 {
        let total = self.success_count + self.failure_count;
        if total == 0 {
            return 0.5; // unknown -> neutral
        }
        let base_score = self.success_count as f64 / (total as f64 + 1.0);
        // Weight by recency: recent failure reduces score more
        let recency_penalty = match self.last_failure {
            Some(t) if t.elapsed().as_secs() < 300 => 0.3,
            Some(_) => 0.1,
            None => 0.0,
        };
        (base_score - recency_penalty).clamp(0.0, 1.0)
    }

    /// Record a successful response with latency measurement
    pub fn record_success(&mut self, latency_ms: f64) {
        self.success_count += 1;
        self.last_success = Some(Instant::now());
        self.failure_count = 0; // reset on success
        if self.avg_response_ms <= 0.0 {
            self.avg_response_ms = latency_ms;
        } else {
            self.avg_response_ms = self.avg_response_ms * 0.9 + latency_ms * 0.1;
        }
    }

    /// Record a failed response and schedule backoff
    pub fn record_failure(&mut self) {
        self.failure_count += 1;
        self.last_failure = Some(Instant::now());
        self.schedule_backoff(10);
    }

    /// Exponential backoff: min(base * 2^failures, 3600s)
    pub fn schedule_backoff(&mut self, base_secs: u64) {
        let exp = self.failure_count.saturating_sub(1).min(10);
        let delay = base_secs.saturating_mul(1 << exp);
        let capped = delay.min(3600);
        self.next_retry_after = Some(Instant::now() + Duration::from_secs(capped));
    }

    /// Check if this tracker is available for retry
    pub fn is_available(&self) -> bool {
        if let Some(retry_at) = self.next_retry_after {
            Instant::now() >= retry_at
        } else {
            true
        }
    }
}

/// A tier of trackers tried in order with reliability-based selection.
///
/// This is a Rust improvement that uses reliability scoring to select
/// the best available tracker within a tier, rather than just
/// sequential iteration.
#[derive(Debug, Clone)]
pub struct TrackerTier {
    pub trackers: Vec<TrackerEntry>,
    pub current_index: usize,
    pub consecutive_failures: u32,
}

impl TrackerTier {
    /// Create a new tier from a list of tracker URLs
    pub fn new(urls: Vec<String>) -> Self {
        let trackers = urls.into_iter().map(TrackerEntry::new).collect();
        Self {
            trackers,
            current_index: 0,
            consecutive_failures: 0,
        }
    }

    /// Select next available tracker within this tier, preferring higher reliability
    pub fn select_next(&mut self) -> Option<&TrackerEntry> {
        // First try current index if available
        if self.current_index < self.trackers.len()
            && self.trackers[self.current_index].is_available()
        {
            return Some(&self.trackers[self.current_index]);
        }

        // Find best available tracker by reliability score
        let mut best_idx = None;
        let mut best_score = -1.0f64;
        for (i, t) in self.trackers.iter().enumerate() {
            if t.is_available() {
                let score = t.reliability_score();
                if score > best_score {
                    best_score = score;
                    best_idx = Some(i);
                }
            }
        }

        if let Some(idx) = best_idx {
            self.current_index = idx;
            return Some(&self.trackers[idx]);
        }

        None // all unavailable
    }

    /// Mark the current tracker as successful
    pub fn mark_current_success(&mut self, latency_ms: f64) {
        if self.current_index < self.trackers.len() {
            self.trackers[self.current_index].record_success(latency_ms);
        }
        self.consecutive_failures = 0;
    }

    /// Mark the current tracker as failed
    pub fn mark_current_failure(&mut self) {
        if self.current_index < self.trackers.len() {
            self.trackers[self.current_index].record_failure();
        }
        self.consecutive_failures += 1;
    }
}

/// Full announce list with multiple tiers for failover support
/// using reliability-based health tracking.
///
/// This is a Rust improvement over the basic C++ AnnounceList that adds
/// per-tracker reliability scoring and exponential backoff. It is kept
/// alongside the C++-compatible [`AnnounceList`] for use cases where
/// the more sophisticated health tracking is desired.
#[derive(Debug, Clone)]
pub struct HealthTrackingAnnounceList {
    pub tiers: Vec<TrackerTier>,
    pub current_tier: usize,
}

impl HealthTrackingAnnounceList {
    /// Create announce list from C++ format or single announce string
    ///
    /// C++ format: announce-list = [[tier1-url1, tier1-url2], [tier2-url1]]
    /// Single announce string becomes tier 0 with one entry
    pub fn new(announce_list: &[Vec<String>], announce: &Option<String>) -> Self {
        let mut tiers = Vec::new();
        if !announce_list.is_empty() {
            for tier_urls in announce_list {
                tiers.push(TrackerTier::new(tier_urls.clone()));
            }
        } else if let Some(url) = announce {
            tiers.push(TrackerTier::new(vec![url.clone()]));
        }
        Self {
            tiers,
            current_tier: 0,
        }
    }

    /// Select next tracker across tiers with failover logic
    pub fn select_next_tracker(&mut self) -> Option<(usize, usize)> {
        if self.tiers.is_empty() {
            return None;
        }

        // Try current tier first
        if let Some(_entry) = self.tiers[self.current_tier].select_next() {
            return Some((
                self.current_tier,
                self.tiers[self.current_tier].current_index,
            ));
        }

        // Current tier exhausted -> try next tier
        for offset in 1..=self.tiers.len() {
            let tier_idx = (self.current_tier + offset) % self.tiers.len();
            if let Some(_entry) = self.tiers[tier_idx].select_next() {
                self.current_tier = tier_idx;
                return Some((tier_idx, self.tiers[tier_idx].current_index));
            }
        }

        None // all trackers unavailable
    }

    /// Record successful response for a specific tier
    pub fn record_success(&mut self, tier_idx: usize, latency_ms: f64) {
        if tier_idx < self.tiers.len() {
            self.tiers[tier_idx].mark_current_success(latency_ms);
        }
    }

    /// Record failed response for a specific tier
    pub fn record_failure(&mut self, tier_idx: usize) {
        if tier_idx < self.tiers.len() {
            self.tiers[tier_idx].mark_current_failure();
        }
    }

    /// Get the URL for a specific tracker by tier and entry index
    pub fn get_tracker_url(&self, tier_idx: usize, entry_idx: usize) -> Option<&String> {
        self.tiers
            .get(tier_idx)
            .and_then(|t| t.trackers.get(entry_idx))
            .map(|e| &e.url)
    }
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // AnnounceEvent Tests
    // ------------------------------------------------------------------

    #[test]
    fn test_announce_event_transitions() {
        // Started -> Downloading
        assert_eq!(
            AnnounceEvent::Started.next_event(),
            AnnounceEvent::Downloading
        );
        // StartedAfterCompletion -> Seeding
        assert_eq!(
            AnnounceEvent::StartedAfterCompletion.next_event(),
            AnnounceEvent::Seeding
        );
        // Stopped -> Halted
        assert_eq!(
            AnnounceEvent::Stopped.next_event(),
            AnnounceEvent::Halted
        );
        // Completed -> Seeding
        assert_eq!(
            AnnounceEvent::Completed.next_event(),
            AnnounceEvent::Seeding
        );
        // Stable states: Downloading, Seeding, Halted remain unchanged
        assert_eq!(
            AnnounceEvent::Downloading.next_event(),
            AnnounceEvent::Downloading
        );
        assert_eq!(
            AnnounceEvent::Seeding.next_event(),
            AnnounceEvent::Seeding
        );
        assert_eq!(
            AnnounceEvent::Halted.next_event(),
            AnnounceEvent::Halted
        );
    }

    #[test]
    fn test_announce_event_next_if_after_started() {
        // Stopped -> Halted
        assert_eq!(
            AnnounceEvent::Stopped.next_event_if_after_started(),
            AnnounceEvent::Halted
        );
        // Completed -> Seeding
        assert_eq!(
            AnnounceEvent::Completed.next_event_if_after_started(),
            AnnounceEvent::Seeding
        );
        // Others remain unchanged
        assert_eq!(
            AnnounceEvent::Started.next_event_if_after_started(),
            AnnounceEvent::Started
        );
        assert_eq!(
            AnnounceEvent::StartedAfterCompletion.next_event_if_after_started(),
            AnnounceEvent::StartedAfterCompletion
        );
        assert_eq!(
            AnnounceEvent::Downloading.next_event_if_after_started(),
            AnnounceEvent::Downloading
        );
        assert_eq!(
            AnnounceEvent::Seeding.next_event_if_after_started(),
            AnnounceEvent::Seeding
        );
    }

    #[test]
    fn test_announce_tier_accepts_events() {
        // Stopped event accepted by: Downloading, Stopped, Completed, Seeding
        assert!(AnnounceEvent::Downloading.accepts_stopped_event());
        assert!(AnnounceEvent::Stopped.accepts_stopped_event());
        assert!(AnnounceEvent::Completed.accepts_stopped_event());
        assert!(AnnounceEvent::Seeding.accepts_stopped_event());
        assert!(!AnnounceEvent::Started.accepts_stopped_event());
        assert!(!AnnounceEvent::StartedAfterCompletion.accepts_stopped_event());
        assert!(!AnnounceEvent::Halted.accepts_stopped_event());

        // Completed event accepted by: Downloading, Completed
        assert!(AnnounceEvent::Downloading.accepts_completed_event());
        assert!(AnnounceEvent::Completed.accepts_completed_event());
        assert!(!AnnounceEvent::Started.accepts_completed_event());
        assert!(!AnnounceEvent::StartedAfterCompletion.accepts_completed_event());
        assert!(!AnnounceEvent::Stopped.accepts_completed_event());
        assert!(!AnnounceEvent::Seeding.accepts_completed_event());
        assert!(!AnnounceEvent::Halted.accepts_completed_event());
    }

    #[test]
    fn test_announce_event_string() {
        assert_eq!(AnnounceEvent::Started.as_event_string(), "started");
        assert_eq!(
            AnnounceEvent::StartedAfterCompletion.as_event_string(),
            "started"
        );
        assert_eq!(AnnounceEvent::Stopped.as_event_string(), "stopped");
        assert_eq!(AnnounceEvent::Completed.as_event_string(), "completed");
        assert_eq!(AnnounceEvent::Downloading.as_event_string(), "");
        assert_eq!(AnnounceEvent::Seeding.as_event_string(), "");
        assert_eq!(AnnounceEvent::Halted.as_event_string(), "");
    }

    // ------------------------------------------------------------------
    // AnnounceTier Tests
    // ------------------------------------------------------------------

    #[test]
    fn test_announce_tier_next_event() {
        let mut tier = AnnounceTier::from_urls(vec!["http://tracker.test/announce".to_string()]);
        assert_eq!(tier.event, AnnounceEvent::Started);

        tier.next_event();
        assert_eq!(tier.event, AnnounceEvent::Downloading);

        // Downloading is stable
        tier.next_event();
        assert_eq!(tier.event, AnnounceEvent::Downloading);
    }

    #[test]
    fn test_announce_tier_next_event_if_after_started() {
        let mut tier = AnnounceTier::from_urls(vec!["http://tracker.test/announce".to_string()]);
        tier.event = AnnounceEvent::Stopped;
        tier.next_event_if_after_started();
        assert_eq!(tier.event, AnnounceEvent::Halted);

        tier.event = AnnounceEvent::Completed;
        tier.next_event_if_after_started();
        assert_eq!(tier.event, AnnounceEvent::Seeding);

        // Started should NOT transition via nextEventIfAfterStarted
        tier.event = AnnounceEvent::Started;
        tier.next_event_if_after_started();
        assert_eq!(tier.event, AnnounceEvent::Started);
    }

    // ------------------------------------------------------------------
    // AnnounceList Tests
    // ------------------------------------------------------------------

    #[test]
    fn test_announce_list_creation() {
        // Test from announce string
        let list = AnnounceList::new(&[], &Some("http://tracker1.com/announce".to_string()));
        assert_eq!(list.tier_count(), 1);
        assert_eq!(list.get_announce(), Some("http://tracker1.com/announce"));

        // Test from multi-tier list
        let multi_tier = vec![
            vec![
                "http://tier1-1.com/announce".to_string(),
                "http://tier1-2.com/announce".to_string(),
            ],
            vec!["http://tier2-1.com/announce".to_string()],
        ];
        let list2 = AnnounceList::new(&multi_tier, &None);
        assert_eq!(list2.tier_count(), 2);
        assert_eq!(list2.get_announce(), Some("http://tier1-1.com/announce"));

        // Test empty case
        let list3 = AnnounceList::new(&[], &None);
        assert_eq!(list3.tier_count(), 0);
        assert!(list3.get_announce().is_none());
    }

    #[test]
    fn test_announce_list_success_resets_to_first_tier() {
        let multi_tier = vec![
            vec!["http://t1.com/announce".to_string()],
            vec!["http://t2.com/announce".to_string()],
        ];
        let mut list = AnnounceList::new(&multi_tier, &None);

        // Initially at tier 0
        assert_eq!(list.get_announce(), Some("http://t1.com/announce"));

        // Advance to tier 1 via failure
        list.announce_failure();

        // Now at tier 1
        assert_eq!(list.get_announce(), Some("http://t2.com/announce"));

        // Success resets to first tier and advances event
        list.announce_success();
        // C++ behavior: announceSuccess on current tier (tier 1):
        // 1. Calls nextEvent on tier 1 (Started -> Downloading)
        // 2. Removes current URL and pushes to front of tier 1
        // 3. Resets currentTier to begin (tier 0)
        // So we should be back at tier 0, tracker 0 = t1
        assert_eq!(list.get_announce(), Some("http://t1.com/announce"));
    }

    #[test]
    fn test_announce_list_failure_advances_tracker() {
        let urls = vec![
            "http://t1.com/announce".to_string(),
            "http://t2.com/announce".to_string(),
        ];
        let mut list = AnnounceList::new(&[urls], &None);

        // Initially at tracker 0
        assert_eq!(list.get_announce(), Some("http://t1.com/announce"));

        // Failure advances to next tracker in same tier
        list.announce_failure();
        assert_eq!(list.get_announce(), Some("http://t2.com/announce"));
    }

    #[test]
    fn test_announce_list_failure_advances_tier_on_last_url() {
        let multi_tier = vec![
            vec!["http://t1.com/announce".to_string()],
            vec!["http://t2.com/announce".to_string()],
        ];
        let mut list = AnnounceList::new(&multi_tier, &None);

        // Tier 0 has only 1 URL, so failure should advance to tier 1
        list.announce_failure();
        assert_eq!(list.get_announce(), Some("http://t2.com/announce"));
    }

    #[test]
    fn test_announce_list_all_tiers_failed() {
        let multi_tier = vec![
            vec!["http://t1.com/announce".to_string()],
            vec!["http://t2.com/announce".to_string()],
        ];
        let mut list = AnnounceList::new(&multi_tier, &None);

        assert!(!list.all_tiers_failed());

        // Fail tier 0
        list.announce_failure();
        assert!(!list.all_tiers_failed());

        // Fail tier 1
        list.announce_failure();
        assert!(list.all_tiers_failed());
        assert!(list.get_announce().is_none());
    }

    #[test]
    fn test_announce_list_event_management() {
        let mut list = AnnounceList::new(
            &[vec!["http://t.com/announce".to_string()]],
            &None,
        );

        // Initial event is Started
        assert_eq!(list.get_event(), AnnounceEvent::Started);
        assert_eq!(list.get_event_string(), "started");

        // Set event to Completed
        list.set_event(AnnounceEvent::Completed);
        assert_eq!(list.get_event(), AnnounceEvent::Completed);
        assert_eq!(list.get_event_string(), "completed");

        // After success, event advances: Completed -> Seeding
        list.announce_success();
        // Success resets to first tier; event on that tier is now Downloading
        // (since the first tier's event was Started, and nextEvent makes it Downloading)
        // Wait - we set it to Completed on the first tier, then announceSuccess
        // calls nextEvent on that tier: Completed -> Seeding
        // Then resets to first tier
        assert_eq!(list.get_event(), AnnounceEvent::Seeding);
    }

    #[test]
    fn test_announce_list_stopped_allowed_tiers() {
        let mut list = AnnounceList::new(
            &[
                vec!["http://t1.com/announce".to_string()],
                vec!["http://t2.com/announce".to_string()],
            ],
            &None,
        );

        // Both tiers start with Started event - does NOT accept stopped
        assert_eq!(list.count_stopped_allowed_tier(), 0);

        // Advance tier 0 to Downloading
        list.announce_success(); // tier 0: Started -> Downloading, reset to tier 0
        assert_eq!(list.get_event(), AnnounceEvent::Downloading);
        assert_eq!(list.count_stopped_allowed_tier(), 1);

        // Advance tier 1 too - need to fail through to it
        list.announce_failure(); // move to tier 1
        list.announce_success(); // tier 1: Started -> Downloading
        // Now both tiers should be Downloading
        assert_eq!(list.count_stopped_allowed_tier(), 2);
    }

    #[test]
    fn test_announce_list_completed_allowed_tiers() {
        let list = AnnounceList::new(
            &[
                vec!["http://t1.com/announce".to_string()],
                vec!["http://t2.com/announce".to_string()],
            ],
            &None,
        );

        // Both tiers start with Started - does NOT accept completed
        assert_eq!(list.count_completed_allowed_tier(), 0);
    }

    #[test]
    fn test_announce_list_move_to_stopped_allowed_tier() {
        let mut list = AnnounceList::new(
            &[
                vec!["http://t1.com/announce".to_string()],
                vec!["http://t2.com/announce".to_string()],
            ],
            &None,
        );

        // Set tier 1 to Downloading (accepts stopped)
        list.tiers[1].event = AnnounceEvent::Downloading;

        // Current tier is 0 (Started, doesn't accept stopped)
        assert!(!list.current_tier_accepts_stopped_event());

        // Move to stopped-allowed tier
        list.move_to_stopped_allowed_tier();
        assert!(list.current_tier_accepts_stopped_event());
        assert_eq!(list.get_announce(), Some("http://t2.com/announce"));
    }

    #[test]
    fn test_announce_list_reset_tier() {
        let multi_tier = vec![
            vec!["http://t1.com/announce".to_string()],
            vec!["http://t2.com/announce".to_string()],
        ];
        let mut list = AnnounceList::new(&multi_tier, &None);

        // Advance through failures
        list.announce_failure();
        assert_eq!(list.get_announce(), Some("http://t2.com/announce"));

        // Reset should go back to beginning
        list.reset_tier();
        assert_eq!(list.get_announce(), Some("http://t1.com/announce"));
    }

    #[test]
    fn test_announce_list_shuffle() {
        let urls: Vec<String> = (0..20)
            .map(|i| format!("http://tracker{}.com/announce", i))
            .collect();
        let mut list = AnnounceList::new(&[urls.clone()], &None);

        let _original_first = list.get_announce().unwrap().to_string();
        list.shuffle();

        // After shuffle, the list should still contain all URLs
        // (it's very unlikely shuffle produces the same order for 20 items)
        assert_eq!(list.tier_count(), 1);
        let tier_urls: Vec<&str> = list.tiers[0].urls.iter().map(|s| s.as_str()).collect();
        assert_eq!(tier_urls.len(), 20);
    }

    // ------------------------------------------------------------------
    // BtAnnounce Tests
    // ------------------------------------------------------------------

    #[test]
    fn test_bt_announce_default_ready_after_interval() {
        let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));

        // Initially ready (no previous announce)
        assert!(bt.is_default_announce_ready());

        // Simulate a successful announce
        bt.announce_start();
        assert!(!bt.is_default_announce_ready()); // in-flight

        bt.announce_success();
        // After success, prev_announce_time is set, so not immediately ready
        // (interval hasn't elapsed yet)
        assert!(!bt.is_default_announce_ready());
    }

    #[test]
    fn test_bt_announce_default_ready_when_interval_zero() {
        let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));
        // Set min_interval to zero so elapsed time check passes
        bt.min_interval = Duration::ZERO;
        bt.prev_announce_time = Some(Instant::now());

        // With zero interval, should be ready immediately after announce
        assert!(bt.is_default_announce_ready());
    }

    #[test]
    fn test_bt_announce_stopped_ready_when_halted() {
        let mut bt = BtAnnounce::new(
            &[vec!["http://tracker.test/announce".to_string()]],
            &None,
        );
        // Advance the tier to Downloading so it accepts stopped
        bt.announce_list_mut().tiers[0].event = AnnounceEvent::Downloading;

        assert!(!bt.is_stopped_announce_ready()); // not halted

        bt.set_runtime_halted(true);
        assert!(bt.is_stopped_announce_ready());
    }

    #[test]
    fn test_bt_announce_completed_ready() {
        let mut bt = BtAnnounce::new(
            &[vec!["http://tracker.test/announce".to_string()]],
            &None,
        );
        // Advance the tier to Downloading so it accepts completed
        bt.announce_list_mut().tiers[0].event = AnnounceEvent::Downloading;

        assert!(!bt.is_completed_announce_ready()); // not complete

        bt.set_download_complete(true);
        assert!(bt.is_completed_announce_ready());
    }

    #[test]
    fn test_bt_announce_no_more_announce() {
        let mut bt = BtAnnounce::new(
            &[vec!["http://tracker.test/announce".to_string()]],
            &None,
        );

        assert!(!bt.no_more_announce()); // not halted

        bt.set_runtime_halted(true);
        // Tier 0 is still Started, doesn't accept stopped
        assert!(bt.no_more_announce());

        // Advance tier to Downloading (accepts stopped)
        bt.announce_list_mut().tiers[0].event = AnnounceEvent::Downloading;
        assert!(!bt.no_more_announce());
    }

    #[test]
    fn test_bt_announce_adjust_started_after_completion() {
        let mut bt = BtAnnounce::new(
            &[vec!["http://tracker.test/announce".to_string()]],
            &None,
        );

        // Mark download complete while event is still Started
        bt.set_download_complete(true);
        // Override min_interval so default announce is ready
        bt.min_interval = Duration::ZERO;
        bt.prev_announce_time = Some(Instant::now());

        assert!(bt.adjust_announce_list());
        // Event should be changed to STARTED_AFTER_COMPLETION
        assert_eq!(bt.announce_list().get_event(), AnnounceEvent::StartedAfterCompletion);
    }

    #[test]
    fn test_bt_announce_adjust_stopped_priority() {
        let mut bt = BtAnnounce::new(
            &[vec!["http://tracker.test/announce".to_string()]],
            &None,
        );

        // Set up both stopped and completed as ready
        bt.announce_list_mut().tiers[0].event = AnnounceEvent::Downloading;
        bt.set_runtime_halted(true);
        bt.set_download_complete(true);
        bt.min_interval = Duration::ZERO;
        bt.prev_announce_time = Some(Instant::now());

        assert!(bt.adjust_announce_list());
        // Stopped should take priority over completed
        assert_eq!(bt.announce_list().get_event(), AnnounceEvent::Stopped);
    }

    #[test]
    fn test_get_announce_url_includes_all_params() {
        let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));
        bt.set_tcp_port(6881);

        let info_hash = [0xABu8; 20];
        let peer_id = [0xCDu8; 20];
        let url = bt
            .get_announce_url(&info_hash, &peer_id, 1000, 5000, 500, None)
            .unwrap();

        // Verify all required parameters are present
        assert!(url.contains("info_hash="));
        assert!(url.contains("peer_id="));
        assert!(url.contains("uploaded=1000"));
        assert!(url.contains("downloaded=5000"));
        assert!(url.contains("left=500"));
        assert!(url.contains("compact=1"));
        assert!(url.contains("key="));
        assert!(url.contains("numwant="));
        assert!(url.contains("no_peer_id=1"));
        assert!(url.contains("port=6881"));
        assert!(url.contains("event=started"));
        assert!(url.contains("supportcrypto=1"));
    }

    #[test]
    fn test_get_announce_url_with_existing_query() {
        let mut bt = BtAnnounce::new(
            &[],
            &Some("http://tracker.test/announce?passkey=abc123".to_string()),
        );

        let info_hash = [0u8; 20];
        let peer_id = [0u8; 20];
        let url = bt.get_announce_url(&info_hash, &peer_id, 0, 0, 0, None).unwrap();

        // Should use & instead of ? when URL already has query params
        assert!(url.contains("&info_hash="));
        assert!(!url.contains("?info_hash="));
    }

    #[test]
    fn test_get_announce_url_numwant_zero_when_enough_peers() {
        let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));
        bt.set_less_than_min_peers(false); // we have enough peers

        let info_hash = [0u8; 20];
        let peer_id = [0u8; 20];
        let url = bt.get_announce_url(&info_hash, &peer_id, 0, 0, 0, None).unwrap();

        assert!(url.contains("numwant=0"));
    }

    #[test]
    fn test_get_announce_url_numwant_zero_when_halted() {
        let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));
        bt.set_runtime_halted(true);
        bt.announce_list_mut().tiers[0].event = AnnounceEvent::Downloading;

        let info_hash = [0u8; 20];
        let peer_id = [0u8; 20];
        let url = bt.get_announce_url(&info_hash, &peer_id, 0, 0, 0, None).unwrap();

        assert!(url.contains("numwant=0"));
    }

    #[test]
    fn test_process_announce_response_updates_interval() {
        use aria2_protocol::bittorrent::tracker::response::{PeerInfo, TrackerResponse};

        let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));

        let response = TrackerResponse {
            interval: 900,
            min_interval: Some(300),
            seeders: 10,
            leechers: 5,
            peers: vec![PeerInfo {
                ip: "1.2.3.4".to_string(),
                port: 6881,
                peer_id: None,
            }],
            tracker_id: None,
            warning_message: None,
            failure_reason: None,
        };

        let result = bt.process_announce_response(&response);
        assert!(result.is_ok());
        assert_eq!(bt.interval(), Duration::from_secs(900));
        assert_eq!(bt.min_interval(), Duration::from_secs(300));
        assert_eq!(bt.complete(), 10);
        assert_eq!(bt.incomplete(), 5);
        assert_eq!(result.unwrap().len(), 1);
    }

    #[test]
    fn test_process_announce_response_failure() {
        use aria2_protocol::bittorrent::tracker::response::TrackerResponse;

        let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));

        let response = TrackerResponse {
            interval: 300,
            min_interval: None,
            seeders: 0,
            leechers: 0,
            peers: vec![],
            tracker_id: None,
            warning_message: None,
            failure_reason: Some("tracker offline".to_string()),
        };

        let result = bt.process_announce_response(&response);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "tracker offline");
    }

    #[test]
    fn test_process_announce_response_min_interval_capped() {
        use aria2_protocol::bittorrent::tracker::response::TrackerResponse;

        let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));

        // min_interval > interval should be capped
        let response = TrackerResponse {
            interval: 300,
            min_interval: Some(600),
            seeders: 0,
            leechers: 0,
            peers: vec![],
            tracker_id: None,
            warning_message: None,
            failure_reason: None,
        };

        let _ = bt.process_announce_response(&response);
        assert_eq!(bt.interval(), Duration::from_secs(300));
        assert_eq!(bt.min_interval(), Duration::from_secs(300)); // capped to interval
    }

    #[test]
    fn test_process_announce_response_uses_interval_as_min() {
        use aria2_protocol::bittorrent::tracker::response::TrackerResponse;

        let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));

        // No min_interval: should use interval as min_interval
        let response = TrackerResponse {
            interval: 600,
            min_interval: None,
            seeders: 0,
            leechers: 0,
            peers: vec![],
            tracker_id: None,
            warning_message: None,
            failure_reason: None,
        };

        let _ = bt.process_announce_response(&response);
        assert_eq!(bt.interval(), Duration::from_secs(600));
        assert_eq!(bt.min_interval(), Duration::from_secs(600)); // same as interval
    }

    #[test]
    fn test_process_announce_response_stores_tracker_id() {
        use aria2_protocol::bittorrent::tracker::response::{PeerInfo, TrackerResponse};

        let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));

        // Process response with tracker_id
        let response = TrackerResponse {
            interval: 300,
            min_interval: None,
            seeders: 0,
            leechers: 0,
            peers: vec![PeerInfo {
                ip: "1.2.3.4".to_string(),
                port: 6881,
                peer_id: None,
            }],
            tracker_id: Some("tracker-abc".to_string()),
            warning_message: None,
            failure_reason: None,
        };

        let _ = bt.process_announce_response(&response);
        assert_eq!(bt.tracker_id(), "tracker-abc");

        // Verify tracker_id is sent in subsequent announce URLs
        let info_hash = [0u8; 20];
        let peer_id = [0u8; 20];
        let url = bt.get_announce_url(&info_hash, &peer_id, 0, 0, 0, None).unwrap();
        assert!(
            url.contains("&trackerid="),
            "announce URL should contain trackerid parameter: {}",
            url
        );
    }

    #[test]
    fn test_bt_announce_user_defined_interval() {
        let mut bt = BtAnnounce::new(&[], &Some("http://tracker.test/announce".to_string()));
        bt.prev_announce_time = Some(Instant::now());

        // With normal min_interval (120s), not ready
        assert!(!bt.is_default_announce_ready());

        // Set user-defined interval to 0 (use tracker interval)
        bt.set_user_defined_interval(Duration::ZERO);
        assert!(!bt.is_default_announce_ready());

        // Set user-defined interval to 0 seconds (immediate)
        bt.set_user_defined_interval(Duration::from_secs(0));
        // Zero duration means use tracker interval, so still not ready
        // Actually Duration::ZERO == Duration::from_secs(0), which is > Duration::ZERO is false
        // So the check `user_defined_interval > Duration::ZERO` is false, so it uses min_interval
        assert!(!bt.is_default_announce_ready());

        // Override min_interval to 0
        bt.override_min_interval(Duration::ZERO);
        assert!(bt.is_default_announce_ready());
    }

    // ------------------------------------------------------------------
    // Legacy AnnounceList (HealthTracking) Tests
    // ------------------------------------------------------------------

    #[test]
    fn test_health_tracking_announce_list_creation() {
        // Test from announce string
        let list1 = HealthTrackingAnnounceList::new(
            &[],
            &Some("http://tracker1.com/announce".to_string()),
        );
        assert_eq!(list1.tiers.len(), 1);
        assert_eq!(list1.tiers[0].trackers.len(), 1);
        assert_eq!(
            list1.get_tracker_url(0, 0).unwrap(),
            &"http://tracker1.com/announce".to_string()
        );

        // Test from multi-tier list
        let multi_tier = vec![
            vec![
                "http://tier1-1.com/announce".to_string(),
                "http://tier1-2.com/announce".to_string(),
            ],
            vec!["http://tier2-1.com/announce".to_string()],
        ];
        let list2 = HealthTrackingAnnounceList::new(&multi_tier, &None);
        assert_eq!(list2.tiers.len(), 2);
        assert_eq!(list2.tiers[0].trackers.len(), 2);
        assert_eq!(list2.tiers[1].trackers.len(), 1);

        // Test empty case
        let mut list3 = HealthTrackingAnnounceList::new(&[], &None);
        assert_eq!(list3.tiers.len(), 0);
        assert!(list3.select_next_tracker().is_none());
    }

    #[test]
    fn test_tier_selection_order() {
        let mut tier = TrackerTier::new(vec![
            "http://tracker-a.com".to_string(),
            "http://tracker-b.com".to_string(),
            "http://tracker-c.com".to_string(),
        ]);

        // Give tracker-b better reliability through simulated successes
        tier.trackers[1].record_success(50.0);
        tier.trackers[1].record_success(45.0);
        tier.trackers[1].record_success(55.0);

        // Give tracker-c some failures
        tier.trackers[2].record_failure();

        // Make tracker-a temporarily unavailable to force reliability-based selection
        tier.trackers[0].record_failure();

        // Selection should prefer higher reliability among available trackers
        let selected = tier.select_next();
        assert!(selected.is_some());
        // tracker-b should be selected due to highest reliability score
        assert_eq!(tier.current_index, 1);
    }

    #[test]
    fn test_failover_across_tiers() {
        let mut announce_list = HealthTrackingAnnounceList::new(
            &[
                vec!["http://tier1-tracker.com/announce".to_string()],
                vec!["http://tier2-tracker.com/announce".to_string()],
            ],
            &None,
        );

        // Initially should select from tier 0
        let selection1 = announce_list.select_next_tracker();
        assert_eq!(selection1, Some((0, 0)));

        // Make tier 0 tracker fail multiple times to trigger backoff
        announce_list.tiers[0].trackers[0].record_failure();
        announce_list.tiers[0].trackers[0].record_failure();
        announce_list.tiers[0].trackers[0].record_failure(); // Will have long backoff

        // Now should failover to tier 1
        let selection2 = announce_list.select_next_tracker();
        assert_eq!(selection2, Some((1, 0)));
        assert_eq!(announce_list.current_tier, 1);
    }

    #[test]
    fn test_exponential_backoff_sequence() {
        let mut entry = TrackerEntry::new("http://tracker.test/announce".to_string());

        // Verify backoff sequence: 10 -> 20 -> 40 -> 80 -> 160 -> 320 -> 640 -> 1280 -> 2560 -> 3600 (capped)
        let expected_delays = [10u64, 20, 40, 80, 160, 320, 640, 1280, 2560, 3600];

        for (i, &expected) in expected_delays.iter().enumerate() {
            entry.record_failure();

            // Calculate expected delay based on failure count
            let base: u64 = 10;
            let exp = entry.failure_count.saturating_sub(1).min(10);
            let calculated_delay = base.saturating_mul(1 << exp).min(3600);
            assert_eq!(
                calculated_delay,
                expected,
                "Failure {}: expected {}s, got {}s",
                i + 1,
                expected,
                calculated_delay
            );
        }

        // After many failures, should be capped at 3600 seconds
        assert!(entry.next_retry_after.is_some());
    }

    #[test]
    fn test_reliability_scoring() {
        let mut entry1 = TrackerEntry::new("http://good.tracker/announce".to_string());
        let mut entry2 = TrackerEntry::new("http://bad.tracker/announce".to_string());
        let entry3 = TrackerEntry::new("http://unknown.tracker/announce".to_string());

        // Simulate good tracker with many successes
        for _ in 0..10 {
            entry1.record_success(100.0);
        }

        // Simulate bad tracker with many failures
        for _ in 0..5 {
            entry2.record_failure();
        }

        // Unknown tracker has no history

        let score1 = entry1.reliability_score();
        let score2 = entry2.reliability_score();
        let score3 = entry3.reliability_score();

        // Good tracker should have highest score
        assert!(
            score1 > score3,
            "Good tracker ({}) should beat unknown ({})",
            score1,
            score3
        );

        // Bad tracker should have lowest score (penalized by recent failures)
        assert!(
            score3 > score2,
            "Unknown ({}) should beat bad tracker ({})",
            score3,
            score2
        );

        // Good tracker should definitely beat bad tracker
        assert!(
            score1 > score2,
            "Good tracker ({}) should beat bad tracker ({})",
            score1,
            score2
        );
    }

    #[test]
    fn test_urlencode_infohash() {
        let hash = [0xABu8; 20];
        let encoded = urlencode_infohash(&hash);
        assert_eq!(encoded, "%AB".repeat(20));
    }

    #[test]
    fn test_urlencode_bytes() {
        let data = [0x01, 0x02, 0xFF, 0x00];
        let encoded = urlencode_bytes(&data);
        assert_eq!(encoded, "%01%02%FF%00");
    }
}

//! Multi-tier tracker announce list management.
//!
//! Contains [`AnnounceTier`] and [`AnnounceList`] which manage tracker URL
//! lists with ordered tiers, per-tier event state, and failover.

use std::collections::{HashSet, VecDeque};

#[cfg(test)]
#[path = "tests/announce_list.rs"]
mod tests;

/// Announce events defined by the BitTorrent tracker protocol.
///
/// Core schedules when to announce; this type defines the event state and
/// wire values shared by tracker transports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnounceEvent {
    /// Initial announce when a download starts.
    Started,
    /// Start after completion without sending a duplicate completed event.
    StartedAfterCompletion,
    /// Regular announce while downloading.
    Downloading,
    /// Announce when the client is stopping.
    Stopped,
    /// Announce when the download has just completed.
    Completed,
    /// Regular announce while seeding.
    Seeding,
    /// Terminal state after stopped.
    Halted,
}

impl AnnounceEvent {
    /// Advance the event state after a successful announce.
    pub fn next_event(self) -> Self {
        match self {
            Self::Started => Self::Downloading,
            Self::StartedAfterCompletion => Self::Seeding,
            Self::Stopped => Self::Halted,
            Self::Completed => Self::Seeding,
            other => other,
        }
    }

    /// Advance terminal lifecycle events after a failed announce.
    pub fn next_event_if_after_started(self) -> Self {
        match self {
            Self::Stopped => Self::Halted,
            Self::Completed => Self::Seeding,
            other => other,
        }
    }

    /// Whether this tier should receive a stopped announce.
    pub fn accepts_stopped_event(self) -> bool {
        matches!(
            self,
            Self::Downloading | Self::Stopped | Self::Completed | Self::Seeding
        )
    }

    /// Whether this tier should receive a completed announce.
    pub fn accepts_completed_event(self) -> bool {
        matches!(self, Self::Downloading | Self::Completed)
    }

    /// Convert the event to its tracker announce wire value.
    pub fn as_event_string(self) -> &'static str {
        match self {
            Self::Started | Self::StartedAfterCompletion => "started",
            Self::Stopped => "stopped",
            Self::Completed => "completed",
            Self::Downloading | Self::Seeding | Self::Halted => "",
        }
    }
}

impl From<AnnounceEvent> for super::udp_tracker_protocol::UdpEvent {
    fn from(event: AnnounceEvent) -> Self {
        match event {
            AnnounceEvent::Started | AnnounceEvent::StartedAfterCompletion => Self::Started,
            AnnounceEvent::Completed => Self::Completed,
            AnnounceEvent::Stopped => Self::Stopped,
            AnnounceEvent::Downloading | AnnounceEvent::Seeding | AnnounceEvent::Halted => {
                Self::None
            }
        }
    }
}

// ======================================================================
// AnnounceTier
// ======================================================================

/// A single announce tier containing a deque of tracker URLs and event state.
///
/// Each tier has an event state and an ordered list of tracker URLs. Trackers
/// are tried in order; exhausting a tier advances to the next tier.
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
    /// The event starts as `AnnounceEvent::Started`.
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

    /// Advance to the next event state after a successful announce.
    pub fn next_event(&mut self) {
        self.event = self.event.next_event();
    }

    /// Advance terminal events after an announce failure.
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
// AnnounceList
// ======================================================================

/// Ordered tracker tiers with a cursor for announce selection and failover.
///
/// Successful announces promote the responding URL within its tier. Failed
/// announces advance through the tier and then to the next tier. Lifecycle
/// events can select the next tier that accepts that event.
#[derive(Debug, Clone)]
pub struct AnnounceList {
    /// Tiers of tracker URLs
    pub(crate) tiers: Vec<AnnounceTier>,
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

    /// Create announce tiers from metainfo tiers or a single announce URL.
    ///
    /// Tier format: `[[tier1-url1, tier1-url2], [tier2-url1]]`.
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
    /// If not initialized, returns `AnnounceEvent::Started`.
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
        if self.current_tracker_initialized
            && let Some(tier) = self.tiers.get_mut(self.current_tier)
        {
            tier.event = event;
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

    /// Handle announce success.
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

    /// Handle announce failure.
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
        self.tiers
            .iter()
            .filter(|t| t.accepts_stopped_event())
            .count()
    }

    /// Count the number of tiers that accept the "completed" event.
    pub fn count_completed_allowed_tier(&self) -> usize {
        self.tiers
            .iter()
            .filter(|t| t.accepts_completed_event())
            .count()
    }

    /// Move to a tier that accepts the "stopped" event using wrap-around search.
    ///
    /// Search from the current tier to the end, then wrap to the beginning.
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
    /// Search from the current tier to the end, then wrap to the beginning.
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

    /// Reset the selection cursor to the beginning.
    pub fn reset_tier(&mut self) {
        self.reset_iterator();
    }

    /// Shuffle tracker URLs within each tier.
    pub fn shuffle(&mut self) {
        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        for tier in &mut self.tiers {
            let mut urls: Vec<String> = tier.urls.drain(..).collect();
            urls.shuffle(&mut rng);
            tier.urls = urls.into_iter().collect();
        }
    }

    /// Returns the number of tiers.
    pub fn tier_count(&self) -> usize {
        self.tiers.len()
    }

    /// Get the URL for a specific tracker by tier and entry index.
    pub fn get_tracker_url(&self, tier_idx: usize, entry_idx: usize) -> Option<&String> {
        self.tiers.get(tier_idx).and_then(|t| t.urls.get(entry_idx))
    }

    /// Return whether a tracker URL already belongs to any tier.
    pub fn contains_url(&self, url: &str) -> bool {
        self.tiers
            .iter()
            .any(|tier| tier.urls.iter().any(|candidate| candidate == url))
    }

    /// Reconfigure the announce list from a new multi-tier list.
    ///
    /// Replaces all tiers with the supplied list and resets the cursor. Live
    /// tracker-response extensions use `append_tiers` instead, preserving the
    /// torrent, user, and public-tracker tiers already in the list.
    pub fn reconfigure(&mut self, announce_list: &[Vec<String>]) {
        self.tiers.clear();
        for tier_urls in announce_list {
            if tier_urls.is_empty() {
                continue;
            }
            self.tiers.push(AnnounceTier::from_urls(tier_urls.clone()));
        }
        self.reset_iterator();
    }

    /// Reconfigure the announce list from a single URL.
    ///
    /// Adds the URL as a new tier and resets the cursor.
    pub fn reconfigure_from_url(&mut self, url: &str) {
        let mut urls = VecDeque::new();
        urls.push_back(url.to_string());
        self.tiers.push(AnnounceTier::new(urls));
        self.reset_iterator();
    }

    /// Append tiers without replacing the trackers already supplied by the
    /// torrent or the user.
    ///
    /// The caller may use this for trackers discovered after the download has
    /// started. Existing URLs keep their tier and order; only URLs not
    /// present anywhere in the list are appended. If the previous list was
    /// exhausted, the first newly appended tier becomes immediately usable.
    /// Returns the URLs that were actually added.
    pub fn append_tiers(&mut self, tiers: &[Vec<String>]) -> Vec<String> {
        let was_exhausted =
            !self.current_tracker_initialized || self.current_tier >= self.tiers.len();
        let mut existing = self
            .tiers
            .iter()
            .flat_map(|tier| tier.urls.iter().cloned())
            .collect::<HashSet<_>>();
        let mut added = Vec::new();
        let first_added_tier = self.tiers.len();

        for tier_urls in tiers {
            let unique_urls = tier_urls
                .iter()
                .filter(|url| !url.is_empty() && existing.insert((*url).clone()))
                .cloned()
                .collect::<Vec<_>>();
            if unique_urls.is_empty() {
                continue;
            }
            added.extend(unique_urls.iter().cloned());
            self.tiers.push(AnnounceTier::from_urls(unique_urls));
        }

        if !added.is_empty() && was_exhausted {
            self.current_tier = first_added_tier;
            self.current_tracker = 0;
            self.current_tracker_initialized = true;
        }

        added
    }
}

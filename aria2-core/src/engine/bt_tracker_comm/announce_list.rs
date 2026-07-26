//! Multi-tier tracker announce list management.
//!
//! Contains [`AnnounceTier`] and [`AnnounceList`] which manage tracker URL
//! lists with tier-based failover, matching the C++ aria2 behavior.

use super::types::AnnounceEvent;
use std::collections::VecDeque;

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
        self.tiers.get(tier_idx).and_then(|t| t.urls.get(entry_idx))
    }
}

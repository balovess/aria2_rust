//! Health-tracking announce list with reliability scoring.
//!
//! Contains [`HealthTrackingAnnounceList`] — a Rust improvement over the
//! basic C++ [`AnnounceList`](super::announce_list::AnnounceList) that adds
//! per-tracker reliability scoring and exponential backoff.

use super::types::TrackerTier;

/// Full announce list with multiple tiers for failover support
/// using reliability-based health tracking.
///
/// This is a Rust improvement over the basic C++ AnnounceList that adds
/// per-tracker reliability scoring and exponential backoff. It is kept
/// alongside the C++-compatible [`AnnounceList`](super::announce_list::AnnounceList)
/// for use cases where the more sophisticated health tracking is desired.
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

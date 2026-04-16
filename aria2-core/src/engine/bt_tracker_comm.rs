#![allow(clippy::empty_line_after_doc_comments)]

use crate::engine::http_tracker_client::{TrackerEvent, build_tracker_client, is_https_tracker};
use crate::error::{Aria2Error, RecoverableError, Result};
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Tracker request timeout (seconds)
const TRACKER_REQUEST_TIMEOUT_SECS: u64 = 5;

/// BitTorrent tracker communication module.
///
/// Handles all interactions with:
/// - HTTP/HTTPS trackers (announce requests with event state machine)
/// - UDP trackers
/// - DHT peer discovery
/// - Public tracker lists for fallback peer discovery
///
/// Extracted from BtDownloadCommand to separate network communication
/// concerns from download orchestration logic.
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

// ======================================================================
// HTTP Tracker Communication
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
/// * `event` - Optional tracker event (started, completed, stopped, or empty)
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
        TrackerEvent::Started, // Default to Started for backward compatibility
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
        "{}?info_hash={}&peer_id={}&port=6881&uploaded={}downloaded={}left={}{}compact=1",
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
// Multi-home Tracker with Failover
// ======================================================================

/// A single tracker entry with health tracking
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

/// A tier of trackers tried in order
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
#[derive(Debug, Clone)]
pub struct AnnounceList {
    pub tiers: Vec<TrackerTier>,
    pub current_tier: usize,
}

impl AnnounceList {
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
// Tests for Multi-home Tracker
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_announce_list_creation() {
        // Test from announce string
        let list1 = AnnounceList::new(&[], &Some("http://tracker1.com/announce".to_string()));
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
        let list2 = AnnounceList::new(&multi_tier, &None);
        assert_eq!(list2.tiers.len(), 2);
        assert_eq!(list2.tiers[0].trackers.len(), 2);
        assert_eq!(list2.tiers[1].trackers.len(), 1);

        // Test empty case
        let mut list3 = AnnounceList::new(&[], &None);
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
        let mut announce_list = AnnounceList::new(
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
}

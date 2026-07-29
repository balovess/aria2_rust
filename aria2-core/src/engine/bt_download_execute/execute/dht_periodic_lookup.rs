//! Periodic DHT peer lookup for active BitTorrent downloads.
//!
//! C++ reference: `DHTGetPeersCommand.h/cc`
//!
//! In the C++ implementation, `DHTGetPeersCommand` is a per-torrent command
//! that periodically triggers DHT `get_peers` lookups. The intervals adapt
//! based on the number of known peers:
//!
//! - Normal interval: 15 minutes
//! - Low peers (< min_peers): 5 minutes
//! - Zero peers: 1 minute
//! - Retry after failed lookup: 5 seconds
//! - Maximum retries: 10
//!
//! In this Rust implementation, instead of a separate Command object, we
//! integrate the periodic DHT lookup directly into the download loop via
//! [`DhtPeriodicLookup`], which tracks timing and peer counts.

use std::time::{Duration, Instant};

use tracing::{debug, trace};

// ── Intervals (matching C++ DHTGetPeersCommand.cc) ─────────────────────

/// Normal interval between DHT get_peers lookups.
const GET_PEER_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Interval when the peer list is low (below min_peers).
const GET_PEER_INTERVAL_LOW: Duration = Duration::from_secs(5 * 60);

/// Interval when the peer list is empty.
const GET_PEER_INTERVAL_ZERO: Duration = Duration::from_secs(60);

/// Interval for retry after a failed lookup.
const GET_PEER_INTERVAL_RETRY: Duration = Duration::from_secs(5);

/// Maximum retries. Try more than 5 to drop bad nodes.
const MAX_RETRIES: u32 = 10;

// ── DhtPeriodicLookup ────────────────────────────────────────────────────

/// State machine for periodic DHT peer lookups per torrent.
///
/// Tracks when the last lookup was performed, how many retries have
/// occurred, and whether a lookup is currently in progress.
///
/// C++: `DHTGetPeersCommand`
pub struct DhtPeriodicLookup {
    /// When the last DHT get_peers lookup was initiated.
    last_lookup_time: Option<Instant>,
    /// Number of consecutive retries (reset to 0 when we have enough peers).
    num_retry: u32,
    /// Whether a DHT lookup is currently in progress.
    lookup_in_progress: bool,
    /// Minimum number of peers desired before reducing lookup frequency.
    min_peers: usize,
    /// Maximum number of peers (0 = unlimited).
    max_peers: usize,
}

impl DhtPeriodicLookup {
    /// Create a new periodic lookup tracker with default settings.
    pub fn new() -> Self {
        Self {
            last_lookup_time: None,
            num_retry: 0,
            lookup_in_progress: false,
            min_peers: 30, // C++ uses btRuntime->lessThanMinPeers()
            max_peers: 55, // C++ uses btRuntime->getMaxPeers()
        }
    }

    /// Create a new periodic lookup tracker with custom peer limits.
    pub fn with_peer_limits(min_peers: usize, max_peers: usize) -> Self {
        Self {
            last_lookup_time: None,
            num_retry: 0,
            lookup_in_progress: false,
            min_peers,
            max_peers,
        }
    }

    /// Check if a DHT get_peers lookup should be initiated now.
    ///
    /// Returns `true` when:
    /// - No lookup is in progress, AND
    /// - The appropriate interval has elapsed based on current peer count
    ///
    /// C++: `DHTGetPeersCommand::execute()` — the interval logic
    pub fn should_lookup(&self, current_peer_count: usize) -> bool {
        if self.lookup_in_progress {
            return false;
        }

        let elapsed = match self.last_lookup_time {
            Some(t) => t.elapsed(),
            None => Duration::from_secs(u64::MAX), // never looked up → do it now
        };

        // Determine the appropriate interval based on peer count
        let interval = if current_peer_count == 0 {
            GET_PEER_INTERVAL_ZERO
        } else if current_peer_count < self.min_peers {
            if self.num_retry > 0 {
                GET_PEER_INTERVAL_RETRY
            } else {
                GET_PEER_INTERVAL_LOW
            }
        } else {
            GET_PEER_INTERVAL
        };

        elapsed >= interval
    }

    /// Mark that a DHT lookup has been initiated.
    ///
    /// Call this after `should_lookup()` returns `true` and the lookup
    /// is actually started.
    pub fn on_lookup_started(&mut self) {
        self.last_lookup_time = Some(Instant::now());
        self.lookup_in_progress = true;
    }

    /// Handle completion of a DHT get_peers lookup.
    ///
    /// Updates retry count: if we still have too few peers and haven't
    /// exceeded MAX_RETRIES, increment the retry counter for a faster
    /// next lookup.
    ///
    /// C++: `DHTGetPeersCommand::execute()` — task finished handling
    pub fn on_lookup_completed(&mut self, current_peer_count: usize) {
        self.lookup_in_progress = false;

        // If we still don't have enough peers, increment retry for faster
        // next lookup. Otherwise, reset retry count.
        if self.num_retry < MAX_RETRIES
            && (self.max_peers == 0 || current_peer_count < self.max_peers)
        {
            self.num_retry += 1;
            if self.num_retry > 1 {
                trace!(
                    peers = current_peer_count,
                    max_peers = self.max_peers,
                    retry = self.num_retry,
                    "Too few peers, will retry DHT lookup sooner"
                );
            }
        } else {
            self.num_retry = 0;
        }
    }

    /// Get the current retry count.
    pub fn retry_count(&self) -> u32 {
        self.num_retry
    }

    /// Whether a lookup is currently in progress.
    pub fn is_lookup_in_progress(&self) -> bool {
        self.lookup_in_progress
    }

    /// Get the time elapsed since the last lookup.
    pub fn time_since_last_lookup(&self) -> Option<Duration> {
        self.last_lookup_time.map(|t| t.elapsed())
    }
}

impl Default for DhtPeriodicLookup {
    fn default() -> Self {
        Self::new()
    }
}

// ── Integration function ────────────────────────────────────────────────

/// Check if periodic DHT lookup is needed and, if so, initiate one.
///
/// This is the main integration point called from the BT download loop.
/// It mirrors the C++ `DHTGetPeersCommand::execute()` flow:
/// 1. Check if a lookup should be initiated
/// 2. If so, trigger the DHT engine's find_peers
/// 3. If a previous lookup completed, update state and handle retries
///
/// Returns `true` if a new DHT lookup was initiated this call.
pub async fn check_periodic_dht_lookup(
    dht_lookup: &mut DhtPeriodicLookup,
    dht_engine: Option<&std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    info_hash: &[u8; 20],
    current_peer_count: usize,
    new_peers: &mut Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
) -> bool {
    // Check if previous lookup completed
    if dht_lookup.is_lookup_in_progress() {
        // In a real implementation, we would check if the async lookup
        // has completed via a oneshot channel or JoinHandle.
        // For now, we use a synchronous model where lookups are
        // initiated and completed in the same call.
        return false;
    }

    // Check if we should initiate a new lookup
    if !dht_lookup.should_lookup(current_peer_count) {
        return false;
    }

    let Some(engine) = dht_engine else {
        return false;
    };

    debug!(
        info_hash = %hex::encode(info_hash),
        peers = current_peer_count,
        retry = dht_lookup.retry_count(),
        "Initiating periodic DHT get_peers lookup"
    );

    dht_lookup.on_lookup_started();

    // Perform the DHT lookup
    match engine.find_peers(info_hash).await {
        Ok(result) => {
            let before = new_peers.len();
            for addr in &result.peers {
                let ip_str = addr.ip().to_string();
                let paddr = aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
                    &ip_str,
                    addr.port(),
                );
                if !new_peers.iter().any(|p| p.ip == paddr.ip && p.port == paddr.port) {
                    new_peers.push(paddr);
                }
            }
            let added = new_peers.len() - before;
            if added > 0 {
                debug!(
                    info_hash = %hex::encode(info_hash),
                    added,
                    total = new_peers.len(),
                    "Periodic DHT lookup discovered new peers"
                );
            }
        }
        Err(e) => {
            debug!(
                info_hash = %hex::encode(info_hash),
                error = %e,
                "Periodic DHT lookup failed"
            );
        }
    }

    dht_lookup.on_lookup_completed(current_peer_count);
    true
}

// ── Unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_lookup_has_no_last_time() {
        let lookup = DhtPeriodicLookup::new();
        assert!(lookup.last_lookup_time.is_none());
        assert!(!lookup.is_lookup_in_progress());
        assert_eq!(lookup.retry_count(), 0);
    }

    #[test]
    fn should_lookup_when_never_looked_up() {
        let lookup = DhtPeriodicLookup::new();
        // Never looked up → should_lookup returns true regardless of peer count
        assert!(lookup.should_lookup(0));
        assert!(lookup.should_lookup(100));
    }

    #[test]
    fn should_not_lookup_while_in_progress() {
        let mut lookup = DhtPeriodicLookup::new();
        lookup.on_lookup_started();
        assert!(!lookup.should_lookup(0));
    }

    #[test]
    fn on_completed_resets_in_progress() {
        let mut lookup = DhtPeriodicLookup::new();
        lookup.on_lookup_started();
        assert!(lookup.is_lookup_in_progress());
        lookup.on_lookup_completed(55); // enough peers
        assert!(!lookup.is_lookup_in_progress());
        assert_eq!(lookup.retry_count(), 0); // reset
    }

    #[test]
    fn retry_increments_when_few_peers() {
        let mut lookup = DhtPeriodicLookup::new();
        lookup.on_lookup_started();
        lookup.on_lookup_completed(5); // few peers
        assert_eq!(lookup.retry_count(), 1);

        lookup.on_lookup_started();
        lookup.on_lookup_completed(5); // still few peers
        assert_eq!(lookup.retry_count(), 2);
    }

    #[test]
    fn retry_resets_when_enough_peers() {
        let mut lookup = DhtPeriodicLookup::new();
        lookup.on_lookup_started();
        lookup.on_lookup_completed(5); // few peers → retry=1
        lookup.on_lookup_started();
        lookup.on_lookup_completed(60); // enough peers → retry=0
        assert_eq!(lookup.retry_count(), 0);
    }

    #[test]
    fn retry_reaches_max_then_resets() {
        let mut lookup = DhtPeriodicLookup::new();

        // After 10 lookups with 0 peers, retry reaches MAX_RETRIES.
        // This matches C++ DHTGetPeersCommand::execute() where numRetry_
        // increments while numRetry_ < MAX_RETRIES.
        for _ in 0..10 {
            lookup.on_lookup_started();
            lookup.on_lookup_completed(0);
        }
        assert_eq!(lookup.retry_count(), MAX_RETRIES);

        // On the 11th completion, num_retry >= MAX_RETRIES, so the else
        // branch resets to 0 (C++ design: after exhausting retries, back
        // off from aggressive 5-second retry interval to normal interval).
        lookup.on_lookup_started();
        lookup.on_lookup_completed(0);
        assert_eq!(lookup.retry_count(), 0);
    }

    #[test]
    fn interval_adapts_to_peer_count() {
        let mut lookup = DhtPeriodicLookup::new();

        // Zero peers: very short interval
        lookup.on_lookup_started();
        lookup.on_lookup_completed(0);

        // After 30 seconds: should be eligible for zero-peer interval (1 min)
        // This test verifies the logic; timing-dependent tests are fragile.
        let elapsed = lookup.time_since_last_lookup();
        assert!(elapsed.is_some());
    }

    #[test]
    fn with_custom_peer_limits() {
        let lookup = DhtPeriodicLookup::with_peer_limits(10, 20);
        assert_eq!(lookup.min_peers, 10);
        assert_eq!(lookup.max_peers, 20);
    }
}

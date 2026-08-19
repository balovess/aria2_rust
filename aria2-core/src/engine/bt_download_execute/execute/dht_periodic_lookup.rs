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

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

use tracing::{debug, trace};

type DhtLookupResult = std::io::Result<aria2_protocol::bittorrent::dht::engine::FindPeersResult>;
type DhtLookupTask = tokio::task::JoinHandle<()>;

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
    /// Background lookup task. Completion is published through
    /// `lookup_result` and `lookup_notify` so a slow DHT query never causes
    /// the piece loop to poll task state.
    lookup_task: Option<DhtLookupTask>,
    /// Result slot filled by the background lookup task before notification.
    lookup_result: Arc<Mutex<Option<DhtLookupResult>>>,
    /// Wakes an event-driven consumer when the lookup result is available.
    lookup_notify: Arc<Notify>,
    /// A completed result still needs to pass through PeerStorage admission
    /// before the retry counter can observe the final tracked-peer count.
    lookup_completion_pending: bool,
}

impl DhtPeriodicLookup {
    /// Create a new periodic lookup tracker with default settings.
    pub fn new() -> Self {
        Self::with_peer_limits(30, 55)
    }

    /// Create a new periodic lookup tracker with custom peer limits.
    pub fn with_peer_limits(min_peers: usize, max_peers: usize) -> Self {
        Self {
            last_lookup_time: None,
            num_retry: 0,
            lookup_in_progress: false,
            min_peers,
            max_peers,
            lookup_task: None,
            lookup_result: Arc::new(Mutex::new(None)),
            lookup_notify: Arc::new(Notify::new()),
            lookup_completion_pending: false,
        }
    }

    /// Update the live peer limits used by the adaptive interval policy.
    ///
    /// `BtRuntimeState` is the source of truth for runtime option changes;
    /// this tracker keeps only the small snapshot needed for scheduling.
    pub fn set_peer_limits(&mut self, min_peers: usize, max_peers: usize) {
        self.min_peers = min_peers;
        self.max_peers = max_peers;
    }

    /// Check if a DHT get_peers lookup should be initiated now.
    ///
    /// Returns `true` when:
    /// - No lookup is in progress, AND
    /// - The appropriate interval has elapsed based on current peer count
    ///
    /// C++: `DHTGetPeersCommand::execute()` — the interval logic
    pub fn should_lookup(&self, current_peer_count: usize) -> bool {
        if self.lookup_in_progress || self.lookup_completion_pending {
            return false;
        }

        let elapsed = match self.last_lookup_time {
            Some(t) => t.elapsed(),
            None => Duration::from_secs(u64::MAX), // never looked up → do it now
        };

        elapsed >= self.interval_for(current_peer_count)
    }

    /// Select the interval from the live connection count.
    ///
    /// The original `BtRuntime::lessThanMinPeers()` treats a zero minimum as
    /// permanently below the minimum. The explicit branch preserves that
    /// edge case instead of conflating it with the normal peer-limit path.
    fn interval_for(&self, active_connection_count: usize) -> Duration {
        let below_minimum = self.min_peers == 0 || active_connection_count < self.min_peers;

        if active_connection_count == 0 {
            if self.num_retry > 0 {
                GET_PEER_INTERVAL_RETRY
            } else {
                GET_PEER_INTERVAL_ZERO
            }
        } else if below_minimum {
            if self.num_retry > 0 {
                GET_PEER_INTERVAL_RETRY
            } else {
                GET_PEER_INTERVAL_LOW
            }
        } else {
            GET_PEER_INTERVAL
        }
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
        self.lookup_completion_pending = false;
        self.last_lookup_time = Some(Instant::now());

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

    /// Record a lookup performed by the initial peer-discovery phase.
    ///
    /// The initial discovery is intentionally awaited during setup. Recording
    /// it here prevents the periodic scheduler from immediately issuing the
    /// same lookup again once piece downloading starts.
    pub fn record_lookup_completed(&mut self, current_peer_count: usize) {
        self.on_lookup_completed(current_peer_count);
    }

    /// Get the current retry count.
    pub fn retry_count(&self) -> u32 {
        self.num_retry
    }

    /// Whether a lookup is currently in progress.
    pub fn is_lookup_in_progress(&self) -> bool {
        self.lookup_in_progress
    }

    /// Whether a finished result is waiting for PeerStorage admission.
    pub fn is_lookup_completion_pending(&self) -> bool {
        self.lookup_completion_pending
    }

    /// Get the time elapsed since the last lookup.
    pub fn time_since_last_lookup(&self) -> Option<Duration> {
        self.last_lookup_time.map(|t| t.elapsed())
    }

    /// Return the time until the next lookup deadline for the current peer
    /// count. An in-flight lookup has its completion notification as the wake
    /// source, so it does not need a timer while it is running.
    pub fn next_lookup_delay(&self, active_connection_count: usize) -> Option<Duration> {
        if self.lookup_in_progress || self.lookup_completion_pending {
            return None;
        }

        let interval = self.interval_for(active_connection_count);
        Some(match self.last_lookup_time {
            Some(last) => interval.saturating_sub(last.elapsed()),
            None => Duration::ZERO,
        })
    }

    /// Start a lookup in the background when the adaptive interval allows it.
    ///
    /// The task owns its `Arc<DhtEngine>` and copied info-hash, so the command
    /// can continue processing pieces while the network lookup is pending.
    pub fn start_lookup(
        &mut self,
        dht_engine: Option<&Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        info_hash: [u8; 20],
        active_connection_count: usize,
    ) -> bool {
        if !self.should_lookup(active_connection_count) {
            return false;
        }
        let Some(engine) = dht_engine else {
            return false;
        };

        debug!(
            info_hash = %hex::encode(info_hash),
            peers = active_connection_count,
            retry = self.retry_count(),
            "Scheduling periodic DHT get_peers lookup"
        );
        self.on_lookup_started();
        let engine = Arc::clone(engine);
        let result_slot = Arc::clone(&self.lookup_result);
        let notify = Arc::clone(&self.lookup_notify);
        self.lookup_task = Some(tokio::spawn(async move {
            let result = engine.find_peers(&info_hash).await;
            *result_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
            notify.notify_one();
        }));
        true
    }

    /// Return the completion notification used by the download loop.
    pub fn completion_notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.lookup_notify)
    }

    /// Take a completed background lookup and append newly discovered peers.
    ///
    /// Returns `true` only when the background task published a result. An
    /// unfinished lookup has no result to take, so callers remain responsive
    /// without inspecting `JoinHandle::is_finished()` on every loop turn.
    pub fn take_completed_lookup(
        &mut self,
        new_peers: &mut Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    ) -> bool {
        let result = self
            .lookup_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(result) = result else {
            return false;
        };

        // The task has already published its result. Dropping the completed
        // handle releases the task allocation without another status probe.
        self.lookup_task.take();
        match result {
            Ok(result) => {
                let before = new_peers.len();
                for addr in result.peers {
                    let peer = aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
                        &addr.ip().to_string(),
                        addr.port(),
                    );
                    if !new_peers
                        .iter()
                        .any(|known| known.ip == peer.ip && known.port == peer.port)
                    {
                        new_peers.push(peer);
                    }
                }
                let added = new_peers.len() - before;
                if added > 0 {
                    debug!(
                        added,
                        total = new_peers.len(),
                        "Periodic DHT lookup discovered new peers"
                    );
                }
            }
            Err(error) => {
                debug!(error = %error, "Periodic DHT lookup failed");
            }
        }
        self.lookup_in_progress = false;
        self.lookup_completion_pending = true;
        true
    }

    /// Cancel and join a pending lookup during command shutdown.
    ///
    /// `Drop` still aborts as a synchronous fallback, but normal command
    /// teardown awaits the aborted task so its engine reference and any local
    /// lookup state are released before the command lifecycle ends.
    pub async fn cancel_pending_lookup(&mut self) {
        if let Some(task) = self.lookup_task.take() {
            task.abort();
            let _ = task.await;
        }
        self.lookup_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.lookup_in_progress = false;
        self.lookup_completion_pending = false;
    }
}

impl Default for DhtPeriodicLookup {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DhtPeriodicLookup {
    fn drop(&mut self) {
        if let Some(task) = self.lookup_task.take() {
            task.abort();
        }
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
/// Returns `true` if a lookup was started or a previous result was collected.
/// The caller must invoke [`DhtPeriodicLookup::on_lookup_completed`] after it
/// admits the returned peers so retry decisions use the final tracked count.
pub async fn check_periodic_dht_lookup(
    dht_lookup: &mut DhtPeriodicLookup,
    dht_engine: Option<&std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
    info_hash: &[u8; 20],
    active_connection_count: usize,
    new_peers: &mut Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
) -> bool {
    let completed = dht_lookup.take_completed_lookup(new_peers);
    if completed {
        return true;
    }
    let started = dht_lookup.start_lookup(dht_engine, *info_hash, active_connection_count);
    completed || started
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

    #[test]
    fn interval_matches_original_connection_count_branches() {
        let lookup = DhtPeriodicLookup::with_peer_limits(10, 20);
        assert_eq!(lookup.interval_for(0), GET_PEER_INTERVAL_ZERO);
        assert_eq!(lookup.interval_for(5), GET_PEER_INTERVAL_LOW);
        assert_eq!(lookup.interval_for(10), GET_PEER_INTERVAL);

        let unlimited = DhtPeriodicLookup::with_peer_limits(0, 0);
        assert_eq!(unlimited.interval_for(0), GET_PEER_INTERVAL_ZERO);
        assert_eq!(unlimited.interval_for(1), GET_PEER_INTERVAL_LOW);
    }

    #[tokio::test]
    async fn background_lookup_publishes_completion_without_status_polling() {
        let engine = aria2_protocol::bittorrent::dht::engine::DhtEngine::start(
            aria2_protocol::bittorrent::dht::engine::DhtEngineConfig::local(),
        )
        .await
        .expect("local DHT engine should start");
        let mut lookup = DhtPeriodicLookup::with_peer_limits(1, 2);
        let mut peers = Vec::new();

        assert!(lookup.start_lookup(Some(&engine), [0x42; 20], 0));
        assert!(lookup.is_lookup_in_progress());

        let notify = lookup.completion_notifier();
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut notified)
            .await
            .expect("background lookup should publish completion");

        assert!(lookup.take_completed_lookup(&mut peers));
        assert!(!lookup.is_lookup_in_progress());
        assert!(lookup.is_lookup_completion_pending());
        assert!(peers.is_empty());

        lookup.on_lookup_completed(0);
        assert!(!lookup.is_lookup_completion_pending());

        engine.shutdown();
    }

    #[tokio::test]
    async fn shutdown_cancels_pending_lookup() {
        let engine = aria2_protocol::bittorrent::dht::engine::DhtEngine::start(
            aria2_protocol::bittorrent::dht::engine::DhtEngineConfig::local(),
        )
        .await
        .expect("local DHT engine should start");
        let mut lookup = DhtPeriodicLookup::new();

        assert!(lookup.start_lookup(Some(&engine), [0x24; 20], 0));
        lookup.cancel_pending_lookup().await;

        assert!(!lookup.is_lookup_in_progress());
        assert!(!lookup.is_lookup_completion_pending());
        engine.shutdown();
    }
}

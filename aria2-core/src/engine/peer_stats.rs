//! Peer statistics tracking with sliding window speed calculation.
//!
//! This module provides [`PeerStats`] for tracking per-peer metrics including
//! upload/download byte counts, speed calculations using Exponential Moving Average (EMA),
//! and choke/interested state management for BT choking algorithm implementation.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// EMA smoothing factor (alpha).
///
/// Controls responsiveness vs. smoothness of speed estimates.
/// 0.5 provides balanced behavior: responsive to changes while filtering noise.
const EMA_ALPHA: f64 = 0.5;

/// Per-peer statistics for BitTorrent choking algorithm decisions.
///
/// Tracks cumulative byte counts, real-time speeds via EMA, choke/interested states,
/// and timestamps for snubbed detection and unchoke rotation eligibility.
pub struct PeerStats {
    /// 20-byte peer identifier from the BitTorrent handshake.
    pub peer_id: [u8; 20],

    /// Network address of this peer.
    pub addr: SocketAddr,

    // ------------------------------------------------------------------
    // Cumulative byte counts
    // ------------------------------------------------------------------
    /// Total bytes uploaded to this peer (cumulative).
    pub uploaded_bytes: u64,

    /// Total bytes downloaded from this peer (cumulative).
    pub downloaded_bytes: u64,

    // ------------------------------------------------------------------
    // Speed estimates (bytes/sec), updated via EMA
    // ------------------------------------------------------------------
    /// Current upload speed estimate in bytes/second.
    pub upload_speed: f64,

    /// Current download speed estimate in bytes/second.
    pub download_speed: f64,

    // ------------------------------------------------------------------
    // Choke / Interested state (per BEP-0003)
    // ------------------------------------------------------------------
    /// Whether *we* are choking this peer.
    ///
    /// Starts as `true` (we choke all peers by default).
    pub am_choking: bool,

    /// Whether *we* are interested in data from this peer.
    pub am_interested: bool,

    /// Whether *this peer* is choking us.
    pub peer_choking: bool,

    /// Whether *this peer* is interested in data from us.
    pub peer_interested: bool,

    /// Whether this peer has been marked as snubbed (not sending data).
    pub is_snubbed: bool,

    // ------------------------------------------------------------------
    // Timestamps for speed calculation & snubbed detection
    // ------------------------------------------------------------------
    /// Instant of the most recent message received from this peer.
    pub last_message_received_at: Instant,

    /// Instant when we last unchoked this peer (for rotation round-robin).
    pub last_unchoke_at: Instant,

    /// Instant when we last optimistically unchoked this peer.
    pub last_optimistic_unchoke_at: Instant,

    /// When this `PeerStats` was created.
    created_at: Instant,

    // ------------------------------------------------------------------
    // Internal: previous timestamp for EMA speed calculation
    // ------------------------------------------------------------------
    /// Last time `on_data_sent` was called (for upload speed EMA).
    last_upload_tick: Instant,

    /// Last time `on_data_received` was called (for download speed EMA).
    last_download_tick: Instant,
}

impl PeerStats {
    /// Create a new `PeerStats` for the given peer.
    ///
    /// # Default state
    ///
    /// - Byte counters start at 0.
    /// - Speeds start at 0.0.
    /// - `am_choking = true` (we choke by default).
    /// - All other boolean flags are `false`.
    /// - All timestamps are set to `Instant::now()`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use std::net::SocketAddr;
    /// let addr: SocketAddr = "192.168.1.5:6881".parse().unwrap();
    /// let stats = PeerStats::new([0u8; 20], addr);
    /// assert!(stats.am_choking);
    /// assert_eq!(stats.uploaded_bytes, 0);
    /// ```
    pub fn new(peer_id: [u8; 20], addr: SocketAddr) -> Self {
        let now = Instant::now();
        Self {
            peer_id,
            addr,
            uploaded_bytes: 0,
            downloaded_bytes: 0,
            upload_speed: 0.0,
            download_speed: 0.0,
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            is_snubbed: false,
            last_message_received_at: now,
            last_unchoke_at: now,
            last_optimistic_unchoke_at: now,
            created_at: now,
            last_upload_tick: now,
            last_download_tick: now,
        }
    }

    // ------------------------------------------------------------------
    // Data event handlers (update counters + EMA speeds)
    // ------------------------------------------------------------------

    /// Record that we sent `bytes` to this peer.
    ///
    /// Increments [`uploaded_bytes`](Self::uploaded_bytes) and updates
    /// [`upload_speed`](Self::upload_speed) using an Exponential Moving Average:
    ///
    /// ```text
    /// new_speed = alpha * instant_rate + (1 - alpha) * old_speed
    /// ```
    ///
    /// where `alpha = 0.5`. On the **first** call the raw instant rate is used directly.
    pub fn on_data_sent(&mut self, bytes: u64) {
        self.uploaded_bytes += bytes;

        let now = Instant::now();
        let elapsed = now - self.last_upload_tick;
        self.last_upload_tick = now;

        if elapsed.is_zero() {
            return; // avoid division-by-zero; speed unchanged
        }

        let instant_rate = bytes as f64 / elapsed.as_secs_f64();

        if self.upload_speed == 0.0 && self.uploaded_bytes == bytes {
            // First measurement: use raw rate
            self.upload_speed = instant_rate;
        } else {
            // EMA update
            self.upload_speed = EMA_ALPHA * instant_rate + (1.0 - EMA_ALPHA) * self.upload_speed;
        }
    }

    /// Record that we received `bytes` from this peer.
    ///
    /// Increments [`downloaded_bytes`](Self::downloaded_bytes),
    /// resets [`is_snubbed`](Self::is_snubbed) to `false`,
    /// updates [`last_message_received_at`](Self::last_message_received_at),
    /// and refreshes [`download_speed`](Self::download_speed) via EMA.
    pub fn on_data_received(&mut self, bytes: u64) {
        self.downloaded_bytes += bytes;
        self.last_message_received_at = Instant::now();
        self.is_snubbed = false;

        let now = Instant::now();
        let elapsed = now - self.last_download_tick;
        self.last_download_tick = now;

        if elapsed.is_zero() {
            return;
        }

        let instant_rate = bytes as f64 / elapsed.as_secs_f64();

        if self.download_speed == 0.0 && self.downloaded_bytes == bytes {
            // First measurement: use raw rate
            self.download_speed = instant_rate;
        } else {
            // EMA update
            self.download_speed =
                EMA_ALPHA * instant_rate + (1.0 - EMA_ALPHA) * self.download_speed;
        }
    }

    // ------------------------------------------------------------------
    // Snubbed detection
    // ------------------------------------------------------------------

    /// Check whether this peer should be marked as snubbed due to inactivity.
    ///
    /// Returns `true` if the peer has **just** transitioned into the snubbed state
    /// (i.e., no data for at least `timeout_secs` seconds and was not already snubbed).
    ///
    /// Returns `false` if the peer is still active or was already snubbed.
    pub fn check_snubbed(&mut self, timeout_secs: u64) -> bool {
        if self.last_message_received_at.elapsed().as_secs() >= timeout_secs && !self.is_snubbed {
            self.is_snubbed = true;
            return true;
        }
        false
    }

    /// Explicitly reset the snubbed flag (e.g. after an unchoke).
    pub fn reset_snubbed(&mut self) {
        self.is_snubbed = false;
    }

    // ------------------------------------------------------------------
    // Choke / Unchoke bookkeeping
    // ------------------------------------------------------------------

    /// Record that we have **unchoked** this peer.
    ///
    /// Sets [`am_choking`](Self::am_choking) to `false` and refreshes
    /// [`last_unchoke_at`](Self::last_unchoke_at).
    pub fn record_unchoke(&mut self) {
        self.am_choking = false;
        self.last_unchoke_at = Instant::now();
    }

    /// Record that we have **choked** this peer.
    ///
    /// Sets [`am_choking`](Self::am_choking) to `true`.
    pub fn record_choke(&mut self) {
        self.am_choking = true;
    }

    /// Record that we performed an **optimistic unchoke** on this peer.
    ///
    /// Sets [`am_choking`](Self::am_choking) to `false` and refreshes
    /// [`last_optimistic_unchoke_at`](Self::last_optimistic_unchoke_at).
    pub fn record_optimistic_unchoke(&mut self) {
        self.am_choking = false;
        self.last_optimistic_unchoke_at = Instant::now();
    }

    // ------------------------------------------------------------------
    // Time-since helpers for rotation logic
    // ------------------------------------------------------------------

    /// Elapsed time since we last unchoked this peer (regular unchoke).
    ///
    /// Used by the choking algorithm to determine rotation eligibility
    /// (peers that have been unchoked longest are candidates for choking).
    pub fn time_since_last_unchoke(&self) -> Duration {
        self.last_unchoke_at.elapsed()
    }

    /// Elapsed time since we last optimistically unchoked this peer.
    ///
    /// Used to avoid re-selecting the same peer for optimistic unchoke
    /// too frequently.
    pub fn time_since_last_optimistic_unchoke(&self) -> Duration {
        self.last_optimistic_unchoke_at.elapsed()
    }

    /// Elapsed time since this `PeerStats` was created.
    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    fn make_test_peer() -> PeerStats {
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        PeerStats::new([0x42; 20], addr)
    }

    #[test]
    fn test_new_peer_stats() {
        let stats = make_test_peer();

        // Byte counters should be zero
        assert_eq!(stats.uploaded_bytes, 0);
        assert_eq!(stats.downloaded_bytes, 0);

        // Speeds should be zero
        assert_eq!(stats.upload_speed, 0.0);
        assert_eq!(stats.download_speed, 0.0);

        // Default choke state: we choke the peer by default
        assert!(stats.am_choking);
        assert!(!stats.am_interested);

        // Peer default states
        assert!(stats.peer_choking); // peer chokes us initially
        assert!(!stats.peer_interested);

        // Not snubbed initially
        assert!(!stats.is_snubbed);

        // Peer ID preserved
        assert_eq!(stats.peer_id, [0x42; 20]);
    }

    #[test]
    fn test_on_data_sent_updates_counters() {
        let mut stats = make_test_peer();

        // Small sleep so elapsed > 0
        thread::sleep(Duration::from_millis(10));

        stats.on_data_sent(1024);

        assert_eq!(stats.uploaded_bytes, 1024);
        assert!(
            stats.upload_speed > 0.0,
            "upload_speed should be positive after sending data"
        );

        // Send more data
        thread::sleep(Duration::from_millis(10));
        stats.on_data_sent(2048);

        assert_eq!(stats.uploaded_bytes, 1024 + 2048);
        assert!(stats.upload_speed > 0.0);
    }

    #[test]
    fn test_on_data_received_resets_snubbed() {
        let mut stats = make_test_peer();

        // Mark as snubbed manually
        stats.is_snubbed = true;
        assert!(stats.is_snubbed);

        // Receive data -- should reset snubbed flag
        thread::sleep(Duration::from_millis(10));
        stats.on_data_received(512);

        assert!(
            !stats.is_snubbed,
            "receiving data should reset snubbed status"
        );
        assert_eq!(stats.downloaded_bytes, 512);
        assert!(stats.download_speed > 0.0);
    }

    #[test]
    fn test_check_snubbed_timeout() {
        let mut stats = make_test_peer();

        // Immediately after creation, should NOT be snubbed with a reasonable timeout
        let result = stats.check_snubbed(10);
        assert!(!result, "should not be snubbed immediately");
        assert!(!stats.is_snubbed);

        // Use timeout=0 to guarantee it triggers (elapsed >= 0 always true)
        let result = stats.check_snubbed(0);
        assert!(
            result,
            "with timeout=0, any elapsed time should trigger snubbed"
        );
        assert!(stats.is_snubbed);

        // Calling again should return false (already snubbed)
        let result2 = stats.check_snubbed(0);
        assert!(
            !result2,
            "second call should return false (already snubbed)"
        );
    }

    #[test]
    fn test_choke_state_transitions() {
        let mut stats = make_test_peer();

        // Initial state: we are choking
        assert!(stats.am_choking);

        // Unchoke
        stats.record_unchoke();
        assert!(!stats.am_choking);

        // Verify timestamp updated
        let unchoke_time = stats.time_since_last_unchoke();
        assert!(unchoke_time < Duration::from_millis(100));

        // Re-choke
        stats.record_choke();
        assert!(stats.am_choking);

        // Optimistic unchoke
        stats.record_optimistic_unchoke();
        assert!(!stats.am_choking);

        let opt_time = stats.time_since_last_optimistic_unchoke();
        assert!(opt_time < Duration::from_millis(100));
    }

    #[test]
    fn test_ema_speed_smoothing() {
        let mut stats = make_test_peer();

        // First measurement: raw rate
        thread::sleep(Duration::from_millis(20));
        stats.on_data_received(1000);

        let first_speed = stats.download_speed;
        assert!(first_speed > 0.0);

        // Second measurement at similar interval: EMA should produce
        // a value close to first_speed (smoothed)
        thread::sleep(Duration::from_millis(20));
        stats.on_data_received(1000);

        let second_speed = stats.download_speed;
        // With alpha=0.5, second_speed ~= 0.5*rate2 + 0.5*first_speed
        // Since rate1 ~= rate2 (same bytes, same interval), second_speed ~= first_speed
        let ratio = second_speed / first_speed;
        assert!(
            ratio > 0.3 && ratio < 3.0,
            "EMA should smooth speeds reasonably (ratio={:.2})",
            ratio
        );
    }

    #[test]
    fn test_cumulative_byte_counts() {
        let mut stats = make_test_peer();

        for _ in 0..5 {
            thread::sleep(Duration::from_millis(5));
            stats.on_data_sent(1024);
            thread::sleep(Duration::from_millis(5));
            stats.on_data_received(2048);
        }

        assert_eq!(stats.uploaded_bytes, 5 * 1024);
        assert_eq!(stats.downloaded_bytes, 5 * 2048);
    }

    #[test]
    fn test_reset_snubbed_explicit() {
        let mut stats = make_test_peer();

        stats.is_snubbed = true;
        stats.reset_snubbed();
        assert!(!stats.is_snubbed);
    }
}

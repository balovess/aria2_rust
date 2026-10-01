//! Peer statistics tracking with sliding window speed calculation.
//!
//! This module provides [`PeerStats`] for tracking per-peer metrics including
//! upload/download byte counts, EMA speed estimates and bounded rolling speed
//! windows for choke ranking,
//! choke/interested state management for BT choking algorithm implementation,
//! and bad peer detection/banning system for handling peers that send invalid data.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::constants;

/// EMA smoothing factor (alpha).
///
/// Controls responsiveness vs. smoothness of speed estimates.
/// 0.5 provides balanced behavior: responsive to changes while filtering noise.
const EMA_ALPHA: f64 = constants::PEER_STATS_EMA_ALPHA;
pub(crate) const PEER_RATE_WINDOW: Duration = Duration::from_secs(10);
pub(crate) const PEER_RATE_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Threshold for banning peers that send too many invalid pieces.
///
/// When a peer's `bad_data_count` reaches this value, they are permanently
/// banned for the remainder of the session.
pub const BAD_DATA_THRESHOLD: u32 = constants::PEER_STATS_BAD_DATA_THRESHOLD as u32;

/// Bounded byte-rate samples used to match aria2's `SpeedCalc` choke ranking.
#[derive(Clone, Default)]
struct PeerSpeedWindow {
    samples: VecDeque<(Instant, u64)>,
}

impl PeerSpeedWindow {
    fn speed_at(&self, now: Instant) -> u64 {
        let (bytes_in_window, oldest_sample) = self.totals_at(now);
        let Some(oldest_sample) = oldest_sample else {
            return 0;
        };
        let elapsed_millis = now
            .saturating_duration_since(oldest_sample)
            .as_millis()
            .max(1);
        ((u128::from(bytes_in_window) * 1000) / elapsed_millis).min(u128::from(u64::MAX)) as u64
    }

    fn totals_at(&self, now: Instant) -> (u64, Option<Instant>) {
        let mut bytes_in_window = 0u64;
        let mut oldest_sample = None;
        for (sample_time, bytes) in &self.samples {
            if now.saturating_duration_since(*sample_time) <= PEER_RATE_WINDOW {
                bytes_in_window = bytes_in_window.saturating_add(*bytes);
                oldest_sample.get_or_insert(*sample_time);
            }
        }
        (bytes_in_window, oldest_sample)
    }

    fn record(&mut self, bytes: u64, at: Instant) {
        if bytes == 0 {
            return;
        }
        self.remove_expired(at);
        if let Some((last_sample, last_bytes)) = self.samples.back_mut()
            && at.saturating_duration_since(*last_sample) < PEER_RATE_SAMPLE_INTERVAL
        {
            *last_bytes = last_bytes.saturating_add(bytes);
        } else {
            self.samples.push_back((at, bytes));
        }
    }

    fn next_expiration_after(&self, now: Instant) -> Option<Instant> {
        self.samples.iter().find_map(|(sample_time, _)| {
            sample_time
                .checked_add(PEER_RATE_WINDOW)?
                .checked_add(Duration::from_nanos(1))
                .filter(|deadline| *deadline > now)
        })
    }

    fn remove_expired(&mut self, now: Instant) {
        while let Some((sample_time, _)) = self.samples.front().copied() {
            if now.saturating_duration_since(sample_time) <= PEER_RATE_WINDOW {
                break;
            }
            self.samples.pop_front();
        }
    }
}

/// Per-peer statistics for BitTorrent choking algorithm decisions.
///
/// Tracks cumulative byte counts, real-time speeds via EMA, choke/interested states,
/// timestamps for snubbed detection and unchoke rotation eligibility,
/// and bad data detection for peer banning system.
#[derive(Clone)]
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
    // Speed estimates (bytes/sec); these public estimates remain EMA-based.
    // ------------------------------------------------------------------
    /// Current upload speed estimate in bytes/second.
    pub upload_speed: f64,

    /// Current download speed estimate in bytes/second.
    pub download_speed: f64,

    /// Average upload speed over the entire connection (bytes/sec).
    pub avg_upload_speed: u64,

    /// Average download speed over the entire connection (bytes/sec).
    pub avg_download_speed: u64,

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

    /// Whether this peer is being optimistically unchoked.
    ///
    /// Used by the seeder-state and leecher-state choking algorithms to
    /// track which peer received the optimistic unchoke slot. Mirrors
    /// C++ `Peer::optUnchoking()`.
    pub opt_unchoking: bool,

    /// Number of outstanding (in-flight) upload requests from this peer.
    ///
    /// In the seeder-state choking algorithm, peers with outstanding uploads
    /// receive the highest ranking priority. Mirrors C++
    /// `Peer::countOutstandingUpload()`.
    pub outstanding_upload_count: usize,

    // ------------------------------------------------------------------
    // Bad data tracking (for ban system)
    // ------------------------------------------------------------------
    /// Number of times this peer sent invalid piece data (hash verification failed).
    ///
    /// When this reaches [`BAD_DATA_THRESHOLD`], the peer is permanently banned.
    pub bad_data_count: u32,

    /// Number of times this peer has been marked as snubbed.
    pub snub_count: u32,

    /// Whether this peer has been banned for sending too much invalid data.
    ///
    /// Banned peers are disconnected, excluded from selection, and not reconnected
    /// for the remainder of the session.
    pub is_banned: bool,

    /// Reason why this peer was banned (if `is_banned == true`).
    pub ban_reason: Option<String>,

    // ------------------------------------------------------------------
    // Timestamps for speed calculation & snubbed detection
    // ------------------------------------------------------------------
    /// Instant of the most recent message received from this peer.
    pub last_message_received_at: Instant,

    /// Instant of the most recent data received FROM this peer.
    pub last_data_time: Option<Instant>,

    /// Instant of the most recent data sent TO this peer.
    pub last_upload_time: Option<Instant>,

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

    /// Recent sent bytes used by seeder-state choke ranking.
    upload_rate_window: PeerSpeedWindow,
    /// Recent received bytes used by leecher-state choke ranking.
    download_rate_window: PeerSpeedWindow,
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
    /// - Bad data count starts at 0.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use std::net::SocketAddr;
    /// let addr: SocketAddr = "192.168.1.5:6881".parse().unwrap();
    /// let stats = PeerStats::new([0u8; 20], addr);
    /// assert!(stats.am_choking);
    /// assert_eq!(stats.uploaded_bytes, 0);
    /// assert!(!stats.is_banned);
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
            avg_upload_speed: 0,
            avg_download_speed: 0,
            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            is_snubbed: false,
            opt_unchoking: false,
            outstanding_upload_count: 0,
            bad_data_count: 0,
            snub_count: 0,
            is_banned: false,
            ban_reason: None,
            last_message_received_at: now,
            last_data_time: None,
            last_upload_time: None,
            last_unchoke_at: now,
            last_optimistic_unchoke_at: now,
            created_at: now,
            last_upload_tick: now,
            last_download_tick: now,
            upload_rate_window: PeerSpeedWindow::default(),
            download_rate_window: PeerSpeedWindow::default(),
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
    /// Also updates [`last_upload_time`](Self::last_upload_time).
    pub fn on_data_sent(&mut self, bytes: u64) {
        self.uploaded_bytes += bytes;
        let now = Instant::now();
        self.last_upload_time = Some(now);
        self.upload_rate_window.record(bytes, now);

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

        // Update average upload speed (cumulative)
        let total_elapsed = self.created_at.elapsed().as_secs_f64();
        if total_elapsed > 0.0 {
            self.avg_upload_speed = (self.uploaded_bytes as f64 / total_elapsed) as u64;
        }
    }

    /// Return upload throughput over the same rolling window used by aria2's
    /// `SpeedCalc::calculateSpeed`, for seeder-state choke ranking.
    pub(crate) fn recent_upload_speed_at(&self, now: Instant) -> u64 {
        self.upload_rate_window.speed_at(now)
    }

    pub(crate) fn recent_upload_window_at(&self, now: Instant) -> (u64, Option<Instant>) {
        self.upload_rate_window.totals_at(now)
    }

    pub(crate) fn next_upload_speed_deadline(&self, now: Instant) -> Option<Instant> {
        self.upload_rate_window.next_expiration_after(now)
    }

    /// Return download throughput over the same rolling window used by aria2's
    /// `SpeedCalc::calculateSpeed`, for leecher-state choke ranking.
    pub(crate) fn recent_download_speed_at(&self, now: Instant) -> u64 {
        self.download_rate_window.speed_at(now)
    }

    #[cfg(test)]
    pub(crate) fn record_download_rate_at(&mut self, bytes: u64, at: Instant) {
        self.download_rate_window.record(bytes, at);
    }

    #[cfg(test)]
    pub(crate) fn record_upload_rate_at(&mut self, bytes: u64, at: Instant) {
        self.upload_rate_window.record(bytes, at);
    }

    /// Record that we received `bytes` from this peer.
    ///
    /// Increments [`downloaded_bytes`](Self::downloaded_bytes),
    /// resets [`is_snubbed`](Self::is_snubbed) to `false`,
    /// updates [`last_message_received_at`](Self::last_message_received_at),
    /// updates [`last_data_time`](Self::last_data_time),
    /// and refreshes [`download_speed`](Self::download_speed) via EMA.
    pub fn on_data_received(&mut self, bytes: u64) {
        self.downloaded_bytes += bytes;
        let now = Instant::now();
        self.download_rate_window.record(bytes, now);
        self.last_message_received_at = now;
        self.last_data_time = Some(now);
        self.is_snubbed = false;

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

        // Update average download speed (cumulative)
        let total_elapsed = self.created_at.elapsed().as_secs_f64();
        if total_elapsed > 0.0 {
            self.avg_download_speed = (self.downloaded_bytes as f64 / total_elapsed) as u64;
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
    /// Also increments [`snub_count`](Self::snub_count) when transitioning to snubbed state.
    pub fn check_snubbed(&mut self, timeout_secs: u64) -> bool {
        if self.last_message_received_at.elapsed().as_secs() >= timeout_secs && !self.is_snubbed {
            self.is_snubbed = true;
            self.snub_count = self.snub_count.saturating_add(1);
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

    /// Get the total duration of this peer connection in seconds.
    ///
    /// Returns the number of seconds since this peer was first connected.
    pub fn connection_duration_secs(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }

    // ------------------------------------------------------------------
    // Bad data tracking / Ban system
    // ------------------------------------------------------------------

    /// Increment the bad data counter for this peer.
    ///
    /// Called when a piece received from this peer fails hash verification.
    ///
    /// # Returns
    ///
    /// * `true` if the peer should now be banned (count >= [`BAD_DATA_THRESHOLD`])
    /// * `false` if the peer is still under the threshold
    pub fn increment_bad_data(&mut self) -> bool {
        self.bad_data_count = self.bad_data_count.saturating_add(1);
        self.bad_data_count >= BAD_DATA_THRESHOLD
    }

    /// Decrement the bad data counter for this peer (gradual recovery).
    ///
    /// Called when a valid, verified piece is successfully received from this peer.
    /// This allows peers who occasionally send bad data to recover their reputation.
    /// The count is floored at 0 (never goes negative).
    pub fn decrement_bad_data(&mut self) {
        self.bad_data_count = self.bad_data_count.saturating_sub(1);
    }

    /// Ban this peer with a reason.
    ///
    /// Sets [`is_banned`](Self::is_banned) to `true`, stores the reason,
    /// and logs the ban event. Banned peers are:
    /// - Disconnected immediately
    /// - Not reconnected for the rest of the session
    /// - Excluded from all selection algorithms
    ///
    /// # Arguments
    ///
    /// * `reason` - Human-readable explanation for why the peer was banned
    pub fn ban_peer(&mut self, reason: String) {
        self.is_banned = true;
        self.ban_reason = Some(reason);
    }

    /// Check if this peer is eligible for selection in algorithms.
    ///
    /// Returns `false` if the peer is banned, regardless of other metrics.
    /// This should be called before including a peer in any selection logic.
    pub fn is_eligible_for_selection(&self) -> bool {
        !self.is_banned
    }
}

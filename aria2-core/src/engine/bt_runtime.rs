//! BtRuntime — Per-torrent BitTorrent runtime state
//!
//! Manages the core runtime environment for a single BitTorrent download,
//! including peer connection limits, halt/ready flags, and upload tracking.
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ `BtRuntime.h / BtRuntime.cc`.
//!
//! # Thread Safety
//!
//! The struct itself is **not** `Sync`. In the C++ codebase `BtRuntime` is
//! shared via `std::shared_ptr` with no internal synchronisation — callers
//! coordinate externally. The same pattern applies here: wrap in
//! `Arc<Mutex<BtRuntime>>` (or `Arc<RwLock<...>>`) when shared across tasks.

use tracing::trace;

// ── Constants ─────────────────────────────────────────────────────────

/// Default maximum number of peers (0 in `max_peers` means unlimited).
pub const DEFAULT_MAX_PEERS: u32 = 55;

/// Default minimum number of peers.
pub const DEFAULT_MIN_PEERS: u32 = 40;

// ── BtRuntime ─────────────────────────────────────────────────────────

/// Per-torrent BitTorrent runtime state.
///
/// Tracks connection counts, peer limits, halt/ready signals, and the
/// cumulative upload length recorded at startup (used for seed ratio
/// calculations).
#[derive(Debug, Clone)]
pub struct BtRuntime {
    /// Cumulative upload length at the moment the torrent was started.
    upload_length_at_startup: u64,
    /// When `true`, the torrent should stop as soon as possible.
    halt: bool,
    /// Current number of active peer connections.
    connections: u32,
    /// Whether the BT runtime has finished initial setup and is ready.
    ready: bool,
    /// Maximum peers allowed. `0` means unlimited.
    max_peers: u32,
    /// Minimum peers threshold. `0` means "always under minimum" (i.e. the
    /// `less_than_min_peers` / `less_than_eq_min_peers` predicates always
    /// return `true`).
    min_peers: u32,
}

impl BtRuntime {
    /// Create a new `BtRuntime` with default values matching the C++ constructor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            upload_length_at_startup: 0,
            halt: false,
            connections: 0,
            ready: false,
            max_peers: DEFAULT_MAX_PEERS,
            min_peers: DEFAULT_MIN_PEERS,
        }
    }

    // ── Accessors ──────────────────────────────────────────────────────

    /// Return the cumulative upload length recorded at startup.
    #[inline]
    pub fn upload_length_at_startup(&self) -> u64 {
        self.upload_length_at_startup
    }

    /// Set the cumulative upload length recorded at startup.
    pub fn set_upload_length_at_startup(&mut self, length: u64) {
        trace!(
            old = self.upload_length_at_startup,
            new = length,
            "set_upload_length_at_startup"
        );
        self.upload_length_at_startup = length;
    }

    /// Return `true` if the torrent has been signalled to halt.
    #[inline]
    pub fn is_halt(&self) -> bool {
        self.halt
    }

    /// Set the halt flag.
    pub fn set_halt(&mut self, halt: bool) {
        trace!(old = self.halt, new = halt, "set_halt");
        self.halt = halt;
    }

    /// Return the current number of peer connections.
    #[inline]
    pub fn connections(&self) -> u32 {
        self.connections
    }

    /// Increase the connection counter by one (saturating — never overflows).
    pub fn increase_connections(&mut self) {
        let prev = self.connections;
        self.connections = self.connections.saturating_add(1);
        trace!(prev, new = self.connections, "increase_connections");
    }

    /// Decrease the connection counter by one (saturating — never underflows
    /// to negative, unlike the C++ version which can go below zero).
    pub fn decrease_connections(&mut self) {
        let prev = self.connections;
        self.connections = self.connections.saturating_sub(1);
        trace!(prev, new = self.connections, "decrease_connections");
    }

    /// Return `true` if we have not yet reached the maximum peer limit.
    ///
    /// When `max_peers == 0` (unlimited) this always returns `true`.
    #[inline]
    pub fn less_than_max_peers(&self) -> bool {
        self.max_peers == 0 || self.connections < self.max_peers
    }

    /// Return `true` if the current connection count is below the minimum
    /// peer threshold.
    ///
    /// When `min_peers == 0` this always returns `true` (interpreted as
    /// "always under minimum", i.e. we always need more peers).
    #[inline]
    pub fn less_than_min_peers(&self) -> bool {
        self.min_peers == 0 || self.connections < self.min_peers
    }

    /// Return `true` if the current connection count is at or below the
    /// minimum peer threshold.
    ///
    /// When `min_peers == 0` this always returns `true`.
    #[inline]
    pub fn less_than_eq_min_peers(&self) -> bool {
        self.min_peers == 0 || self.connections <= self.min_peers
    }

    /// Return whether the runtime is ready.
    #[inline]
    pub fn ready(&self) -> bool {
        self.ready
    }

    /// Set the ready flag.
    pub fn set_ready(&mut self, go: bool) {
        trace!(old = self.ready, new = go, "set_ready");
        self.ready = go;
    }

    /// Return the current maximum peer limit. `0` means unlimited.
    #[inline]
    pub fn max_peers(&self) -> u32 {
        self.max_peers
    }

    /// Set the maximum number of peers and auto-calculate `min_peers`.
    ///
    /// `min_peers` is set to `max_peers * 0.8` (truncated). If the result
    /// would be `0` but `max_peers` is non-zero, `min_peers` is set equal
    /// to `max_peers` to prevent a degenerate state where we always
    /// consider ourselves "under minimum".
    pub fn set_max_peers(&mut self, max_peers: u32) {
        let min_peers = if max_peers == 0 {
            0
        } else {
            let calculated = (max_peers as f64 * 0.8) as u32;
            if calculated == 0 {
                max_peers
            } else {
                calculated
            }
        };
        trace!(
            old_max = self.max_peers,
            new_max = max_peers,
            new_min = min_peers,
            "set_max_peers"
        );
        self.max_peers = max_peers;
        self.min_peers = min_peers;
    }
}

impl Default for BtRuntime {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Default values ─────────────────────────────────────────────────

    #[test]
    fn test_default_values() {
        let rt = BtRuntime::new();
        assert_eq!(rt.upload_length_at_startup(), 0);
        assert!(!rt.is_halt());
        assert_eq!(rt.connections(), 0);
        assert!(!rt.ready());
        assert_eq!(rt.max_peers(), DEFAULT_MAX_PEERS);
        assert_eq!(rt.min_peers, DEFAULT_MIN_PEERS);
    }

    #[test]
    fn test_default_trait() {
        let rt = BtRuntime::default();
        assert_eq!(rt.max_peers(), DEFAULT_MAX_PEERS);
    }

    // ── Connections ────────────────────────────────────────────────────

    #[test]
    fn test_increase_connections() {
        let mut rt = BtRuntime::new();
        assert_eq!(rt.connections(), 0);
        rt.increase_connections();
        assert_eq!(rt.connections(), 1);
        rt.increase_connections();
        assert_eq!(rt.connections(), 2);
    }

    #[test]
    fn test_decrease_connections() {
        let mut rt = BtRuntime::new();
        rt.increase_connections();
        rt.increase_connections();
        assert_eq!(rt.connections(), 2);
        rt.decrease_connections();
        assert_eq!(rt.connections(), 1);
    }

    #[test]
    fn test_decrease_connections_saturating() {
        let mut rt = BtRuntime::new();
        // Decreasing from 0 should not underflow (unlike C++ which can go negative)
        rt.decrease_connections();
        assert_eq!(rt.connections(), 0);
    }

    #[test]
    fn test_increase_connections_saturating() {
        let mut rt = BtRuntime::new();
        rt.connections = u32::MAX;
        rt.increase_connections();
        // Should saturate at u32::MAX, not overflow
        assert_eq!(rt.connections(), u32::MAX);
    }

    // ── less_than_max_peers ────────────────────────────────────────────

    #[test]
    fn test_less_than_max_peers_unlimited() {
        let mut rt = BtRuntime::new();
        rt.max_peers = 0; // unlimited
        rt.connections = 9999;
        assert!(rt.less_than_max_peers());
    }

    #[test]
    fn test_less_than_max_peers_below() {
        let rt = BtRuntime::new(); // max_peers = 55
        assert!(rt.less_than_max_peers()); // connections = 0 < 55
    }

    #[test]
    fn test_less_than_max_peers_at_limit() {
        let mut rt = BtRuntime::new();
        rt.connections = DEFAULT_MAX_PEERS;
        assert!(!rt.less_than_max_peers()); // 55 == 55, not less than
    }

    #[test]
    fn test_less_than_max_peers_above_limit() {
        let mut rt = BtRuntime::new();
        rt.connections = DEFAULT_MAX_PEERS + 1;
        assert!(!rt.less_than_max_peers());
    }

    // ── less_than_min_peers ────────────────────────────────────────────

    #[test]
    fn test_less_than_min_peers_zero_always_true() {
        let mut rt = BtRuntime::new();
        rt.min_peers = 0;
        rt.connections = 9999;
        assert!(rt.less_than_min_peers());
    }

    #[test]
    fn test_less_than_min_peers_below() {
        let rt = BtRuntime::new(); // min_peers = 40
        assert!(rt.less_than_min_peers()); // connections = 0 < 40
    }

    #[test]
    fn test_less_than_min_peers_at_limit() {
        let mut rt = BtRuntime::new();
        rt.connections = DEFAULT_MIN_PEERS;
        assert!(!rt.less_than_min_peers()); // 40 == 40, not less than
    }

    // ── less_than_eq_min_peers ─────────────────────────────────────────

    #[test]
    fn test_less_than_eq_min_peers_zero_always_true() {
        let mut rt = BtRuntime::new();
        rt.min_peers = 0;
        rt.connections = 9999;
        assert!(rt.less_than_eq_min_peers());
    }

    #[test]
    fn test_less_than_eq_min_peers_below() {
        let rt = BtRuntime::new(); // min_peers = 40
        assert!(rt.less_than_eq_min_peers()); // 0 <= 40
    }

    #[test]
    fn test_less_than_eq_min_peers_at_limit() {
        let mut rt = BtRuntime::new();
        rt.connections = DEFAULT_MIN_PEERS;
        assert!(rt.less_than_eq_min_peers()); // 40 <= 40
    }

    #[test]
    fn test_less_than_eq_min_peers_above() {
        let mut rt = BtRuntime::new();
        rt.connections = DEFAULT_MIN_PEERS + 1;
        assert!(!rt.less_than_eq_min_peers()); // 41 > 40
    }

    // ── set_max_peers ──────────────────────────────────────────────────

    #[test]
    fn test_set_max_peers_normal() {
        let mut rt = BtRuntime::new();
        rt.set_max_peers(55);
        assert_eq!(rt.max_peers(), 55);
        assert_eq!(rt.min_peers, 44); // 55 * 0.8 = 44.0
    }

    #[test]
    fn test_set_max_peers_zero() {
        let mut rt = BtRuntime::new();
        rt.set_max_peers(0);
        assert_eq!(rt.max_peers(), 0);
        assert_eq!(rt.min_peers, 0); // both zero = unlimited
    }

    #[test]
    fn test_set_max_peers_one() {
        let mut rt = BtRuntime::new();
        rt.set_max_peers(1);
        assert_eq!(rt.max_peers(), 1);
        // 1 * 0.8 = 0.8, truncated to 0 → min_peers = max_peers = 1
        assert_eq!(rt.min_peers, 1);
    }

    #[test]
    fn test_set_max_peers_two() {
        let mut rt = BtRuntime::new();
        rt.set_max_peers(2);
        assert_eq!(rt.max_peers(), 2);
        // 2 * 0.8 = 1.6, truncated to 1
        assert_eq!(rt.min_peers, 1);
    }

    #[test]
    fn test_set_max_peers_ten() {
        let mut rt = BtRuntime::new();
        rt.set_max_peers(10);
        assert_eq!(rt.max_peers(), 10);
        // 10 * 0.8 = 8.0
        assert_eq!(rt.min_peers, 8);
    }

    // ── Halt flag ──────────────────────────────────────────────────────

    #[test]
    fn test_halt_flag() {
        let mut rt = BtRuntime::new();
        assert!(!rt.is_halt());
        rt.set_halt(true);
        assert!(rt.is_halt());
        rt.set_halt(false);
        assert!(!rt.is_halt());
    }

    // ── Ready flag ─────────────────────────────────────────────────────

    #[test]
    fn test_ready_flag() {
        let mut rt = BtRuntime::new();
        assert!(!rt.ready());
        rt.set_ready(true);
        assert!(rt.ready());
        rt.set_ready(false);
        assert!(!rt.ready());
    }

    // ── Upload length at startup ───────────────────────────────────────

    #[test]
    fn test_upload_length_at_startup() {
        let mut rt = BtRuntime::new();
        assert_eq!(rt.upload_length_at_startup(), 0);
        rt.set_upload_length_at_startup(1_048_576);
        assert_eq!(rt.upload_length_at_startup(), 1_048_576);
    }

    // ── Clone ──────────────────────────────────────────────────────────

    #[test]
    fn test_clone() {
        let mut rt = BtRuntime::new();
        rt.set_halt(true);
        rt.increase_connections();
        rt.set_upload_length_at_startup(42);
        let cloned = rt.clone();
        assert_eq!(cloned.is_halt(), true);
        assert_eq!(cloned.connections(), 1);
        assert_eq!(cloned.upload_length_at_startup(), 42);
        assert_eq!(cloned.max_peers(), DEFAULT_MAX_PEERS);
    }
}

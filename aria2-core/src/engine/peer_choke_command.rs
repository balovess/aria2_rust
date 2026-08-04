//! Periodic choke-round command — Rust equivalent of C++ `PeerChokeCommand`.
//!
//! In the C++ codebase, `PeerChokeCommand` is a long-lived `Command` that
//! re-adds itself to the engine's command queue on every execution. It checks
//! whether the choke-round interval has elapsed and, if so, executes a choke
//! round on the peer storage.
//!
//! In the Rust async architecture, we expose `PeerChokeDriver` which can be
//! called periodically (e.g. from a timer tick or a download command's execute
//! loop). It encapsulates the same interval-check + execute logic.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `PeerChokeDriver` | `PeerChokeCommand` |
//! | `PeerChokeDriver::tick()` | `PeerChokeCommand::execute()` |
//! | `should_execute()` | `chokeRoundIntervalElapsed()` |

use std::time::Duration;

use crate::engine::bt_choke_manager::{BtLeecherStateChoke, BtSeederStateChoke};
use crate::engine::peer_stats::PeerStats;

/// Default choke round interval in seconds.
/// Mirrors C++ `10_s` used in `DefaultPeerStorage`.
pub const DEFAULT_CHOKE_ROUND_INTERVAL_SECS: u64 = 10;

// ===========================================================================
// PeerChokeDriver
// ===========================================================================

/// Periodic choke-round driver, equivalent to C++ `PeerChokeCommand`.
///
/// Encapsulates the seeder/leecher choke algorithms and the interval check.
/// Call [`tick`](Self::tick) periodically; it will execute a choke round
/// only when the interval has elapsed.
///
/// # Usage
///
/// ```ignore
/// let mut driver = PeerChokeDriver::new(choke_interval);
/// // Call on every timer tick (~1s):
/// driver.tick(halt, download_finished, &mut peer_refs);
/// ```
#[derive(Debug, Clone)]
pub struct PeerChokeDriver {
    /// Choke round interval.
    interval: Duration,
    /// Seeder-state choking algorithm.
    seeder_choke: BtSeederStateChoke,
    /// Leecher-state choking algorithm.
    leecher_choke: BtLeecherStateChoke,
    /// Whether the download has finished (determines seeder vs leecher mode).
    download_finished: bool,
}

impl PeerChokeDriver {
    /// Create a new `PeerChokeDriver` with the given choke round interval.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            seeder_choke: BtSeederStateChoke::new(),
            leecher_choke: BtLeecherStateChoke::new(),
            download_finished: false,
        }
    }

    /// Create a `PeerChokeDriver` with the default 10-second interval.
    pub fn with_default_interval() -> Self {
        Self::new(Duration::from_secs(DEFAULT_CHOKE_ROUND_INTERVAL_SECS))
    }

    /// Create a `PeerChokeDriver` with a custom number of seeder unchoke slots.
    ///
    /// Useful for testing or configuration override.
    pub fn with_seeder_slots(slots: usize) -> Self {
        let mut driver = Self::with_default_interval();
        driver.seeder_choke = BtSeederStateChoke::with_slots(slots);
        driver
    }

    /// Set whether the download has finished (affects seeder vs leecher mode).
    pub fn set_download_finished(&mut self, finished: bool) {
        self.download_finished = finished;
    }

    /// Get whether the download has finished.
    pub fn download_finished(&self) -> bool {
        self.download_finished
    }

    /// Check whether a choke round should be executed based on the interval.
    ///
    /// Mirrors C++ `PeerChokeCommand::execute()` checking
    /// `peerStorage_->chokeRoundIntervalElapsed()`.
    pub fn should_execute(&self) -> bool {
        if self.download_finished {
            self.seeder_choke.should_execute(self.interval)
        } else {
            self.leecher_choke.should_execute(self.interval)
        }
    }

    /// Execute a choke round if the interval has elapsed.
    ///
    /// Mirrors C++ `PeerChokeCommand::execute()`:
    /// 1. If the runtime is halted, do nothing (return `false`).
    /// 2. If the interval has elapsed, execute the appropriate choke algorithm.
    /// 3. Returns `true` if a choke round was executed.
    ///
    /// # Arguments
    ///
    /// * `halt` - Whether the BT runtime is halted (stop downloading/seeding).
    /// * `peers` - Mutable slice of peer statistics to update.
    ///
    /// # Returns
    ///
    /// `true` if a choke round was executed this tick, `false` otherwise.
    pub fn tick(&mut self, halt: bool, peers: &mut [&mut PeerStats]) -> bool {
        if halt {
            return false;
        }

        if !self.should_execute() {
            return false;
        }

        self.execute_choke(peers);
        true
    }

    /// Force-execute a choke round regardless of the interval.
    ///
    /// Useful for testing or immediate choke adjustment after a peer state
    /// change (e.g. peer disconnect).
    pub fn execute_choke_by_identity(&mut self, peers: &mut [&mut PeerStats]) {
        if self.download_finished {
            self.seeder_choke.execute_choke_by_identity(peers);
        } else {
            self.leecher_choke.execute_choke_by_identity(peers);
        }
    }

    pub fn execute_choke(&mut self, peers: &mut [&mut PeerStats]) {
        self.execute_choke_by_identity(peers);
    }

    // ------------------------------------------------------------------
    // Accessors for testing / diagnostics
    // ------------------------------------------------------------------

    /// Get a reference to the seeder-state choke algorithm.
    pub fn seeder_choke(&self) -> &BtSeederStateChoke {
        &self.seeder_choke
    }

    /// Get a reference to the leecher-state choke algorithm.
    pub fn leecher_choke(&self) -> &BtLeecherStateChoke {
        &self.leecher_choke
    }

    /// Get the current round of the active choke algorithm.
    pub fn round(&self) -> u32 {
        if self.download_finished {
            self.seeder_choke.round()
        } else {
            self.leecher_choke.round()
        }
    }

    /// Get the choke round interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }
}

impl Default for PeerChokeDriver {
    fn default() -> Self {
        Self::with_default_interval()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::peer_stats::PeerStats;

    fn make_peer() -> PeerStats {
        PeerStats::new([0u8; 20], "127.0.0.1:6881".parse().unwrap())
    }

    /// Convert a mutable slice of PeerStats into the `Vec<&mut PeerStats>`
    /// format required by `execute_choke` / `tick`.
    fn to_refs(peers: &mut [PeerStats]) -> Vec<&mut PeerStats> {
        peers.iter_mut().collect()
    }

    #[test]
    fn test_driver_default_interval() {
        let driver = PeerChokeDriver::default();
        assert_eq!(driver.interval(), Duration::from_secs(10));
    }

    #[test]
    fn test_driver_should_execute_initially() {
        // No rounds have been executed — interval should be considered elapsed.
        let driver = PeerChokeDriver::with_default_interval();
        assert!(driver.should_execute());
    }

    #[test]
    fn test_driver_tick_executes_choke_round() {
        let mut driver = PeerChokeDriver::with_default_interval();
        let mut peers = [make_peer()];
        let mut refs = to_refs(&mut peers);

        let executed = driver.tick(false, &mut refs[..]);
        assert!(executed, "First tick should execute a choke round");
        // After execution, interval has not elapsed — next tick should not execute.
        let mut refs = to_refs(&mut peers);
        let executed2 = driver.tick(false, &mut refs[..]);
        assert!(
            !executed2,
            "Immediate second tick should not execute (interval not elapsed)"
        );
    }

    #[test]
    fn test_driver_tick_halting_does_not_execute() {
        let mut driver = PeerChokeDriver::with_default_interval();
        let mut peers = [make_peer()];
        let mut refs = to_refs(&mut peers);

        let executed = driver.tick(true, &mut refs[..]);
        assert!(!executed, "Should not execute when halted");
    }

    #[test]
    fn test_driver_seeder_mode() {
        let mut driver = PeerChokeDriver::with_default_interval();
        driver.set_download_finished(true);
        assert!(driver.download_finished());

        let mut peers = {
            let mut p = make_peer();
            p.peer_interested = true;
            [p]
        };
        let mut refs = to_refs(&mut peers);

        let executed = driver.tick(false, &mut refs[..]);
        assert!(executed);
        // Peer should be unchoked (seeder unchokes interested peers)
        assert!(!peers[0].am_choking);
    }

    #[test]
    fn test_driver_leecher_mode() {
        let mut driver = PeerChokeDriver::with_default_interval();
        assert!(!driver.download_finished());

        let mut peers = {
            let mut p = make_peer();
            p.peer_interested = true;
            [p]
        };
        let mut refs = to_refs(&mut peers);

        let executed = driver.tick(false, &mut refs[..]);
        assert!(executed);
        // In leecher mode with only one interested peer, it should be unchoked
        assert!(!peers[0].am_choking);
    }

    #[test]
    fn test_driver_round_advances() {
        let mut driver = PeerChokeDriver::with_default_interval();
        let mut peers = {
            let mut p = make_peer();
            p.peer_interested = true;
            [p]
        };

        assert_eq!(driver.round(), 0);
        let mut refs = to_refs(&mut peers);
        driver.execute_choke(&mut refs[..]);
        assert_eq!(driver.round(), 1);

        let mut refs = to_refs(&mut peers);
        driver.execute_choke(&mut refs[..]);
        assert_eq!(driver.round(), 2);

        let mut refs = to_refs(&mut peers);
        driver.execute_choke(&mut refs[..]);
        assert_eq!(driver.round(), 0); // wraps
    }

    #[test]
    fn test_driver_force_execute() {
        let mut driver = PeerChokeDriver::with_default_interval();
        let mut peers = {
            let mut p = make_peer();
            p.peer_interested = true;
            [p]
        };

        // Execute first round
        let mut refs = to_refs(&mut peers);
        driver.execute_choke(&mut refs[..]);

        // Force-execute second round even though interval hasn't elapsed
        let mut refs = to_refs(&mut peers);
        driver.execute_choke(&mut refs[..]);
        assert_eq!(driver.round(), 2);
    }

    #[test]
    fn test_driver_with_seeder_slots() {
        let driver = PeerChokeDriver::with_seeder_slots(2);
        // Should create a driver with 2 seeder unchoke slots
        assert_eq!(driver.interval(), Duration::from_secs(10));
    }
}

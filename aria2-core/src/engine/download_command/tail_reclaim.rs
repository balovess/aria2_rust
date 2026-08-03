//! Tail reclaim integration for DownloadCommand.
//!
//! Ports the C++ `DownloadCommand` tail reclaim methods:
//! - `updateTailReclaimProgress()` — tracks when progress was last made
//! - `fillTailReclaimState()` — populates `HttpTailReclaimState` from current
//!   download state
//! - `isTailReclaimCheckReady()` — returns true when the tail is blocked
//!   (conditions met for potential reclaim)
//! - `shouldReclaimTailSegment()` — returns true when the tail should
//!   actually be reclaimed (blocked + stalled long enough)
//!
//! In C++, `isTailReclaimCheckReady()` is used in `noCheck()` to keep the
//! command monitored even when no explicit speed limit is set. In Rust, the
//! equivalent is `needs_speed_monitoring()`.

use std::time::{Duration, Instant};

use crate::engine::http_tail_reclaim::{self, HttpTailReclaimState};
use crate::util::rwlock_ext::RwLockRecover;

use super::DownloadCommand;

impl DownloadCommand {
    // ── Tail Reclaim Progress Tracking ──────────────────────────────────

    /// Update the tail reclaim progress tracker.
    ///
    /// Must be called after data is received so the "last progress" timestamp
    /// stays current. If the completed length has increased since the last
    /// call, the tracking timestamp is refreshed.
    ///
    /// Mirrors C++ `DownloadCommand::updateTailReclaimProgress()`.
    pub fn update_tail_reclaim_progress(&mut self) {
        let current_completed = self.progress.completed_length();
        if current_completed > self.last_tail_reclaim_session_download_length {
            self.last_tail_reclaim_session_download_length = current_completed;
            self.tail_reclaim_last_progress = Instant::now();
        }
    }

    // ── Tail Reclaim State ──────────────────────────────────────────────

    /// Fill an `HttpTailReclaimState` from the current download state.
    ///
    /// Returns `None` if the download is not in a state where tail reclaim
    /// is applicable (e.g. no URI, zero total length).
    ///
    /// Mirrors C++ `DownloadCommand::fillTailReclaimState()`.
    pub fn fill_tail_reclaim_state(&self) -> Option<HttpTailReclaimState> {
        // Read current progress snapshot.  The caller should have called
        // update_tail_reclaim_progress() recently so that
        // last_tail_reclaim_session_download_length and tail_reclaim_last_progress
        // are up-to-date.  In C++ this is guaranteed because
        // updateTailReclaimProgress() is called on every data chunk; in Rust
        // it is called at strategic points in execute().
        let current_completed = self.progress.completed_length();
        let last_reclaim = self.last_tail_reclaim_session_download_length;

        // Derive protocol from the first URI.
        let uri = {
            let g = self.group.recover();
            g.uris().first().cloned().unwrap_or_default()
        };
        if uri.is_empty() {
            return None;
        }

        let protocol = extract_protocol(&uri);

        let g = self.group.recover();
        let total_length = g.total_length() as i64;
        if total_length <= 0 {
            return None;
        }

        let completed_length = g.completed_length();
        let pending_length = total_length.saturating_sub(completed_length as i64);
        if pending_length <= 0 {
            return None;
        }

        // In the Rust architecture, DownloadCommand handles HTTP/HTTPS only.
        // BitTorrent is handled by BtDownloadCommand. Therefore:
        // - p2p_involved is always false for this command type
        // - num_stream_command == num_commands (all are HTTP stream commands)
        //   This means isHttpTailBlocked returns false for pure HTTP downloads
        //   (as designed — tail reclaim targets mixed HTTP+BT scenarios).
        //   However, we populate the state accurately so that when BT web
        //   seed support is added, the logic will work correctly.
        let p2p_involved = false;
        let num_concurrent_command = g.num_commands() as i32;
        let num_stream_command = num_concurrent_command; // all HTTP for now

        // No piece storage equivalent in Rust yet — default to false.
        let has_missing_unused_piece = false;

        // Calculate no-progress time since the last recorded progress.
        let no_progress_time = self.tail_reclaim_last_progress.elapsed();

        // Stall time is the startup idle time (C++ uses startupIdleTime_).
        let stall_time = self.startup_idle_time;

        Some(HttpTailReclaimState {
            protocol,
            p2p_involved,
            total_length,
            pending_length,
            has_missing_unused_piece,
            num_concurrent_command,
            num_stream_command,
            current_session_download_length: current_completed,
            last_session_download_length: last_reclaim,
            no_progress_time,
            stall_time,
        })
    }

    /// Check whether the HTTP tail segment should be reclaimed.
    ///
    /// Returns `true` when:
    /// 1. The tail is blocked (conditions met for potential reclaim), AND
    /// 2. No progress has been made since the last check, AND
    /// 3. The no-progress duration has reached the stall threshold.
    ///
    /// Mirrors C++ `DownloadCommand::shouldReclaimTailSegment()`.
    pub fn should_reclaim_tail_segment(&self) -> bool {
        match self.fill_tail_reclaim_state() {
            Some(state) => http_tail_reclaim::should_reclaim_http_tail_segment(&state),
            None => false,
        }
    }

    /// Check whether the tail reclaim conditions are met for potential
    /// reclaim consideration (i.e. the tail is "blocked").
    ///
    /// This is a lighter check than `should_reclaim_tail_segment()` — it
    /// only checks if the download is in a state where tail reclaim should
    /// be considered, without checking the stall timeout.
    ///
    /// Mirrors C++ `DownloadCommand::isTailReclaimCheckReady()`.
    pub fn is_tail_reclaim_check_ready(&self) -> bool {
        match self.fill_tail_reclaim_state() {
            Some(state) => http_tail_reclaim::is_http_tail_blocked(&state),
            None => false,
        }
    }

    /// Whether the download needs speed monitoring.
    ///
    /// Returns `true` when either:
    /// - A lowest speed limit is configured, or
    /// - The tail reclaim check is ready (the tail is blocked).
    ///
    /// Mirrors C++ `DownloadCommand::noCheck()`, which returns true when
    /// the command should remain monitored in the event loop.
    pub fn needs_speed_monitoring(&self) -> bool {
        self.lowest_speed_limit > 0 || self.is_tail_reclaim_check_ready()
    }

    /// Get the configured startup idle time.
    pub fn startup_idle_time(&self) -> Duration {
        self.startup_idle_time
    }

    /// Set the startup idle time (stall threshold for tail reclaim).
    ///
    /// Mirrors C++ `DownloadCommand::setStartupIdleTime()`.
    pub fn set_startup_idle_time(&mut self, secs: u64) {
        self.startup_idle_time = Duration::from_secs(secs);
    }

    /// Get the configured lowest download speed limit.
    pub fn lowest_speed_limit(&self) -> u64 {
        self.lowest_speed_limit
    }

    /// Set the lowest download speed limit.
    ///
    /// Mirrors C++ `DownloadCommand::setLowestDownloadSpeedLimit()`.
    pub fn set_lowest_speed_limit(&mut self, limit: u64) {
        self.lowest_speed_limit = limit;
    }
}

/// Extract the protocol scheme from a URI string (case-preserved).
///
/// Returns `"http"` for `http://...`, `"https"` for `https://...`, etc.
/// Returns an empty string if the URI doesn't contain a scheme.
fn extract_protocol(uri: &str) -> String {
    if let Some(colon_pos) = uri.find(':') {
        uri[..colon_pos].to_string()
    } else {
        String::new()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::download_command::DownloadCommand;
    use crate::request::request_group::{DownloadOptions, GroupId};

    fn create_test_cmd() -> DownloadCommand {
        DownloadCommand::new(
            GroupId::new(1),
            "http://example.com/file.bin",
            &DownloadOptions::default(),
            None,
            None,
        )
        .expect("DownloadCommand::new should succeed")
    }

    // ── extract_protocol ────────────────────────────────────────────────

    #[test]
    fn test_extract_protocol_http() {
        assert_eq!(extract_protocol("http://example.com/file"), "http");
    }

    #[test]
    fn test_extract_protocol_https() {
        assert_eq!(extract_protocol("https://example.com/file"), "https");
    }

    #[test]
    fn test_extract_protocol_ftp() {
        assert_eq!(extract_protocol("ftp://example.com/file"), "ftp");
    }

    #[test]
    fn test_extract_protocol_no_scheme() {
        assert_eq!(extract_protocol("example.com/file"), "");
    }

    // ── fill_tail_reclaim_state ─────────────────────────────────────────

    #[test]
    fn test_fill_tail_reclaim_state_returns_none_for_zero_total_length() {
        let cmd = create_test_cmd();
        // Total length is 0 by default → should return None.
        assert!(cmd.fill_tail_reclaim_state().is_none());
    }

    #[test]
    fn test_fill_tail_reclaim_state_returns_some_with_total_length() {
        let cmd = create_test_cmd();
        // Set total length to simulate a download in progress.
        cmd.group.recover().set_total_length(10_000);
        let state = cmd.fill_tail_reclaim_state();
        assert!(state.is_some());
        let s = state.unwrap();
        assert_eq!(s.protocol, "http");
        assert!(!s.p2p_involved);
        assert_eq!(s.total_length, 10_000);
        assert_eq!(s.pending_length, 10_000); // nothing downloaded yet
    }

    // ── should_reclaim_tail_segment ─────────────────────────────────────

    #[test]
    fn test_should_reclaim_returns_false_by_default() {
        let cmd = create_test_cmd();
        // Default state: pure HTTP download with num_stream_command ==
        // num_concurrent_command → isHttpTailBlocked returns false →
        // should_reclaim returns false.
        assert!(!cmd.should_reclaim_tail_segment());
    }

    // ── is_tail_reclaim_check_ready ─────────────────────────────────────

    #[test]
    fn test_is_tail_reclaim_check_ready_returns_false_for_pure_http() {
        let cmd = create_test_cmd();
        // Pure HTTP: num_stream_command >= num_concurrent_command →
        // isHttpTailBlocked returns false.
        assert!(!cmd.is_tail_reclaim_check_ready());
    }

    // ── needs_speed_monitoring ──────────────────────────────────────────

    #[test]
    fn test_needs_speed_monitoring_default() {
        let cmd = create_test_cmd();
        // No speed limit and no tail reclaim check ready → false.
        assert!(!cmd.needs_speed_monitoring());
    }

    #[test]
    fn test_needs_speed_monitoring_with_speed_limit() {
        let mut cmd = create_test_cmd();
        cmd.set_lowest_speed_limit(1024);
        assert!(cmd.needs_speed_monitoring());
    }

    // ── update_tail_reclaim_progress ────────────────────────────────────

    #[test]
    fn test_update_tail_reclaim_progress_tracks_length() {
        let mut cmd = create_test_cmd();
        assert_eq!(cmd.last_tail_reclaim_session_download_length, 0);

        // Simulate progress by updating AtomicProgress directly.
        cmd.progress.set_completed_length(5000);
        cmd.update_tail_reclaim_progress();
        assert_eq!(cmd.last_tail_reclaim_session_download_length, 5000);

        // No new progress → tracking value unchanged.
        cmd.update_tail_reclaim_progress();
        assert_eq!(cmd.last_tail_reclaim_session_download_length, 5000);

        // More progress → updated.
        cmd.progress.set_completed_length(8000);
        cmd.update_tail_reclaim_progress();
        assert_eq!(cmd.last_tail_reclaim_session_download_length, 8000);
    }

    // ── setter methods ──────────────────────────────────────────────────

    #[test]
    fn test_set_startup_idle_time() {
        let mut cmd = create_test_cmd();
        cmd.set_startup_idle_time(20);
        assert_eq!(cmd.startup_idle_time(), Duration::from_secs(20));
    }

    #[test]
    fn test_set_lowest_speed_limit() {
        let mut cmd = create_test_cmd();
        cmd.set_lowest_speed_limit(4096);
        assert_eq!(cmd.lowest_speed_limit(), 4096);
    }

    // ── Integration: tail reclaim state with mixed commands ─────────────
    // These tests verify that if we manually set num_stream_command <
    // num_concurrent_command (simulating a BT+HTTP mixed scenario), the
    // tail reclaim policy works correctly.

    #[test]
    fn test_tail_reclaim_blocked_with_simulated_mixed_commands() {
        let cmd = create_test_cmd();
        cmd.group.recover().set_total_length(10_000);

        // Build state manually to simulate a BT+HTTP mixed scenario.
        let mut state = cmd.fill_tail_reclaim_state().unwrap();

        // Override to simulate: 3 concurrent commands, 1 stream (HTTP),
        // 2 BT — this is the scenario where tail reclaim is relevant.
        state.num_concurrent_command = 3;
        state.num_stream_command = 1;
        state.p2p_involved = false;
        state.has_missing_unused_piece = false;
        state.no_progress_time = Duration::from_secs(30);
        state.stall_time = Duration::from_secs(20);
        // No progress: current <= last.
        state.current_session_download_length = 100;
        state.last_session_download_length = 100;

        assert!(http_tail_reclaim::is_http_tail_blocked(&state));
        assert!(http_tail_reclaim::should_reclaim_http_tail_segment(&state));
    }

    #[test]
    fn test_tail_reclaim_not_blocked_when_still_progressing() {
        let cmd = create_test_cmd();
        cmd.group.recover().set_total_length(10_000);

        let mut state = cmd.fill_tail_reclaim_state().unwrap();
        state.num_concurrent_command = 3;
        state.num_stream_command = 1;
        // Progress is being made: current > last.
        state.current_session_download_length = 200;
        state.last_session_download_length = 100;
        state.no_progress_time = Duration::from_secs(60);
        state.stall_time = Duration::from_secs(20);

        assert!(http_tail_reclaim::is_http_tail_blocked(&state));
        assert!(!http_tail_reclaim::should_reclaim_http_tail_segment(&state));
    }

    #[test]
    fn test_tail_reclaim_not_blocked_when_p2p_involved() {
        let cmd = create_test_cmd();
        cmd.group.recover().set_total_length(10_000);

        let mut state = cmd.fill_tail_reclaim_state().unwrap();
        state.num_concurrent_command = 3;
        state.num_stream_command = 1;
        state.p2p_involved = true;

        assert!(!http_tail_reclaim::is_http_tail_blocked(&state));
        assert!(!http_tail_reclaim::should_reclaim_http_tail_segment(&state));
    }
}

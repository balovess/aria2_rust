//! HTTP/HTTPS Tracker Client with Event State Machine
//!
//! This module provides a stateful tracker client that supports both HTTP and HTTPS
//! tracker URLs, with proper event lifecycle management (Started -> Completed -> Stopped).
//!
//! # Event State Machine
//!
//! The tracker follows a well-defined event sequence:
//! - **Started**: Sent when download begins (first announce)
//! - **Completed**: Sent when all pieces are downloaded successfully
//! - **Stopped**: Sent when download is cancelled or removed
//! - **None**: Regular interval announces (no specific event)

use std::time::{Duration, Instant};

use tracing::{debug, info};

/// Tracker announce events as defined in BEP 3 / BEP 15.
///
/// These events control the lifecycle of tracker announcements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerEvent {
    /// Download started - sent on first announce
    Started,
    /// Download stopped/cancelled - sent on shutdown
    Stopped,
    /// Download completed - sent when all pieces downloaded
    Completed,
    /// No specific event - used for regular interval announces
    None,
}

impl TrackerEvent {
    /// Convert event to the string value expected by trackers
    pub fn as_str(&self) -> &'static str {
        match self {
            TrackerEvent::Started => "started",
            TrackerEvent::Stopped => "stopped",
            TrackerEvent::Completed => "completed",
            TrackerEvent::None => "",
        }
    }
}

/// State machine for tracking announce events and intervals.
///
/// Manages the current event state, timing of announces, and ensures
/// proper event sequencing according to BitTorrent specification.
#[derive(Debug)]
pub struct TrackerState {
    /// Current event to send on next announce
    pub current_event: TrackerEvent,
    /// Timestamp of last successful announce
    pub last_announce_time: Option<Instant>,
    /// Minimum interval between announces (from tracker response)
    pub min_interval_secs: u64,
    /// Normal interval between announces (from tracker response)
    pub interval_secs: u64,
    /// Total number of announces sent
    pub announce_count: u64,
    /// Whether we have already sent the 'completed' event
    completed_sent: bool,
    /// Whether we have already sent the 'stopped' event
    stopped_sent: bool,
}

impl Default for TrackerState {
    fn default() -> Self {
        Self {
            current_event: TrackerEvent::Started, // First announce should be Started
            last_announce_time: None,
            min_interval_secs: 300, // Default 5 minutes
            interval_secs: 1800,    // Default 30 minutes
            announce_count: 0,
            completed_sent: false,
            stopped_sent: false,
        }
    }
}

impl TrackerState {
    /// Create a new tracker state with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if it's time to send another announce based on interval constraints
    ///
    /// Returns true if enough time has elapsed since the last announce
    /// according to min_interval_secs
    pub fn should_announce(&self) -> bool {
        match self.last_announce_time {
            Some(last) => {
                let elapsed = last.elapsed().as_secs();
                elapsed >= self.min_interval_secs
            }
            None => true, // Never announced before
        }
    }

    /// Get the recommended wait time until next announce
    pub fn secs_until_next_announce(&self) -> u64 {
        match self.last_announce_time {
            Some(last) => {
                let elapsed = last.elapsed().as_secs();
                if elapsed >= self.min_interval_secs {
                    0
                } else {
                    self.min_interval_secs - elapsed
                }
            }
            None => 0,
        }
    }

    /// Mark that an announce has been sent and advance the event state
    ///
    /// This updates internal timing and transitions the event state:
    /// - After Started -> None (for regular announces)
    /// - After Completed -> None
    /// - After Stopped -> stays Stopped (terminal state)
    pub fn record_announce(&mut self, event: TrackerEvent) {
        self.current_event = match event {
            TrackerEvent::Started => {
                debug!("TrackerState: Recorded Started event");
                TrackerEvent::None // Next announce has no special event
            }
            TrackerEvent::Completed => {
                self.completed_sent = true;
                info!("TrackerState: Recorded Completed event");
                TrackerEvent::None // Next announce has no special event
            }
            TrackerEvent::Stopped => {
                self.stopped_sent = true;
                info!("TrackerState: Recorded Stopped event (terminal)");
                TrackerEvent::Stopped // Stay in stopped state
            }
            TrackerEvent::None => TrackerEvent::None,
        };

        self.last_announce_time = Some(Instant::now());
        self.announce_count += 1;
    }

    /// Update interval values from tracker response
    ///
    /// Trackers return `interval` (recommended) and optionally `min interval`
    /// which constrain how often we may re-announce.
    pub fn update_intervals(&mut self, interval: Option<u64>, min_interval: Option<u64>) {
        if let Some(iv) = interval {
            // Sanity check: don't allow extremely short intervals (< 60s)
            self.interval_secs = iv.max(60);
            debug!("TrackerState: Updated interval to {}s", self.interval_secs);
        }
        if let Some(mi) = min_interval {
            // Sanity check: don't allow extremely short min intervals (< 30s)
            self.min_interval_secs = mi.max(30);
            debug!(
                "TrackerState: Updated min_interval to {}s",
                self.min_interval_secs
            );
        }
    }

    /// Transition to completed state - call when all pieces are downloaded
    pub fn mark_completed(&mut self) {
        if !self.completed_sent {
            self.current_event = TrackerEvent::Completed;
            info!("TrackerState: Transitioning to Completed event");
        }
    }

    /// Transition to stopped state - call on shutdown/cancel
    pub fn mark_stopped(&mut self) {
        if !self.stopped_sent {
            self.current_event = TrackerEvent::Stopped;
            info!("TrackerState: Transitioning to Stopped event");
        }
    }

    /// Reset state for a new download session
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Check if the tracker is in terminal (stopped) state
    pub fn is_stopped(&self) -> bool {
        self.stopped_sent
    }
}

/// Detect whether a tracker URL uses HTTPS scheme.
///
/// reqwest supports HTTPS natively with default features (native-tls),
/// but this function allows explicit checking for logging or configuration purposes.
///
/// # Arguments
/// * `url` - The tracker URL to check
///
/// # Returns
/// true if the URL starts with "https://", false otherwise
pub fn is_https_tracker(url: &str) -> bool {
    url.to_lowercase().starts_with("https://")
}

/// Build a reqwest::Client configured appropriately for the given URL scheme.
///
/// For HTTPS URLs, returns a standard client (reqwest handles TLS by default).
/// For HTTP URLs, returns a standard client without TLS requirements.
///
/// # Arguments
/// * `timeout_secs` - Request timeout in seconds
pub fn build_tracker_client(timeout_secs: u64) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_https_tracker_url_detected() {
        assert!(is_https_tracker("https://tracker.example.com/announce"));
        assert!(is_https_tracker("HTTPS://TRACKER.EXAMPLE.COM/ANNOUNCE"));
        assert!(!is_https_tracker("http://tracker.example.com/announce"));
        assert!(!is_https_tracker("udp://tracker.example.com:1337/announce"));
    }

    #[test]
    fn test_tracker_event_as_str() {
        assert_eq!(TrackerEvent::Started.as_str(), "started");
        assert_eq!(TrackerEvent::Stopped.as_str(), "stopped");
        assert_eq!(TrackerEvent::Completed.as_str(), "completed");
        assert_eq!(TrackerEvent::None.as_str(), "");
    }

    #[test]
    fn test_tracker_state_default() {
        let state = TrackerState::new();
        assert_eq!(state.current_event, TrackerEvent::Started);
        assert!(state.last_announce_time.is_none());
        assert_eq!(state.announce_count, 0);
        assert!(!state.completed_sent);
        assert!(!state.stopped_sent);
    }

    #[test]
    fn test_tracker_state_should_announce_initially() {
        let state = TrackerState::new();
        assert!(
            state.should_announce(),
            "Should be able to announce immediately on fresh state"
        );
    }

    #[test]
    fn test_tracker_state_sequence_started_to_completed() {
        let mut state = TrackerState::new();

        // Initial state should be Started
        assert_eq!(state.current_event, TrackerEvent::Started);

        // Record started event
        state.record_announce(TrackerEvent::Started);
        assert_eq!(state.announce_count, 1);
        assert_eq!(state.current_event, TrackerEvent::None); // Transitions to None

        // Now mark as completed
        state.mark_completed();
        assert_eq!(state.current_event, TrackerEvent::Completed);
        assert!(!state.completed_sent); // Not sent yet

        // Record completed event
        state.record_announce(TrackerEvent::Completed);
        assert_eq!(state.announce_count, 2);
        assert!(state.completed_sent);
        assert_eq!(state.current_event, TrackerEvent::None); // Transitions back to None
    }

    #[test]
    fn test_tracker_event_stopped_on_cancel() {
        let mut state = TrackerState::new();

        // Simulate started
        state.record_announce(TrackerEvent::Started);

        // Cancel the download
        state.mark_stopped();
        assert_eq!(state.current_event, TrackerEvent::Stopped);
        assert!(!state.stopped_sent); // Not sent yet

        // Record stopped event
        state.record_announce(TrackerEvent::Stopped);
        assert!(state.stopped_sent);
        // Stopped is terminal - should stay stopped
        assert_eq!(state.current_event, TrackerEvent::Stopped);
    }

    #[test]
    fn test_min_interval_respected() {
        let mut state = TrackerState::new();
        state.min_interval_secs = 10;

        // Initial announce
        assert!(state.should_announce());
        state.record_announce(TrackerEvent::Started);

        // Immediately after - should NOT be allowed
        assert!(
            !state.should_announce(),
            "Should not re-announce before min_interval"
        );

        // Check that secs_until_next_announce is positive
        let wait = state.secs_until_next_announce();
        assert!(wait > 0, "Should need to wait at least some seconds");
        assert!(wait <= 10, "Should not wait more than min_interval");
    }

    #[test]
    fn test_update_intervals() {
        let mut state = TrackerState::new();
        assert_eq!(state.interval_secs, 1800); // Default 30 min
        assert_eq!(state.min_interval_secs, 300); // Default 5 min

        state.update_intervals(Some(900), Some(300));
        assert_eq!(state.interval_secs, 900);
        assert_eq!(state.min_interval_secs, 300);

        // Test floor enforcement
        state.update_intervals(Some(10), Some(5));
        assert_eq!(state.interval_secs, 60); // Floor at 60s
        assert_eq!(state.min_interval_secs, 30); // Floor at 30s
    }

    #[test]
    fn test_build_tracker_client_succeeds() {
        let client = build_tracker_client(30);
        assert!(client.is_ok(), "Should build client with valid timeout");
    }

    #[test]
    fn test_is_stopped_terminal_state() {
        let mut state = TrackerState::new();
        assert!(!state.is_stopped());

        state.mark_stopped();
        state.record_announce(TrackerEvent::Stopped);
        assert!(state.is_stopped());
    }

    #[test]
    fn test_reset_clears_state() {
        let mut state = TrackerState::new();
        state.record_announce(TrackerEvent::Started);
        state.mark_completed();
        state.record_announce(TrackerEvent::Completed);

        state.reset();
        assert_eq!(state.current_event, TrackerEvent::Started);
        assert_eq!(state.announce_count, 0);
        assert!(!state.completed_sent);
        assert!(!state.stopped_sent);
    }

    #[test]
    fn test_multiple_completes_only_one_event() {
        let mut state = TrackerState::new();
        state.record_announce(TrackerEvent::Started);

        // First complete
        state.mark_completed();
        assert_eq!(state.current_event, TrackerEvent::Completed);
        state.record_announce(TrackerEvent::Completed);

        // Second complete attempt should not trigger another Completed event
        state.mark_completed();
        assert_eq!(
            state.current_event,
            TrackerEvent::None,
            "Second mark_completed should not change state"
        );
    }
}

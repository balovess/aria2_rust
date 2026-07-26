//! Shared types for tracker announce communication.
//!
//! Contains [`AnnounceEvent`], [`TrackerEntry`], and [`TrackerTier`] —
//! small types used across multiple sub-modules within this crate.

use std::time::{Duration, Instant};

// ======================================================================
// AnnounceEvent Enum (from C++ AnnounceTier::AnnounceEvent)
// ======================================================================

/// Announce event types matching C++ AnnounceTier::AnnounceEvent.
///
/// These events control the tracker announce state machine.
/// The transitions follow the C++ aria2 implementation exactly:
/// - `Started` -> `Downloading` (via nextEvent)
/// - `StartedAfterCompletion` -> `Seeding` (via nextEvent)
/// - `Stopped` -> `Halted` (via nextEvent or nextEventIfAfterStarted)
/// - `Completed` -> `Seeding` (via nextEvent or nextEventIfAfterStarted)
/// - `Downloading`, `Seeding`, `Halted` are stable states (no transition)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnounceEvent {
    /// Initial announce when download starts
    Started,
    /// Started after download already completed (prevent duplicate "completed" event)
    StartedAfterCompletion,
    /// Regular periodic announce during download
    Downloading,
    /// Announce when client is stopping/quitting
    Stopped,
    /// Announce when download just completed
    Completed,
    /// Regular announce during seeding phase
    Seeding,
    /// Terminal state after stopped
    Halted,
}

impl AnnounceEvent {
    /// Transition to the next event state (matching C++ AnnounceTier::nextEvent).
    ///
    /// State transitions:
    /// - `Started` -> `Downloading`
    /// - `StartedAfterCompletion` -> `Seeding`
    /// - `Stopped` -> `Halted`
    /// - `Completed` -> `Seeding`
    /// - `Downloading`, `Seeding`, `Halted` remain unchanged
    pub fn next_event(self) -> Self {
        match self {
            AnnounceEvent::Started => AnnounceEvent::Downloading,
            AnnounceEvent::StartedAfterCompletion => AnnounceEvent::Seeding,
            AnnounceEvent::Stopped => AnnounceEvent::Halted,
            AnnounceEvent::Completed => AnnounceEvent::Seeding,
            other => other,
        }
    }

    /// Transition event only if in STOPPED or COMPLETED state
    /// (matching C++ AnnounceTier::nextEventIfAfterStarted).
    ///
    /// This is called when a tracker announce fails and we need to advance
    /// the event state without going through the normal Started->Downloading
    /// transition (since we may have never successfully announced Started).
    pub fn next_event_if_after_started(self) -> Self {
        match self {
            AnnounceEvent::Stopped => AnnounceEvent::Halted,
            AnnounceEvent::Completed => AnnounceEvent::Seeding,
            other => other,
        }
    }

    /// Returns true if this event state allows sending a "stopped" event.
    ///
    /// Matching C++ FindStoppedAllowedTier: DOWNLOADING, STOPPED, COMPLETED, SEEDING
    pub fn accepts_stopped_event(self) -> bool {
        matches!(
            self,
            AnnounceEvent::Downloading
                | AnnounceEvent::Stopped
                | AnnounceEvent::Completed
                | AnnounceEvent::Seeding
        )
    }

    /// Returns true if this event state allows sending a "completed" event.
    ///
    /// Matching C++ FindCompletedAllowedTier: DOWNLOADING, COMPLETED
    pub fn accepts_completed_event(self) -> bool {
        matches!(self, AnnounceEvent::Downloading | AnnounceEvent::Completed)
    }

    /// Convert to the event string for tracker URL parameter.
    ///
    /// Both Started and StartedAfterCompletion map to "started" since
    /// trackers don't distinguish between these two internal states.
    pub fn as_event_string(self) -> &'static str {
        match self {
            AnnounceEvent::Started | AnnounceEvent::StartedAfterCompletion => "started",
            AnnounceEvent::Stopped => "stopped",
            AnnounceEvent::Completed => "completed",
            AnnounceEvent::Downloading | AnnounceEvent::Seeding | AnnounceEvent::Halted => "",
        }
    }
}

// ======================================================================
// TrackerEntry (Rust Improvement: Reliability Scoring)
// ======================================================================

/// A single tracker entry with health tracking and reliability scoring.
///
/// This is a Rust improvement over the C++ implementation that adds
/// reliability scoring and exponential backoff to individual trackers.
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

// ======================================================================
// TrackerTier (Rust Improvement: Reliability-Based Selection)
// ======================================================================

/// A tier of trackers tried in order with reliability-based selection.
///
/// This is a Rust improvement that uses reliability scoring to select
/// the best available tracker within a tier, rather than just
/// sequential iteration.
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

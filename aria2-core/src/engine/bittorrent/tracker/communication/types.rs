//! Types shared across tracker announce state-machine modules.

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

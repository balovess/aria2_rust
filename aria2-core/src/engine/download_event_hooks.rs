//! Download lifecycle event hooks.
//!
//! Mirrors C++ `util::executeHookByOptName()` and the
//! `PREF_ON_DOWNLOAD_START / COMPLETE / ERROR / PAUSE / STOP /
//! BT_DOWNLOAD_COMPLETE` hook infrastructure.
//!
//! In the original C++ aria2, each lifecycle transition fires a user-configured
//! shell command with arguments: `<command> <GID_hex> <numFiles> <firstFilePath>`.
//! The Rust implementation replicates this via async `tokio::process::Command`
//! spawning, which works on both Unix and Windows.
//!
//! # Events
//!
//! | Event              | C++ Preference              | Fired when                     |
//! |--------------------|-----------------------------|--------------------------------|
//! | `Start`            | `PREF_ON_DOWNLOAD_START`   | Group promoted to active       |
//! | `Complete`         | `PREF_ON_DOWNLOAD_COMPLETE`| Download finished successfully |
//! | `Error`            | `PREF_ON_DOWNLOAD_ERROR`   | Download finished with error   |
//! | `Pause`            | `PREF_ON_DOWNLOAD_PAUSE`   | Download paused by user        |
//! | `Stop`             | `PREF_ON_DOWNLOAD_STOP`    | Download stopped (not complete)|
//! | `BtComplete`       | `PREF_ON_BT_DOWNLOAD_COMPLETE` | BT download fully complete |
//!
//! # Architecture
//!
//! The bus has **two independent sinks**. A single `fire_*` call feeds both;
//! neither sink may gate the other.
//!
//! ```text
//! Group state transition / engine loop / demotion path
//!   │
//!   ├─ on promotion  → fire DownloadEvent::Start
//!   ├─ on demotion   → fire DownloadEvent::Complete / Error / Pause / Stop
//!   └─ on BT done    → fire DownloadEvent::BtComplete
//!         │
//!         ▼
//!   DownloadEventHooks::fire_event()
//!     ├─ sink 1: shell hook (OPTIONAL — only when --on-download-* is set)
//!     │    ├─ resolve hook command from DownloadOptions / global hooks
//!     │    ├─ spawn tokio::process::Command with (gid, numFiles, firstFilePath)
//!     │    └─ log result (fire-and-forget, non-blocking)
//!     │
//!     └─ sink 2: observers (ALWAYS — never gated on sink 1)
//!          └─ DownloadEventListener::on_download_event(event, gid)
//! ```
//!
//! # Observers (`DownloadEventListener`)
//!
//! Sink 2 mirrors C++ `Notifier::addDownloadEventListener()` /
//! `Notifier::notifyDownloadEvent()`. It exists so layers *above* `aria2-core`
//! (notably the JSON-RPC WebSocket notifier, which emits
//! `aria2.onDownloadComplete` / `aria2.onDownloadError` /
//! `aria2.onBtDownloadComplete`) can observe download lifecycle transitions
//! **without `aria2-core` ever depending on `aria2-rpc`**. The binary crate
//! `aria2` owns both crates and installs the adapter, keeping the dependency
//! direction `aria2 → {aria2-core, aria2-rpc}` intact.
//!
//! Observer notification is deliberately synchronous and cheap: implementors
//! must not block (the recommended pattern is an `mpsc` send consumed by a
//! dedicated task).

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, OnceLock, atomic::AtomicU64};

use tokio::sync::broadcast;

use crate::request::request_group::{GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

// ============================================================================
// Download event types
// ============================================================================

/// Download lifecycle events that can trigger user-configured shell hooks.
///
/// Each variant maps to a C++ `PREF_ON_DOWNLOAD_*` preference and is fired
/// at a specific point in the download lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DownloadEvent {
    /// Fired when a download is promoted from reserved to active.
    /// C++: `PREF_ON_DOWNLOAD_START`
    Start,
    /// Fired when a download completes successfully.
    /// C++: `PREF_ON_DOWNLOAD_COMPLETE`
    Complete,
    /// Fired when a download finishes with an error.
    /// C++: `PREF_ON_DOWNLOAD_ERROR`
    Error,
    /// Fired when a download is paused by user request.
    /// C++: `PREF_ON_DOWNLOAD_PAUSE`
    Pause,
    /// Fired when a download is stopped (not complete, not error).
    /// C++: `PREF_ON_DOWNLOAD_STOP`
    Stop,
    /// Fired when a BitTorrent payload download completes. Seeding may
    /// continue after this event.
    /// C++: `PREF_ON_BT_DOWNLOAD_COMPLETE`
    BtComplete,
}

/// Describes the transition from a completed metadata task to download groups
/// that can consume its resolved metadata.
///
/// This is deliberately separate from [`DownloadEvent`]. The latter mirrors
/// aria2's lifecycle hooks and the standard RPC notifications, while metadata
/// resolution is a Rust-library observation that has no corresponding
/// standard aria2 status or notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataResolvedEvent {
    /// GID of the task that produced the metadata.
    pub metadata_gid: GroupId,
    /// GIDs ready to consume the resolved metadata. For an in-place magnet
    /// download this contains the same GID as `metadata_gid`; for a
    /// metadata-only request it is empty. For a post-download metadata task,
    /// these are the values exposed as `followedBy` for `metadata_gid`.
    pub download_gids: Vec<GroupId>,
}

/// Event stream item for Rust-library consumers.
///
/// The lifecycle variant keeps the canonical hex GID used by aria2's RPC
/// surface. Metadata resolution is represented separately because it is a
/// transient library event rather than a standard aria2 download status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadNotification {
    Lifecycle { event: DownloadEvent, gid: String },
    MetadataResolved(MetadataResolvedEvent),
}

/// A loss-aware stream of lifecycle and metadata notifications.
///
/// This is backed by a bounded broadcast channel. A slow consumer must handle
/// [`broadcast::error::RecvError::Lagged`] by taking a fresh snapshot from its
/// [`DownloadHandle`](super::download_manager::DownloadHandle), rather than
/// assuming every intermediate progress event was retained.
pub struct DownloadEventStream {
    receiver: broadcast::Receiver<DownloadNotification>,
}

impl DownloadEventStream {
    pub async fn recv(&mut self) -> Result<DownloadNotification, broadcast::error::RecvError> {
        self.receiver.recv().await
    }

    /// Wait for a terminal lifecycle event for `gid`.
    ///
    /// This consumes unrelated notifications from this receiver while
    /// waiting. `Stop` is included because aria2 uses it for a download that
    /// was stopped without completing or failing. The helper is deliberately
    /// event-driven; callers that need the final snapshot can use the matching
    /// [`DownloadHandle`](super::download_manager::DownloadHandle) after it
    /// returns.
    pub async fn recv_terminal_for(
        &mut self,
        gid: GroupId,
    ) -> Result<DownloadEvent, broadcast::error::RecvError> {
        let gid = gid.to_hex_string();
        loop {
            match self.recv().await? {
                DownloadNotification::Lifecycle {
                    event,
                    gid: event_gid,
                } if event_gid == gid && event.is_terminal() => {
                    return Ok(event);
                }
                DownloadNotification::Lifecycle { .. }
                | DownloadNotification::MetadataResolved(_) => {}
            }
        }
    }

    /// Wait for metadata resolution involving `gid`.
    ///
    /// A metadata task may resolve into the same GID (for a magnet download)
    /// or into one or more child download GIDs (for a metadata-backed graph).
    /// This helper filters both forms while preserving the broadcast channel's
    /// lag error so callers can resubscribe and refresh their snapshots.
    pub async fn recv_metadata_for(
        &mut self,
        gid: GroupId,
    ) -> Result<MetadataResolvedEvent, broadcast::error::RecvError> {
        loop {
            match self.recv().await? {
                DownloadNotification::MetadataResolved(event)
                    if event.metadata_gid == gid || event.download_gids.contains(&gid) =>
                {
                    return Ok(event);
                }
                DownloadNotification::Lifecycle { .. }
                | DownloadNotification::MetadataResolved(_) => {}
            }
        }
    }
}

impl MetadataResolvedEvent {
    /// Create a metadata-resolution event from a parent GID and its children.
    pub fn new(metadata_gid: GroupId, download_gids: Vec<GroupId>) -> Self {
        Self {
            metadata_gid,
            download_gids,
        }
    }
}

impl DownloadEvent {
    /// Returns the human-readable name of this event.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Start => "on-download-start",
            Self::Complete => "on-download-complete",
            Self::Error => "on-download-error",
            Self::Pause => "on-download-pause",
            Self::Stop => "on-download-stop",
            Self::BtComplete => "on-bt-download-complete",
        }
    }

    /// Whether this event can legitimately fire at most **once** for a given
    /// download.
    ///
    /// `Complete`, `Error` and `BtComplete` are one-shot transitions: a
    /// download completes once, fails once, and finishes its BT payload once.
    /// They are emitted from more than one place (the group state transition
    /// itself *and* the engine-loop demotion path), so observer notification is
    /// de-duplicated on `(gid, event)` for these variants.
    ///
    /// `Start` and `Pause` may repeat across pause/unpause cycles and `Stop`
    /// is re-emitted by the RPC layer on explicit removal, so those are never
    /// de-duplicated.
    pub fn is_once_per_download(&self) -> bool {
        matches!(self, Self::Complete | Self::Error | Self::BtComplete)
    }

    /// Whether this notification marks the task as stopped for lifecycle
    /// observers. `BtComplete` only marks payload completion; seeding may
    /// continue until a later terminal event.
    ///
    /// `Stop` is terminal for the current task run even though aria2 may emit
    /// it again when a caller explicitly removes the task. It is therefore
    /// terminal for waiters but not a one-shot event for de-duplication.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Stop | Self::Complete | Self::Error)
    }
}

// ============================================================================
// DownloadEventListener — observer interface for out-of-crate subscribers
// ============================================================================

/// Observer notified of every download lifecycle transition.
///
/// Mirrors C++ `aria2::DownloadEventListener` (see `src/Notifier.h`), whose
/// `onEvent(DownloadEvent, const RequestGroup*)` is invoked by
/// `Notifier::notifyDownloadEvent()`.
///
/// # Contract
///
/// * Implementations **must not block**. Notification happens inline on the
///   thread that performed the state transition, which may be holding
///   unrelated locks. Forward the event to a channel and do the real work on
///   a dedicated task.
/// * Implementations **must not panic**. A panicking listener would unwind
///   through the download engine.
/// * `gid` is the canonical 16-digit lowercase hex GID (`GroupId::to_hex_string`),
///   i.e. exactly the identifier the JSON-RPC layer exposes to clients.
pub trait DownloadEventListener: Send + Sync {
    /// Called for every fired download event.
    fn on_download_event(&self, event: DownloadEvent, gid: &str);

    /// Called for lifecycle events emitted by a request manager.
    ///
    /// The default keeps existing process-wide observers compatible. A
    /// manager-scoped observer can override this method to distinguish equal
    /// GIDs owned by different managers.
    fn on_download_event_scoped(&self, event: DownloadEvent, gid: &str, _scope_id: u64) {
        self.on_download_event(event, gid);
    }

    /// Called after a metadata task has produced and queued its download
    /// groups.
    ///
    /// This callback is intentionally not mapped to a standard aria2 RPC
    /// notification. Implementors that only need lifecycle events can keep
    /// the default no-op implementation.
    fn on_metadata_resolved(&self, _event: &MetadataResolvedEvent) {}

    /// Whether the observer still has a live destination.
    ///
    /// Process-wide buses can outlive a server instance. Implementations that
    /// hold a weak destination should return false after that destination is
    /// dropped so the bus can release the stale observer.
    fn is_alive(&self) -> bool {
        true
    }
}

/// Stable identifier for one registered lifecycle listener.
pub type DownloadEventListenerId = u64;

// ============================================================================
// DownloadEventHooks — manages hook registration and execution
// ============================================================================

/// Maximum number of `(gid, event)` pairs retained for one-shot event
/// de-duplication before the bookkeeping is rotated.
///
/// A long-running daemon must not accumulate one entry per download forever,
/// so the set is kept in two generations: once `current` reaches this cap it
/// is demoted to `previous` and a fresh `current` is started. Lookups consult
/// both generations, so recently-seen events are never forgotten prematurely.
const DEDUP_GENERATION_CAPACITY: usize = 4096;

/// Number of metadata-resolution events retained for late library observers.
///
/// Metadata resolution is a one-shot transition, so a bounded replay history
/// is enough to bridge the small gap between task submission and a caller
/// starting its await. The event stream remains loss-aware for older entries.
const METADATA_HISTORY_CAPACITY: usize = 1024;

/// Two-generation bounded de-duplication ledger for one-shot events.
#[derive(Default)]
struct OneShotLedger {
    current: HashSet<(Option<u64>, String, DownloadEvent)>,
    previous: HashSet<(Option<u64>, String, DownloadEvent)>,
}

impl OneShotLedger {
    /// Record `key` and report whether this is the first time it was seen.
    ///
    /// Returns `true` when the caller should proceed with the notification,
    /// `false` when the event was already delivered.
    fn claim(&mut self, key: (Option<u64>, String, DownloadEvent)) -> bool {
        if self.current.contains(&key) || self.previous.contains(&key) {
            return false;
        }
        if self.current.len() >= DEDUP_GENERATION_CAPACITY {
            self.previous = std::mem::take(&mut self.current);
        }
        self.current.insert(key);
        true
    }
}

/// Manages download event hooks and provides fire-and-forget execution.
///
/// The hook system mirrors C++ aria2's `--on-download-start`,
/// `--on-download-complete`, etc. CLI options. Each option holds a shell
/// command to execute when the corresponding event fires.
///
/// Hook commands are resolved from `DownloadOptions` fields at fire time,
/// allowing per-group hook customization. If no command is configured for
/// an event, nothing is executed.
///
/// # Thread Safety
///
/// `DownloadEventHooks` is `Send + Sync` — it holds no mutable state.
/// Hook execution is fire-and-forget via `tokio::spawn`, so the caller
/// never blocks.
pub struct DownloadEventHooks {
    /// Global hooks applied to all downloads unless overridden per-group.
    /// Maps event type → shell command string.
    global_hooks: Arc<std::sync::RwLock<GlobalHookMap>>,
    /// Registered observers (sink 2). Notified for **every** fired event,
    /// regardless of whether a shell hook command is configured.
    listeners: std::sync::RwLock<Vec<(DownloadEventListenerId, Arc<dyn DownloadEventListener>)>>,
    next_listener_id: AtomicU64,
    /// De-duplication ledger for one-shot events
    /// ([`DownloadEvent::is_once_per_download`]).
    one_shot: std::sync::RwLock<OneShotLedger>,
    /// Bounded event stream for library consumers that do not need to
    /// implement the synchronous listener trait.
    notifications: broadcast::Sender<DownloadNotification>,
    /// Recent structured metadata transitions for late high-level waiters.
    metadata_history: std::sync::RwLock<VecDeque<MetadataResolvedEvent>>,
}

/// Type alias for the global hook map.
type GlobalHookMap = Vec<(DownloadEvent, String)>;

/// Process-wide hook bus, lazily created on first use.
static SHARED_HOOKS: OnceLock<Arc<DownloadEventHooks>> = OnceLock::new();

mod bus;

// ============================================================================
// Helper: extract hook context from a RequestGroup for demotion-time events
// ============================================================================

/// Pre-extracted context needed to fire stop/complete/error hooks after
/// the RequestGroup may no longer be available.
///
/// In C++ aria2, `executeStopHook()` is called while the group is still
/// alive. In Rust, we extract this data before demotion releases resources.
#[derive(Debug, Clone)]
pub struct DownloadEventContext {
    /// GID as hex string (for hook command argument).
    pub gid_hex: String,
    /// Number of requested file entries.
    pub num_files: usize,
    /// Path of the first requested file.
    pub first_file_path: String,
    /// Per-group hook commands (resolved at extraction time).
    pub on_download_complete: Option<String>,
    pub on_download_error: Option<String>,
    pub on_download_pause: Option<String>,
    pub on_download_stop: Option<String>,
    pub on_bt_download_complete: Option<String>,
}

impl DownloadEventContext {
    /// Extract event context from a RequestGroup before it is demoted.
    ///
    /// Must be called BEFORE `demote_group()` releases the download context.
    pub fn from_group(group: &RequestGroup) -> Self {
        // Canonical 16-digit hex, matching C++ `GroupId::toHex()` and the GID
        // format used by the JSON-RPC layer.
        let gid_hex = group.gid().to_hex_string();

        let (num_files, first_file_path) =
            if let Some(dctx) = group.download_context.recover().as_ref() {
                let num = dctx.count_requested_file_entry();
                let path = dctx.first_file_path().unwrap_or_default().to_string();
                (num, path)
            } else {
                (0, String::new())
            };

        let opts = group.options_arc();

        Self {
            gid_hex,
            num_files,
            first_file_path,
            on_download_complete: opts.on_download_complete.clone(),
            on_download_error: opts.on_download_error.clone(),
            on_download_pause: opts.on_download_pause.clone(),
            on_download_stop: opts.on_download_stop.clone(),
            on_bt_download_complete: opts.on_bt_download_complete.clone(),
        }
    }
}

/// Determine which event to fire for a stopped download.
///
/// Mirrors C++ `executeStopHook()` logic:
/// - If result == FINISHED → `Complete`
/// - If result != IN_PROGRESS && != REMOVED → `Error`
/// - Otherwise → `Stop`
///
/// Also fires `Pause` when the group was pause-requested.
pub fn determine_stop_event(
    is_complete: bool,
    is_error: bool,
    is_paused: bool,
) -> Option<DownloadEvent> {
    if is_paused {
        return Some(DownloadEvent::Pause);
    }
    if is_complete {
        return Some(DownloadEvent::Complete);
    }
    if is_error {
        return Some(DownloadEvent::Error);
    }
    // Not paused, not complete, not error → generic stop
    Some(DownloadEvent::Stop)
}

#[cfg(test)]
mod tests;

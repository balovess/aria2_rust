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

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use tracing::{debug, info, warn};

use crate::request::request_group::RequestGroup;
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
    /// Fired when a BitTorrent download finishes completely
    /// (all files downloaded and seeding complete).
    /// C++: `PREF_ON_BT_DOWNLOAD_COMPLETE`
    BtComplete,
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
    /// `Complete`, `Error` and `BtComplete` are terminal one-shot transitions:
    /// a download completes once, fails once, and finishes its BT payload
    /// once. They are emitted from more than one place (the group state
    /// transition itself *and* the engine-loop demotion path), so observer
    /// notification is de-duplicated on `(gid, event)` for these variants.
    ///
    /// `Start` and `Pause` may repeat across pause/unpause cycles and `Stop`
    /// is re-emitted by the RPC layer on explicit removal, so those are never
    /// de-duplicated.
    pub fn is_once_per_download(&self) -> bool {
        matches!(self, Self::Complete | Self::Error | Self::BtComplete)
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
}

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

/// Two-generation bounded de-duplication ledger for one-shot events.
#[derive(Default)]
struct OneShotLedger {
    current: HashSet<(String, DownloadEvent)>,
    previous: HashSet<(String, DownloadEvent)>,
}

impl OneShotLedger {
    /// Record `key` and report whether this is the first time it was seen.
    ///
    /// Returns `true` when the caller should proceed with the notification,
    /// `false` when the event was already delivered.
    fn claim(&mut self, key: (String, DownloadEvent)) -> bool {
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
    listeners: std::sync::RwLock<Vec<Arc<dyn DownloadEventListener>>>,
    /// De-duplication ledger for one-shot events
    /// ([`DownloadEvent::is_once_per_download`]).
    one_shot: std::sync::RwLock<OneShotLedger>,
}

/// Type alias for the global hook map.
type GlobalHookMap = Vec<(DownloadEvent, String)>;

/// Process-wide hook bus, lazily created on first use.
static SHARED_HOOKS: OnceLock<Arc<DownloadEventHooks>> = OnceLock::new();

impl DownloadEventHooks {
    /// Create a new hook manager with no global hooks and no listeners.
    pub fn new() -> Self {
        Self {
            global_hooks: Arc::new(std::sync::RwLock::new(Vec::new())),
            listeners: std::sync::RwLock::new(Vec::new()),
            one_shot: std::sync::RwLock::new(OneShotLedger::default()),
        }
    }

    /// Return the process-wide hook bus.
    ///
    /// Download lifecycle transitions happen deep inside individual
    /// `Command` implementations that have no reference to the engine, and the
    /// engine itself exists in two flavours (the v1 `run()` command-dispatch
    /// loop that production uses today and the v2 `run_v2()` group-management
    /// loop). A process-wide bus is what lets a single listener registration
    /// observe *all* of them, and mirrors C++ where `Notifier` is a
    /// `SingletonHolder`. The same pattern is already used in this crate for
    /// `filesystem::file_allocation_man::shared()` and
    /// `checksum::check_integrity::man::shared()`.
    pub fn shared() -> &'static Arc<DownloadEventHooks> {
        SHARED_HOOKS.get_or_init(|| Arc::new(DownloadEventHooks::new()))
    }

    /// Register an observer that is notified of every fired download event.
    ///
    /// Mirrors C++ `Notifier::addDownloadEventListener()`. Registration is
    /// additive and there is no removal API, matching C++ where listeners
    /// live for the lifetime of the process.
    pub fn add_listener(&self, listener: Arc<dyn DownloadEventListener>) {
        let mut listeners = self.listeners.recover_mut();
        listeners.push(listener);
        debug!(
            count = listeners.len(),
            "Registered download event listener"
        );
    }

    /// Number of currently registered observers (primarily for tests).
    pub fn listener_count(&self) -> usize {
        self.listeners.recover().len()
    }

    /// Notify every registered observer of `event` for `gid`.
    ///
    /// `gid` must be the canonical 16-digit hex GID
    /// ([`GroupId::to_hex_string`](crate::request::request_group::GroupId::to_hex_string)).
    ///
    /// This is the *unconditional* sink: it is deliberately independent of
    /// whether the user configured an `--on-download-*` shell command, because
    /// RPC clients (AriaNg, webui-aria2, ...) rely on the corresponding
    /// WebSocket notifications to ever mark a download as finished.
    ///
    /// One-shot events are de-duplicated so that a transition observed both at
    /// the `RequestGroup` state change and again at engine-loop demotion is
    /// only delivered once.
    pub fn notify_listeners(&self, event: DownloadEvent, gid: &str) {
        // Snapshot under a short read lock so a listener callback can never
        // deadlock against `add_listener`.
        let listeners: Vec<Arc<dyn DownloadEventListener>> = {
            let guard = self.listeners.recover();
            if guard.is_empty() {
                // Nothing to do — and importantly, do not pollute the
                // de-duplication ledger when no one is listening.
                return;
            }
            guard.clone()
        };

        if event.is_once_per_download()
            && !self.one_shot.recover_mut().claim((gid.to_string(), event))
        {
            debug!(
                event = event.name(),
                gid, "Suppressed duplicate one-shot download event"
            );
            return;
        }

        debug!(
            event = event.name(),
            gid,
            listeners = listeners.len(),
            "Notifying download event listeners"
        );
        for listener in listeners {
            listener.on_download_event(event, gid);
        }
    }

    /// Register a global hook for the given event.
    ///
    /// Global hooks are used when no per-group hook is configured.
    /// If a global hook is already registered for this event, it is replaced.
    pub fn set_global_hook(&self, event: DownloadEvent, command: String) {
        let mut hooks = self.global_hooks.recover_mut();
        if let Some(entry) = hooks.iter_mut().find(|(e, _)| *e == event) {
            entry.1 = command;
        } else {
            hooks.push((event, command));
        }
        debug!(
            event = event.name(),
            "Registered global download event hook"
        );
    }

    /// Remove a global hook for the given event.
    pub fn remove_global_hook(&self, event: DownloadEvent) {
        let mut hooks = self.global_hooks.recover_mut();
        hooks.retain(|(e, _)| *e != event);
    }

    /// Fire a download event hook for the given group.
    ///
    /// This method resolves the hook command from (in priority order):
    /// 1. Per-group `DownloadOptions` fields (e.g. `on_download_start`)
    /// 2. Global hooks registered via `set_global_hook()`
    ///
    /// If a command is found, it is spawned as an async child process
    /// with arguments: `<command> <GID_hex> <numFiles> <firstFilePath>`
    ///
    /// This is fire-and-forget — the caller is not blocked.
    /// Errors are logged but do not propagate.
    ///
    /// Mirrors C++ `util::executeHookByOptName(group, option, pref)` followed
    /// by `Notifier::notifyDownloadEvent()` — C++ performs both, in that
    /// order, at every hook site.
    pub fn fire_event(&self, event: DownloadEvent, group: &RequestGroup) {
        // C++ `util::executeHook()` formats the GID with `GroupId::toHex()`,
        // i.e. a zero-padded 16-digit lowercase hex string. Use the same
        // canonical form here so hook arguments match the original *and* so
        // observers receive exactly the GID the JSON-RPC layer publishes.
        let gid_hex = group.gid().to_hex_string();

        // ── Sink 1: user-configured shell hook (optional) ──────────────────
        match self.resolve_hook_command(event, group) {
            Some(command) if !command.is_empty() => {
                let (num_files, first_file_path) = Self::extract_file_info(group);
                self.spawn_hook(&command, &gid_hex, num_files, &first_file_path, event);
            }
            _ => {
                debug!(
                    event = event.name(),
                    gid = %gid_hex,
                    "No hook command configured, skipping shell hook"
                );
            }
        }

        // ── Sink 2: observers (unconditional) ─────────────────────────────
        // MUST run even when no shell hook is configured: the RPC WebSocket
        // notifications (aria2.onDownloadComplete / onDownloadError /
        // onBtDownloadComplete) are delivered through this path, and gating
        // them on an unrelated `--on-download-*` CLI option would leave every
        // default deployment without completion notifications.
        self.notify_listeners(event, &gid_hex);
    }

    /// Fire a download event hook using direct parameters instead of a group.
    ///
    /// Useful when the RequestGroup is no longer available (e.g. after demotion)
    /// but the relevant data has already been extracted.
    ///
    /// `gid_hex` must be the canonical 16-digit hex GID (as produced by
    /// [`DownloadEventContext::from_group`]).
    pub fn fire_event_with_params(
        &self,
        event: DownloadEvent,
        gid_hex: &str,
        num_files: usize,
        first_file_path: &str,
        command: &str,
    ) {
        // Sink 1 — only when a shell command was actually configured.
        if !command.is_empty() {
            self.spawn_hook(command, gid_hex, num_files, first_file_path, event);
        }
        // Sink 2 — always. See `fire_event` for why this must not be gated.
        self.notify_listeners(event, gid_hex);
    }

    /// Extract `(numFiles, firstFilePath)` hook arguments from a group.
    ///
    /// Mirrors the C++ `executeHookByOptName` body, which reads
    /// `group->getDownloadContext()->getNumFileEntries()` and the first
    /// requested file entry's path.
    fn extract_file_info(group: &RequestGroup) -> (usize, String) {
        if let Some(dctx) = group.download_context.recover().as_ref() {
            let num = dctx.count_requested_file_entry();
            let path = dctx.first_file_path().unwrap_or_default().to_string();
            (num, path)
        } else {
            (0, String::new())
        }
    }

    /// Resolve the hook command for an event from per-group or global config.
    fn resolve_hook_command(&self, event: DownloadEvent, group: &RequestGroup) -> Option<String> {
        // Priority 1: per-group options
        let opts = group.options_arc();
        let per_group = match event {
            DownloadEvent::Start => opts.on_download_start.as_ref(),
            DownloadEvent::Complete => opts.on_download_complete.as_ref(),
            DownloadEvent::Error => opts.on_download_error.as_ref(),
            DownloadEvent::Pause => opts.on_download_pause.as_ref(),
            DownloadEvent::Stop => opts.on_download_stop.as_ref(),
            DownloadEvent::BtComplete => opts.on_bt_download_complete.as_ref(),
        };

        if let Some(cmd) = per_group {
            if !cmd.is_empty() {
                return Some(cmd.clone());
            }
        }

        // Priority 2: global hooks
        let hooks = self.global_hooks.recover();
        for (e, cmd) in hooks.iter() {
            if *e == event && !cmd.is_empty() {
                return Some(cmd.clone());
            }
        }

        None
    }

    /// Spawn the hook command as an async child process.
    ///
    /// Mirrors C++ `util::executeHook()` which forks on Unix or
    /// `CreateProcessW` on Windows. The Rust version uses
    /// `tokio::process::Command` which handles both platforms.
    fn spawn_hook(
        &self,
        command: &str,
        gid_hex: &str,
        num_files: usize,
        first_file_path: &str,
        event: DownloadEvent,
    ) {
        info!(
            event = event.name(),
            command,
            gid = gid_hex,
            num_files,
            path = first_file_path,
            "Executing download event hook"
        );

        // Split command into program + args for proper process spawning.
        // C++ uses execlp(command, command, gidStr, numFilesStr, firstFilename, NULL)
        // which searches PATH and passes the arguments directly.
        let num_files_str = num_files.to_string();

        // On Windows, handle .bat/.cmd files specially (like C++ does).
        #[cfg(target_os = "windows")]
        let result = {
            let is_batch = command.to_lowercase().ends_with(".bat")
                || command.to_lowercase().ends_with(".cmd");

            if is_batch {
                let cmd_exe = std::env::var("windir")
                    .map(|w| format!("{}\\system32\\cmd.exe", w))
                    .unwrap_or_else(|_| "cmd.exe".to_string());

                let mut cmd = tokio::process::Command::new(&cmd_exe);
                cmd.args(["/c", command, gid_hex, &num_files_str, first_file_path])
                    .creation_flags(0x08000000); // CREATE_NO_WINDOW
                cmd.spawn()
            } else {
                tokio::process::Command::new(command)
                    .args([gid_hex, &num_files_str, first_file_path])
                    .spawn()
            }
        };

        #[cfg(not(target_os = "windows"))]
        let result = {
            tokio::process::Command::new(command)
                .args([gid_hex, &num_files_str, first_file_path])
                .spawn()
        };

        match result {
            Ok(mut child) => {
                if let Some(id) = child.id() {
                    debug!(event = event.name(), pid = id, "Hook process spawned");
                }
                // Fire-and-forget: await the child in a detached task
                // to reap zombies on Unix.
                let event_name = event.name().to_string();
                tokio::spawn(async move {
                    match child.wait().await {
                        Ok(status) => {
                            if !status.success() {
                                warn!(
                                    event = %event_name,
                                    exit = ?status.code(),
                                    "Hook process exited with non-zero status"
                                );
                            }
                        }
                        Err(e) => {
                            warn!(
                                event = %event_name,
                                error = %e,
                                "Failed to wait for hook process"
                            );
                        }
                    }
                });
            }
            Err(e) => {
                warn!(
                    event = event.name(),
                    command,
                    error = %e,
                    "Failed to spawn hook process"
                );
            }
        }
    }
}

impl Default for DownloadEventHooks {
    fn default() -> Self {
        Self::new()
    }
}

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
mod tests {
    use super::*;

    #[test]
    fn test_determine_stop_event_complete() {
        assert_eq!(
            determine_stop_event(true, false, false),
            Some(DownloadEvent::Complete)
        );
    }

    #[test]
    fn test_determine_stop_event_error() {
        assert_eq!(
            determine_stop_event(false, true, false),
            Some(DownloadEvent::Error)
        );
    }

    #[test]
    fn test_determine_stop_event_paused() {
        assert_eq!(
            determine_stop_event(false, false, true),
            Some(DownloadEvent::Pause)
        );
    }

    #[test]
    fn test_determine_stop_event_generic_stop() {
        assert_eq!(
            determine_stop_event(false, false, false),
            Some(DownloadEvent::Stop)
        );
    }

    #[test]
    fn test_event_names() {
        assert_eq!(DownloadEvent::Start.name(), "on-download-start");
        assert_eq!(DownloadEvent::Complete.name(), "on-download-complete");
        assert_eq!(DownloadEvent::Error.name(), "on-download-error");
        assert_eq!(DownloadEvent::Pause.name(), "on-download-pause");
        assert_eq!(DownloadEvent::Stop.name(), "on-download-stop");
        assert_eq!(DownloadEvent::BtComplete.name(), "on-bt-download-complete");
    }

    #[test]
    fn test_global_hook_registration() {
        let hooks = DownloadEventHooks::new();
        hooks.set_global_hook(DownloadEvent::Start, "/usr/bin/echo".to_string());
        hooks.set_global_hook(DownloadEvent::Complete, "/usr/bin/notify".to_string());

        let guard = hooks.global_hooks.recover();
        assert_eq!(guard.len(), 2);
        assert!(guard.iter().any(|(e, _)| *e == DownloadEvent::Start));
        assert!(guard.iter().any(|(e, _)| *e == DownloadEvent::Complete));
    }

    #[test]
    fn test_global_hook_replacement() {
        let hooks = DownloadEventHooks::new();
        hooks.set_global_hook(DownloadEvent::Start, "/usr/bin/old".to_string());
        hooks.set_global_hook(DownloadEvent::Start, "/usr/bin/new".to_string());

        let guard = hooks.global_hooks.recover();
        let start_hook = guard.iter().find(|(e, _)| *e == DownloadEvent::Start);
        assert_eq!(start_hook.unwrap().1, "/usr/bin/new");
    }

    #[test]
    fn test_global_hook_removal() {
        let hooks = DownloadEventHooks::new();
        hooks.set_global_hook(DownloadEvent::Start, "/usr/bin/echo".to_string());
        hooks.remove_global_hook(DownloadEvent::Start);

        let guard = hooks.global_hooks.recover();
        assert!(guard.is_empty());
    }

    // ── Observer (DownloadEventListener) tests ───────────────────────────

    /// Records every event it is notified about.
    #[derive(Default)]
    struct RecordingListener {
        seen: std::sync::Mutex<Vec<(DownloadEvent, String)>>,
    }

    impl RecordingListener {
        fn events(&self) -> Vec<(DownloadEvent, String)> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl DownloadEventListener for RecordingListener {
        fn on_download_event(&self, event: DownloadEvent, gid: &str) {
            self.seen.lock().unwrap().push((event, gid.to_string()));
        }
    }

    #[test]
    fn test_add_listener_tracks_registration() {
        let hooks = DownloadEventHooks::new();
        assert_eq!(hooks.listener_count(), 0);
        hooks.add_listener(Arc::new(RecordingListener::default()));
        hooks.add_listener(Arc::new(RecordingListener::default()));
        assert_eq!(hooks.listener_count(), 2);
    }

    #[test]
    fn test_notify_listeners_delivers_to_all_listeners() {
        let hooks = DownloadEventHooks::new();
        let a = Arc::new(RecordingListener::default());
        let b = Arc::new(RecordingListener::default());
        hooks.add_listener(a.clone());
        hooks.add_listener(b.clone());

        hooks.notify_listeners(DownloadEvent::Complete, "00000000000000ff");

        let expected = vec![(DownloadEvent::Complete, "00000000000000ff".to_string())];
        assert_eq!(a.events(), expected);
        assert_eq!(b.events(), expected);
    }

    /// Regression test for the P0 defect: observers were only reached when a
    /// `--on-download-*` shell command happened to be configured, so RPC
    /// clients never saw `aria2.onDownloadComplete` in a default deployment.
    #[test]
    fn test_fire_event_with_params_notifies_listener_without_shell_command() {
        let hooks = DownloadEventHooks::new();
        let listener = Arc::new(RecordingListener::default());
        hooks.add_listener(listener.clone());

        // Empty command == no `--on-download-complete` configured.
        hooks.fire_event_with_params(
            DownloadEvent::Complete,
            "0000000000000001",
            1,
            "/tmp/a.bin",
            "",
        );

        assert_eq!(
            listener.events(),
            vec![(DownloadEvent::Complete, "0000000000000001".to_string())],
            "observers must be notified even with no shell hook configured"
        );
    }

    #[test]
    fn test_one_shot_events_are_deduplicated_per_gid() {
        let hooks = DownloadEventHooks::new();
        let listener = Arc::new(RecordingListener::default());
        hooks.add_listener(listener.clone());

        // Same terminal event emitted twice for the same download (state
        // transition + engine-loop demotion) must reach the observer once.
        hooks.notify_listeners(DownloadEvent::Complete, "000000000000000a");
        hooks.notify_listeners(DownloadEvent::Complete, "000000000000000a");
        // A different GID is a different download.
        hooks.notify_listeners(DownloadEvent::Complete, "000000000000000b");
        // A different terminal event for the first download still passes.
        hooks.notify_listeners(DownloadEvent::BtComplete, "000000000000000a");

        assert_eq!(
            listener.events(),
            vec![
                (DownloadEvent::Complete, "000000000000000a".to_string()),
                (DownloadEvent::Complete, "000000000000000b".to_string()),
                (DownloadEvent::BtComplete, "000000000000000a".to_string()),
            ]
        );
    }

    #[test]
    fn test_repeatable_events_are_not_deduplicated() {
        let hooks = DownloadEventHooks::new();
        let listener = Arc::new(RecordingListener::default());
        hooks.add_listener(listener.clone());

        // Pause/unpause cycles legitimately repeat Start and Pause.
        hooks.notify_listeners(DownloadEvent::Pause, "000000000000000c");
        hooks.notify_listeners(DownloadEvent::Start, "000000000000000c");
        hooks.notify_listeners(DownloadEvent::Pause, "000000000000000c");

        assert_eq!(listener.events().len(), 3);
    }

    #[test]
    fn test_notify_listeners_is_noop_without_listeners() {
        let hooks = DownloadEventHooks::new();
        // Must not panic and must not consume de-duplication budget, so a
        // listener registered later still receives the event.
        hooks.notify_listeners(DownloadEvent::Complete, "000000000000000d");

        let listener = Arc::new(RecordingListener::default());
        hooks.add_listener(listener.clone());
        hooks.notify_listeners(DownloadEvent::Complete, "000000000000000d");

        assert_eq!(listener.events().len(), 1);
    }

    #[test]
    fn test_shared_bus_is_a_singleton() {
        let a = DownloadEventHooks::shared();
        let b = DownloadEventHooks::shared();
        assert!(Arc::ptr_eq(a, b), "shared() must return the same instance");
    }

    #[test]
    fn test_is_once_per_download_classification() {
        assert!(DownloadEvent::Complete.is_once_per_download());
        assert!(DownloadEvent::Error.is_once_per_download());
        assert!(DownloadEvent::BtComplete.is_once_per_download());
        assert!(!DownloadEvent::Start.is_once_per_download());
        assert!(!DownloadEvent::Pause.is_once_per_download());
        assert!(!DownloadEvent::Stop.is_once_per_download());
    }

    #[test]
    fn test_one_shot_ledger_rotates_and_stays_bounded() {
        let mut ledger = OneShotLedger::default();
        for i in 0..(DEDUP_GENERATION_CAPACITY + 10) {
            assert!(ledger.claim((format!("{:016x}", i), DownloadEvent::Complete)));
        }
        // Rotation happened, so neither generation exceeds the cap.
        assert!(ledger.current.len() <= DEDUP_GENERATION_CAPACITY);
        assert!(ledger.previous.len() <= DEDUP_GENERATION_CAPACITY);
        // The most recent entries are still remembered.
        assert!(!ledger.claim((
            format!("{:016x}", DEDUP_GENERATION_CAPACITY + 9),
            DownloadEvent::Complete
        )));
    }
}

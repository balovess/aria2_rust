//! Halt reason and control flags for download lifecycle management.
//!
//! Ports the C++ `RequestGroup` halt/pause flag model which separates the
//! *intent* (halt requested, pause requested, force halt) from the *observed
//! status* (`DownloadStatus`). This separation is critical for:
//!
//! - Graceful vs. forced shutdown semantics
//! - Asynchronous pause/halt processing (the running download loop checks
//!   flags on each iteration rather than having its state mutated from outside)
//! - `HaltReason` tracking (shutdown signal vs. user request) which affects
//!   the `DownloadResultCode` reported to RPC consumers

use std::sync::atomic::{AtomicBool, Ordering};

/// Reason why a download was halted.
///
/// Mirrors C++ `RequestGroup::HaltReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HaltReason {
    /// No halt has been requested.
    #[default]
    None,
    /// The program is shutting down (SIGTERM / Ctrl+C).
    ShutdownSignal,
    /// The user explicitly requested the halt (RPC `aria2.remove`, etc.).
    UserRequest,
    /// The command exceeded its configured timeout.
    Timeout,
}

/// Atomic control flags for download lifecycle transitions.
///
/// In C++ aria2, `RequestGroup` stores `haltRequested_`, `forceHaltRequested_`,
/// `pauseRequested_`, and `haltReason_` as separate fields. This struct bundles
/// them together with atomic operations so they can be checked from hot
/// download loops without acquiring the `RwLock` on `DownloadStatus`.
///
/// # Memory ordering
///
/// All loads use `Ordering::Acquire` and all stores use `Ordering::Release`
/// to ensure flag writes are visible to the download loop before it acts on them.
pub struct DownloadControlFlags {
    /// Graceful halt requested: let in-flight chunks finish writing, then stop.
    halt_requested: AtomicBool,
    /// Force halt requested: abort immediately, even mid-write.
    force_halt_requested: AtomicBool,
    /// Pause requested: transition to Paused state on next loop iteration.
    pause_requested: AtomicBool,
    /// Force pause requested: like pause but aborts in-flight commands.
    force_pause_requested: AtomicBool,
    /// Restart requested: stop the current download and re-queue it.
    restart_requested: AtomicBool,
    /// Close file requested: flush and close the output file handle.
    close_file_requested: AtomicBool,
    /// Save control file requested: persist .aria2 progress checkpoint.
    save_control_requested: AtomicBool,
    /// Remove control file requested: delete .aria2 progress checkpoint.
    remove_control_requested: AtomicBool,
}

impl DownloadControlFlags {
    /// Create a new set of control flags, all cleared.
    pub fn new() -> Self {
        Self {
            halt_requested: AtomicBool::new(false),
            force_halt_requested: AtomicBool::new(false),
            pause_requested: AtomicBool::new(false),
            force_pause_requested: AtomicBool::new(false),
            restart_requested: AtomicBool::new(false),
            close_file_requested: AtomicBool::new(false),
            save_control_requested: AtomicBool::new(false),
            remove_control_requested: AtomicBool::new(false),
        }
    }

    // ── Halt ────────────────────────────────────────────────────────────

    /// Request a graceful halt.
    ///
    /// Sets `halt_requested` and clears both pause flags (matching C++
    /// behavior where halt takes precedence over pause).
    pub fn request_halt(&self) {
        self.halt_requested.store(true, Ordering::Release);
        self.pause_requested.store(false, Ordering::Release);
        self.force_pause_requested.store(false, Ordering::Release);
    }

    /// Request a forced halt (abort immediately).
    ///
    /// Sets both `halt_requested` and `force_halt_requested`.
    pub fn request_force_halt(&self) {
        self.halt_requested.store(true, Ordering::Release);
        self.force_halt_requested.store(true, Ordering::Release);
        self.pause_requested.store(false, Ordering::Release);
        self.force_pause_requested.store(false, Ordering::Release);
    }

    /// Check whether a graceful halt has been requested.
    pub fn is_halt_requested(&self) -> bool {
        self.halt_requested.load(Ordering::Acquire)
    }

    /// Check whether a forced halt has been requested.
    pub fn is_force_halt_requested(&self) -> bool {
        self.force_halt_requested.load(Ordering::Acquire)
    }

    /// Clear halt flags (e.g. after the halt has been processed).
    pub fn clear_halt(&self) {
        self.halt_requested.store(false, Ordering::Release);
        self.force_halt_requested.store(false, Ordering::Release);
    }

    // ── Pause ───────────────────────────────────────────────────────────

    /// Request a graceful pause.
    ///
    /// The download loop will transition to `Paused` on the next iteration,
    /// allowing in-flight chunks to finish.
    pub fn request_pause(&self) {
        self.pause_requested.store(true, Ordering::Release);
    }

    /// Request a forced pause.
    ///
    /// Like `request_pause`, but also sets `force_pause_requested` so the
    /// download loop knows to abort in-flight commands immediately.
    pub fn request_force_pause(&self) {
        self.pause_requested.store(true, Ordering::Release);
        self.force_pause_requested.store(true, Ordering::Release);
    }

    /// Check whether a pause has been requested.
    pub fn is_pause_requested(&self) -> bool {
        self.pause_requested.load(Ordering::Acquire)
    }

    /// Check whether a forced pause has been requested.
    pub fn is_force_pause_requested(&self) -> bool {
        self.force_pause_requested.load(Ordering::Acquire)
    }

    /// Clear pause flags (e.g. after the download has been unpaused).
    pub fn clear_pause(&self) {
        self.pause_requested.store(false, Ordering::Release);
        self.force_pause_requested.store(false, Ordering::Release);
    }

    // ── Restart ─────────────────────────────────────────────────────────

    /// Request a download restart (stop then re-queue).
    pub fn request_restart(&self) {
        self.restart_requested.store(true, Ordering::Release);
    }

    /// Check whether a restart has been requested.
    pub fn is_restart_requested(&self) -> bool {
        self.restart_requested.load(Ordering::Acquire)
    }

    /// Clear the restart flag.
    pub fn clear_restart(&self) {
        self.restart_requested.store(false, Ordering::Release);
    }

    // ── Bulk operations ─────────────────────────────────────────────────

    /// Clear all control flags.
    pub fn clear_all(&self) {
        self.halt_requested.store(false, Ordering::Release);
        self.force_halt_requested.store(false, Ordering::Release);
        self.pause_requested.store(false, Ordering::Release);
        self.force_pause_requested.store(false, Ordering::Release);
        self.restart_requested.store(false, Ordering::Release);
        self.close_file_requested.store(false, Ordering::Release);
        self.save_control_requested.store(false, Ordering::Release);
        self.remove_control_requested
            .store(false, Ordering::Release);
    }

    /// Check whether any control flag is set (for quick early-exit checks).
    pub fn any_requested(&self) -> bool {
        self.is_halt_requested() || self.is_pause_requested() || self.is_restart_requested()
    }

    // ── File lifecycle ──────────────────────────────────────────────────

    /// Request the download command to close its output file.
    pub fn request_close_file(&self) {
        self.close_file_requested.store(true, Ordering::Release);
    }

    /// Check whether a file close has been requested.
    pub fn is_close_file_requested(&self) -> bool {
        self.close_file_requested.load(Ordering::Acquire)
    }

    /// Clear the close file flag after processing.
    pub fn clear_close_file(&self) {
        self.close_file_requested.store(false, Ordering::Release);
    }

    /// Request saving the .aria2 control file.
    pub fn request_save_control(&self) {
        self.save_control_requested.store(true, Ordering::Release);
    }

    /// Check whether a control file save has been requested.
    pub fn is_save_control_requested(&self) -> bool {
        self.save_control_requested.load(Ordering::Acquire)
    }

    /// Consume one pending control-file save request.
    pub fn take_save_control_request(&self) -> bool {
        self.save_control_requested.swap(false, Ordering::AcqRel)
    }

    /// Clear the save control flag after processing.
    pub fn clear_save_control(&self) {
        self.save_control_requested.store(false, Ordering::Release);
    }

    /// Request removing the .aria2 control file.
    pub fn request_remove_control(&self) {
        self.remove_control_requested.store(true, Ordering::Release);
    }

    /// Check whether a control file removal has been requested.
    pub fn is_remove_control_requested(&self) -> bool {
        self.remove_control_requested.load(Ordering::Acquire)
    }

    /// Clear the remove control flag after processing.
    pub fn clear_remove_control(&self) {
        self.remove_control_requested
            .store(false, Ordering::Release);
    }
}

impl Default for DownloadControlFlags {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DownloadControlFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadControlFlags")
            .field("halt_requested", &self.is_halt_requested())
            .field("force_halt_requested", &self.is_force_halt_requested())
            .field("pause_requested", &self.is_pause_requested())
            .field("force_pause_requested", &self.is_force_pause_requested())
            .field("restart_requested", &self.is_restart_requested())
            .field("close_file_requested", &self.is_close_file_requested())
            .field("save_control_requested", &self.is_save_control_requested())
            .field(
                "remove_control_requested",
                &self.is_remove_control_requested(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_halt_request() {
        let flags = DownloadControlFlags::new();
        assert!(!flags.is_halt_requested());
        assert!(!flags.is_force_halt_requested());

        flags.request_halt();
        assert!(flags.is_halt_requested());
        assert!(!flags.is_force_halt_requested());

        flags.clear_halt();
        assert!(!flags.is_halt_requested());
    }

    #[test]
    fn test_force_halt_request() {
        let flags = DownloadControlFlags::new();
        flags.request_force_halt();
        assert!(flags.is_halt_requested());
        assert!(flags.is_force_halt_requested());
    }

    #[test]
    fn test_halt_clears_pause() {
        let flags = DownloadControlFlags::new();
        flags.request_force_pause();
        assert!(flags.is_pause_requested());
        assert!(flags.is_force_pause_requested());

        flags.request_halt();
        assert!(!flags.is_pause_requested());
        assert!(!flags.is_force_pause_requested());
        assert!(flags.is_halt_requested());
    }

    #[test]
    fn test_force_halt_clears_pause() {
        let flags = DownloadControlFlags::new();
        flags.request_force_pause();
        flags.request_force_halt();

        assert!(!flags.is_pause_requested());
        assert!(!flags.is_force_pause_requested());
        assert!(flags.is_halt_requested());
        assert!(flags.is_force_halt_requested());
    }

    #[test]
    fn test_pause_request() {
        let flags = DownloadControlFlags::new();
        assert!(!flags.is_pause_requested());

        flags.request_pause();
        assert!(flags.is_pause_requested());
        assert!(!flags.is_force_pause_requested());

        flags.clear_pause();
        assert!(!flags.is_pause_requested());
    }

    #[test]
    fn test_force_pause_request() {
        let flags = DownloadControlFlags::new();
        flags.request_force_pause();
        assert!(flags.is_pause_requested());
        assert!(flags.is_force_pause_requested());
    }

    #[test]
    fn test_restart_request() {
        let flags = DownloadControlFlags::new();
        assert!(!flags.is_restart_requested());

        flags.request_restart();
        assert!(flags.is_restart_requested());

        flags.clear_restart();
        assert!(!flags.is_restart_requested());
    }

    #[test]
    fn test_any_requested() {
        let flags = DownloadControlFlags::new();
        assert!(!flags.any_requested());

        flags.request_pause();
        assert!(flags.any_requested());

        flags.clear_all();
        assert!(!flags.any_requested());
    }

    #[test]
    fn test_clear_all() {
        let flags = DownloadControlFlags::new();
        flags.request_halt();
        flags.request_pause();
        flags.request_restart();
        flags.clear_all();
        assert!(!flags.is_halt_requested());
        assert!(!flags.is_pause_requested());
        assert!(!flags.is_restart_requested());
    }
}

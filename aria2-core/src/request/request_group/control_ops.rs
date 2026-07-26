//! Command counter and lifecycle control flag accessors.
//!
//! These methods provide lock-free access to the command counter
//! (`num_commands`) and control flags (`DownloadControlFlags`), which
//! are checked on every iteration of the download hot path.

use std::sync::atomic::Ordering;

use crate::util::rwlock_ext::RwLockRecover;

impl super::RequestGroup {
    // ── Command Counter (lifecycle tracking) ────────────────────────────

    /// Increment the in-flight command counter (before spawning a task).
    /// Mirrors C++ `AbstractCommand` constructor incrementing `numCommand_`.
    pub fn inc_commands(&self) {
        self.num_commands.fetch_add(1, Ordering::SeqCst);
    }

    /// Decrement the in-flight command counter (when a task completes).
    /// Mirrors C++ `AbstractCommand` destructor decrementing `numCommand_`.
    /// Returns the previous value.
    pub fn dec_commands(&self) -> u32 {
        self.num_commands.fetch_sub(1, Ordering::SeqCst)
    }

    /// Get the current number of in-flight commands (lock-free).
    /// When this is 0, the group has no running work and can be demoted.
    pub fn num_commands(&self) -> u32 {
        self.num_commands.load(Ordering::SeqCst)
    }

    // ── Lifecycle Control Flag Accessors ────────────────────────────────

    /// Non-blocking check whether a graceful halt has been requested.
    ///
    /// The download loop should call this on each iteration; if `true`,
    /// finish writing the current chunk and then stop.
    pub fn is_halt_requested(&self) -> bool {
        self.control_flags.is_halt_requested()
    }

    /// Non-blocking check whether a forced halt has been requested.
    ///
    /// If `true`, the download loop should abort immediately, even mid-write.
    pub fn is_force_halt_requested(&self) -> bool {
        self.control_flags.is_force_halt_requested()
    }

    /// Non-blocking check whether a pause has been requested.
    pub fn is_pause_requested(&self) -> bool {
        self.control_flags.is_pause_requested()
    }

    /// Non-blocking check whether a forced pause has been requested.
    pub fn is_force_pause_requested(&self) -> bool {
        self.control_flags.is_force_pause_requested()
    }

    /// Non-blocking check whether a restart has been requested.
    pub fn is_restart_requested(&self) -> bool {
        self.control_flags.is_restart_requested()
    }

    /// Request a restart (stop current download and re-queue).
    pub fn request_restart(&self) {
        self.control_flags.request_restart();
        tracing::info!(gid = self.gid.value(), "Restart requested");
    }

    /// Get the current halt reason.
    pub fn get_halt_reason(&self) -> super::halt_reason::HaltReason {
        *self.halt_reason.recover()
    }

    /// Record an error code and message for this download.
    pub fn set_last_error(
        &self,
        code: super::result_code::DownloadResultCode,
        message: impl Into<String>,
    ) {
        *self.last_error_code.recover_mut() = code;
        *self.last_error_message.recover_mut() = message.into();
    }

    /// Get the last recorded error code.
    pub fn get_last_error_code(&self) -> super::result_code::DownloadResultCode {
        *self.last_error_code.recover()
    }

    /// Get the last recorded error message.
    pub fn get_last_error_message(&self) -> String {
        self.last_error_message.recover().clone()
    }
}

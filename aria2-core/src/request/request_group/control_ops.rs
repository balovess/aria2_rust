//! Command counter and lifecycle control flag accessors.
//!
//! These methods provide lock-free access to the command counter
//! (`num_commands`) and control flags (`DownloadControlFlags`), which
//! are checked on every iteration of the download hot path.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::util::rwlock_ext::RwLockRecover;

impl super::RequestGroup {
    pub(crate) fn attach_activity_signal(&mut self, signal: Arc<super::ActivitySignal>) {
        let _ = self.activity_signal.set(Arc::clone(&signal));
        self.connection_state
            .attach_activity_signal(Arc::clone(&signal));
        self.progress.attach_activity_signal(signal);
    }

    /// Return the lifecycle notification handle used by async download tasks.
    pub fn lifecycle_notifier(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.lifecycle_notify)
    }

    /// Wake async tasks waiting for a lifecycle transition.
    pub(crate) fn notify_lifecycle_changed(&self) {
        self.lifecycle_notify.notify_waiters();
        self.notify_activity_changed();
    }

    pub(crate) fn notify_activity_changed(&self) {
        if let Some(signal) = self.activity_signal.get() {
            signal.notify();
        }
    }

    /// Request a graceful pause and wake async lifecycle waiters.
    pub fn request_pause(&self) {
        self.control_flags.request_pause();
        self.notify_lifecycle_changed();
    }

    /// Clear a pending pause and wake async lifecycle waiters.
    pub fn clear_pause(&self) {
        self.control_flags.clear_pause();
        self.notify_lifecycle_changed();
    }

    // ── Command Counter (lifecycle tracking) ────────────────────────────

    /// Increment the in-flight command counter (before spawning a task).
    /// Mirrors C++ `AbstractCommand` constructor incrementing `numCommand_`.
    pub fn inc_commands(&self) {
        self.num_commands.fetch_add(1, Ordering::SeqCst);
    }

    /// Decrement the in-flight command counter (when a task completes).
    /// Mirrors C++ `AbstractCommand` destructor decrementing `numCommand_`.
    /// Returns the previous value, or zero when the counter is already empty.
    pub fn dec_commands(&self) -> u32 {
        self.num_commands
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_sub(1)
            })
            .unwrap_or(0)
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
        self.notify_lifecycle_changed();
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

    /// Clear command-failure state before starting a new command generation.
    pub fn clear_command_failure(&self) {
        self.command_failure
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Return the number of resume failures recorded for this group.
    pub fn resume_failure_count(&self) -> u32 {
        self.resume_failure_count
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Record one `CANNOT_RESUME` URI attempt and return the new count.
    pub fn increase_resume_failure_count(&self) -> u32 {
        self.resume_failure_count
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            .saturating_add(1)
    }
}

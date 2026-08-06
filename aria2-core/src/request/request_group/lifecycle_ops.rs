//! Status transition methods for RequestGroup.
//!
//! Mirrors C++ `RequestGroup` state transitions: start, pause, resume,
//! remove, complete, error, and halt. These methods mutate the download
//! status and record timing information.

use tracing::debug;

use crate::engine::download_event_hooks::{DownloadEvent, DownloadEventHooks};
use crate::error::Result;
use crate::util::rwlock_ext::RwLockRecover;

use super::halt_reason::HaltReason;
use super::result_code::DownloadResultCode;
use super::status::DownloadStatus;

impl super::RequestGroup {
    /// Publish a terminal lifecycle event to the process-wide observer bus.
    ///
    /// The status transition itself is the only choke point that every
    /// download command passes through. Emitting here guarantees terminal
    /// lifecycle notifications reach RPC clients; the event bus de-duplicates
    /// one-shot events when the engine loop also observes the transition.
    ///
    /// Only observers are notified — **not** the `--on-download-*` shell
    /// hooks. Spawning a child process requires a Tokio runtime, and these
    /// setters are also called from synchronous contexts; shell hooks stay
    /// owned by the engine-loop hook sites where a runtime is guaranteed.
    fn notify_terminal_event(&self, event: DownloadEvent) {
        DownloadEventHooks::shared().notify_listeners(event, &self.gid.to_hex_string());
    }

    // ── Status Transitions ───────────────────────────────────────────────

    /// Transition to `Active` status and record the start time.
    ///
    /// Mirrors C++ `RequestGroup::setState(STATE_ACTIVE)`.
    pub fn start(&mut self) -> Result<()> {
        let mut status = self.status.recover_mut();
        let mut start_time = self.start_time.recover_mut();

        *status = DownloadStatus::Active;
        *start_time = Some(std::time::Instant::now());

        tracing::info!("Starting download task #{}", self.gid.value());
        Ok(())
    }

    /// Transition to `Paused` status (graceful pause).
    ///
    /// In-flight chunks are allowed to finish before the download loop stops.
    /// Mirrors C++ `pauseRequested_ = true`.
    pub fn pause(&mut self) -> Result<()> {
        let mut status = self.status.recover_mut();

        if matches!(*status, DownloadStatus::Active | DownloadStatus::Waiting) {
            *status = DownloadStatus::Paused;
            self.control_flags.request_halt();
            self.control_flags.request_pause();
            tracing::info!("Pausing download task #{}", self.gid.value());
        }

        Ok(())
    }

    /// Force-pause: set both pause and force-pause flags so the download loop
    /// aborts in-flight commands immediately. Mirrors C++ `forcePauseRequested`.
    pub fn force_pause(&mut self) -> Result<()> {
        let mut status = self.status.recover_mut();

        if matches!(*status, DownloadStatus::Active | DownloadStatus::Waiting) {
            *status = DownloadStatus::Paused;
            self.control_flags.request_force_halt();
            self.control_flags.request_force_pause();
            tracing::info!("Force-pausing download task #{}", self.gid.value());
        }

        Ok(())
    }

    /// Resume a paused download. Clears pause flags and transitions to Waiting.
    pub fn resume(&mut self) -> Result<()> {
        let mut status = self.status.recover_mut();

        if matches!(*status, DownloadStatus::Paused) {
            *status = DownloadStatus::Waiting;
            self.clear_command_failure();
            self.control_flags.clear_pause();
            self.control_flags.clear_halt();
            // Also clear a pending restart intent so a group that was paused
            // by `reduce_to_limit()` (which sets the restart flag alongside
            // pause) does not carry a stale flag after a manual unpause.
            self.control_flags.clear_restart();
            tracing::info!("Resuming download task #{}", self.gid.value());
        }

        Ok(())
    }

    /// Transition to `Removed` status and record the halt reason.
    ///
    /// Mirrors C++ `RequestGroup::setState(STATE_REMOVED)` with
    /// `haltReason_ = USER_REQUEST`.
    pub fn remove(&mut self) -> Result<()> {
        let mut status = self.status.recover_mut();
        let mut end_time = self.end_time.recover_mut();

        *status = DownloadStatus::Removed;
        *end_time = Some(std::time::Instant::now());
        *self.halt_reason.recover_mut() = HaltReason::UserRequest;

        tracing::info!("Removing download task #{}", self.gid.value());
        Ok(())
    }

    /// Mark the group as removed after its command has stopped.
    pub fn mark_removed(&self) {
        *self.status.recover_mut() = DownloadStatus::Removed;
        *self.end_time.recover_mut() = Some(std::time::Instant::now());
        *self.halt_reason.recover_mut() = HaltReason::UserRequest;
        tracing::info!(gid = self.gid.value(), "Marked download as removed");
    }

    /// Request a graceful halt (let in-flight chunks finish).
    /// Mirrors C++ `setHaltRequested(true, reason)`.
    pub fn request_halt(&self, reason: HaltReason) {
        self.control_flags.request_halt();
        *self.halt_reason.recover_mut() = reason;
        tracing::info!(gid = self.gid.value(), ?reason, "Halt requested");
    }

    /// Request a forced halt (abort immediately, even mid-write).
    /// Mirrors C++ `setForceHaltRequested(true, reason)`.
    pub fn request_force_halt(&self, reason: HaltReason) {
        self.control_flags.request_force_halt();
        *self.halt_reason.recover_mut() = reason;
        tracing::info!(gid = self.gid.value(), ?reason, "Force halt requested");
    }

    /// Mark the payload as complete and enter seed-only mode.
    ///
    /// This mirrors C++ `RequestGroup::enableSeedOnly()`: the torrent stays
    /// alive for seeding, while the payload is no longer an active download.
    pub fn enable_seed_only(&self) {
        if self.options().bt_detach_seed_only {
            self.seed_only
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// Return whether this group has entered seed-only mode.
    pub fn is_seed_only(&self) -> bool {
        self.seed_only.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Transition to `Complete` status and record the end time.
    ///
    /// Sets completed_length equal to total_length (mirrors C++ behavior
    /// where the download is considered 100% done).
    pub fn complete(&mut self) -> Result<()> {
        // Scope the guards so no lock is held while observers run.
        {
            let mut status = self.status.recover_mut();
            let mut end_time = self.end_time.recover_mut();

            let total = self.progress.total_length();
            *status = DownloadStatus::Complete;
            *end_time = Some(std::time::Instant::now());
            self.progress.set_completed_length(total);
        }

        tracing::info!("Completing download task #{}", self.gid.value());
        self.notify_terminal_event(DownloadEvent::Complete);
        Ok(())
    }

    /// Transition to `Error` status with an error message.
    pub fn error(&mut self, err: impl Into<String>) -> Result<()> {
        self.clear_bt_peer_snapshots();
        let message = err.into();
        {
            let mut status = self.status.recover_mut();
            let mut end_time = self.end_time.recover_mut();

            *status = DownloadStatus::Error(message.clone());
            *end_time = Some(std::time::Instant::now());
        }
        *self.last_error_message.recover_mut() = message;
        *self.last_error_code.recover_mut() = DownloadResultCode::UnknownError;

        tracing::debug!("Download task #{} encountered error", self.gid.value());
        self.notify_terminal_event(DownloadEvent::Error);
        Ok(())
    }

    /// Mark the download as complete using interior mutability (`&self`).
    ///
    /// Unlike `complete(&mut self)`, this method only requires `&self` so it
    /// can be called from the engine loop which holds the group behind an
    /// `Arc<std::sync::RwLock<RequestGroup>>` without needing a write lock
    /// on the outer guard.
    pub fn mark_complete(&self) {
        *self.status.recover_mut() = DownloadStatus::Complete;
        *self.end_time.recover_mut() = Some(std::time::Instant::now());
        tracing::info!(gid = self.gid.value(), "Marked download as complete");
        self.notify_terminal_event(DownloadEvent::Complete);
    }

    /// Mark the download as errored using interior mutability (`&self`).
    ///
    /// Stores the error message in `last_error_message` and the error code
    /// in `last_error_code`, then transitions status to `Error`.
    pub fn mark_error(&self, message: String) {
        self.mark_error_with_code(DownloadResultCode::UnknownError, message);
    }

    /// Mark the download as errored while preserving the mapped aria2 code.
    pub fn mark_error_with_code(&self, code: DownloadResultCode, message: String) {
        *self.last_error_message.recover_mut() = message.clone();
        *self.last_error_code.recover_mut() = code;
        *self.status.recover_mut() = DownloadStatus::Error(message);
        *self.end_time.recover_mut() = Some(std::time::Instant::now());
        tracing::info!(gid = self.gid.value(), ?code, "Marked download as errored");
        self.notify_terminal_event(DownloadEvent::Error);
    }

    /// Mark a timeout as a terminal error while retaining its structured code.
    pub fn mark_timeout(&self) {
        self.clear_bt_peer_snapshots();
        *self.last_error_message.recover_mut() = "Download timed out".to_string();
        *self.last_error_code.recover_mut() = DownloadResultCode::TimeOut;
        *self.status.recover_mut() = DownloadStatus::Error("Download timed out".to_string());
        *self.end_time.recover_mut() = Some(std::time::Instant::now());
        tracing::info!(gid = self.gid.value(), "Marked download as timed out");
        self.notify_terminal_event(DownloadEvent::Error);
    }

    /// Mark the download as paused using interior mutability (`&self`).
    ///
    /// Used by the engine loop when a running task terminates because a
    /// pause was requested (`aria2.pause` / `aria2.forcePause`). The group
    /// stays in `Paused` status so it can be unpaused and re-promoted,
    /// instead of being recorded as an error. Mirrors C++ where
    /// pause-requested groups return to the reserved queue.
    pub fn mark_paused(&self) {
        *self.status.recover_mut() = DownloadStatus::Paused;
        *self.last_error_code.recover_mut() = super::result_code::DownloadResultCode::Paused;
        *self.last_error_message.recover_mut() = "Download paused".to_string();
        tracing::info!(gid = self.gid.value(), "Marked download as paused");
    }

    /// Transition the group back to `Waiting` status (interior mutability).
    ///
    /// Used when a group's commands all ended without a terminal transition
    /// (e.g. a pause was requested and then undone before the task fully
    /// exited). The group is re-queued so promotion can re-spawn it.
    pub fn mark_waiting(&self) {
        *self.status.recover_mut() = DownloadStatus::Waiting;
        *self.end_time.recover_mut() = None;
        tracing::debug!(gid = self.gid.value(), "Marked download as waiting");
    }

    // ── Status Queries ───────────────────────────────────────────────────

    /// Return the current download status.
    pub fn status(&self) -> DownloadStatus {
        self.status.recover().clone()
    }

    /// Non-blocking check whether this group has been marked as `Removed`.
    ///
    /// Uses `try_read` on the inner status lock so it is safe to call from
    /// hot download loops without risking lock contention or deadlock. When
    /// the lock is contended the method returns `false` (treats the task as
    /// still running); the caller will re-check on the next iteration.
    ///
    /// This is the primary signal used by `DownloadCommand` and the
    /// underlying downloaders to detect that `aria2.remove` /
    /// `aria2.forceRemove` has been invoked.
    pub fn is_removed(&self) -> bool {
        match self.status.try_read() {
            Ok(guard) => matches!(*guard, DownloadStatus::Removed),
            Err(_) => false,
        }
    }

    /// Check whether this group has been paused (non-blocking).
    /// Used by downloaders to detect `aria2.pause` / `aria2.forcePause`.
    pub fn is_paused_flag(&self) -> bool {
        match self.status.try_read() {
            Ok(guard) => matches!(*guard, DownloadStatus::Paused),
            Err(_) => false,
        }
    }

    /// Per-command timeout duration. Returns `None` for no timeout.
    ///
    /// Mirrors C++ `RequestGroup::timeout_`. Currently derived from
    /// download options; may be overridden per-group in the future.
    pub fn timeout(&self) -> Option<std::time::Duration> {
        self.options.timeout.map(std::time::Duration::from_secs)
    }

    /// Drop piece storage and segment data.
    ///
    /// Called during promotion (reserved → active) to release resources
    /// held by a previously paused download. Paused downloads hold
    /// references to PieceStorage via their segment list; releasing
    /// them before the download restarts prevents stale state.
    ///
    /// Mirrors C++ `RequestGroup::dropPieceStorage()` which resets
    /// `segmentMan_` and `pieceStorage_`.
    pub fn drop_piece_storage(&self) {
        self.segments.recover_mut().clear();
        debug!(
            gid = self.gid.value(),
            "Dropped piece storage and segment data"
        );
    }

    /// BT info hash as hex string. Returns `None` for non-BT downloads.
    /// Mirrors C++ `RequestGroup::getDownloadContext()->getInfoHash()`.
    pub fn info_hash_hex(&self) -> Option<String> {
        self.download_context
            .recover()
            .as_ref()
            .and_then(|ctx| ctx.info_hash_hex())
    }
}

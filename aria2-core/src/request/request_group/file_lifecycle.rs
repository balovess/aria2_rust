//! File lifecycle and runtime resource management methods for RequestGroup.
//!
//! Mirrors C++ `RequestGroup::closeFile()`, `saveControlFile()`,
//! `removeControlFile()`, and `releaseRuntimeResource()`.

use std::sync::atomic::Ordering;

use crate::util::rwlock_ext::RwLockRecover;

impl super::RequestGroup {
    // ── File Lifecycle Methods ──────────────────────────────────────────
    // These mirror C++ `RequestGroup::closeFile()`, `saveControlFile()`,
    // `removeControlFile()`, and `releaseRuntimeResource()`.

    /// Close the output file, flushing OS buffers.
    ///
    /// Mirrors C++ `RequestGroup::closeFile()` which flushes the
    /// write-back disk cache and OS buffers before closing.
    ///
    /// In Rust, the primary file handle lives inside the download command.
    /// This method provides a hook for the engine loop to signal "flush
    /// and close" semantics during shutdown or demotion.
    pub fn close_file(&self) {
        self.control_flags.request_close_file();
        tracing::debug!(gid = self.gid.value(), "Requested file close");
    }

    /// Save the .aria2 control file (progress checkpoint).
    ///
    /// Mirrors C++ `RequestGroup::saveControlFile()`. Called by the engine
    /// during periodic auto-save and during shutdown to persist download
    /// progress for resume.
    ///
    /// Returns `true` if a control file was saved, `false` if saving was
    /// disabled (e.g. during hash checking) or no progress file exists.
    pub fn save_control_file(&self) -> bool {
        if !self
            .save_control_file_enabled
            .recover()
            .load(Ordering::SeqCst)
        {
            tracing::debug!(
                gid = self.gid.value(),
                "Control file saving disabled, skipping"
            );
            return false;
        }

        self.control_flags.request_save_control();
        tracing::debug!(gid = self.gid.value(), "Requested control file save");
        true
    }

    /// Request removal of the .aria2 control file.
    pub fn remove_control_file(&self) {
        self.control_flags.request_remove_control();
        tracing::debug!(gid = self.gid.value(), "Requested control file removal");
    }

    /// Associate this group with its output file's `.aria2` sidecar.
    pub fn set_control_file_path(&self, path: impl Into<std::path::PathBuf>) {
        *self.control_file_path.recover_mut() = Some(path.into());
    }

    /// Remove the sidecar immediately when a user halt requests cleanup.
    ///
    /// Missing files are treated as success, matching C++ cleanup semantics.
    pub fn process_remove_control_file(&self) -> crate::error::Result<bool> {
        if !self.control_flags.is_remove_control_requested() {
            return Ok(false);
        }
        if let Some(path) = self.control_file_path.recover().clone() {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(crate::error::Aria2Error::FileIo(format!(
                        "failed to remove control file {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        self.control_flags.clear_remove_control();
        Ok(true)
    }

    /// Enable saving the .aria2 control file.
    ///
    /// Mirrors C++ `RequestGroup::enableSaveControlFile()`. Called after
    /// hash verification completes to re-enable control file saving (which
    /// was disabled during the check to avoid corrupt state).
    pub fn enable_save_control_file(&self) {
        self.save_control_file_enabled
            .recover()
            .store(true, Ordering::SeqCst);
    }

    /// Disable saving the .aria2 control file.
    ///
    /// Mirrors C++ `RequestGroup::disableSaveControlFile()`. Called before
    /// hash verification starts to prevent saving a partially-verified state.
    pub fn disable_save_control_file(&self) {
        self.save_control_file_enabled
            .recover()
            .store(false, Ordering::SeqCst);
    }

    /// Release runtime resources held by this group.
    ///
    /// Mirrors C++ `RequestGroup::releaseRuntimeResource()`. Called when
    /// the group transitions from active to stopped. Clears the download
    /// context and BT-specific runtime resources.
    pub fn release_runtime_resources(&self) {
        *self.download_context.recover_mut() = None;
        self.rate_limiter.recover_mut().take();
        tracing::debug!(gid = self.gid.value(), "Released runtime resources");
    }

    /// Set a dependency that must be resolved before this group can start.
    ///
    /// Mirrors C++ `RequestGroup::dependsOn()`. Used by Metalink downloads
    /// to chain child downloads that wait for the parent to complete.
    pub fn set_dependency(&self, dep: Box<dyn super::dependency::Dependency>) {
        *self.dependency.recover_mut() = Some(dep);
    }

    /// Check whether this group's dependency is resolved.
    ///
    /// Mirrors C++ `RequestGroup::isDependencyResolved()`. Returns `true`
    /// if there is no dependency or the dependency has been resolved.
    pub fn is_dependency_resolved(&self) -> bool {
        self.dependency
            .recover()
            .as_ref()
            .map(|d| d.resolve())
            .unwrap_or(true)
    }
}

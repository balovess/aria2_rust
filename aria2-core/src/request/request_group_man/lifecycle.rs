//! Lifecycle transitions for request groups.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{debug, info, warn};

use super::{GroupId, RequestGroup, RequestGroupMan};
use crate::error::Result;
use crate::request::request_group::{DownloadStatus, HaltReason};
use crate::util::rwlock_ext::RwLockRecover;

impl RequestGroupMan {
    // ── Group Removal ───────────────────────────────────────────────────

    /// Remove a group by numeric GID from either active or reserved.
    pub fn remove_group_by_id(&self, gid: GroupId) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        let _lifecycle = self.lifecycle_guard();
        // Try active first, then reserved.
        let removed = self
            .active
            .remove(&gid)
            .map(|(_, v)| v)
            .or_else(|| self.reserved.remove_by_gid(gid));
        if removed.is_some() {
            self.unregister_group(gid);
        }
        removed
    }

    pub fn remove_group(&self, gid: GroupId) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        // Keep active groups in requestGroups_ and only mark them for halt;
        // RequestGroupMan removes them after their last command exits.
        if let Some(group_lock) = self.active.get(&gid).map(|entry| entry.value().clone()) {
            let group = group_lock.recover();
            group.request_halt(HaltReason::UserRequest);
            info!("Requested removal of active download task #{}", gid.value());
            return Ok(());
        }

        self.remove_reserved_group(gid)
    }

    /// Remove a reserved group while the lifecycle transition lock is held.
    fn remove_reserved_group(&self, gid: GroupId) -> Result<()> {
        let group_lock = self.reserved.find_by_gid(gid).ok_or_else(|| {
            crate::error::Aria2Error::InvalidArgument(format!("GID {} not found", gid.value()))
        })?;

        // Match aria2_original's removeDownload() contract: a reserved group
        // whose dependency is unresolved cannot be removed independently. The
        // prerequisite graph must first reach a terminal state so the manager
        // can resolve or fail the dependent payload coherently.
        if !group_lock.recover().is_dependency_resolved() {
            return Err(crate::error::Aria2Error::InvalidArgument(format!(
                "GID#{} cannot be removed now",
                gid.to_hex_string()
            )));
        }

        // A reserved group has no command to drain, so it can be removed now.
        if let Some(group_lock) = self.reserved.remove_by_gid(gid) {
            let mut group = group_lock.recover_mut();
            group.remove()?;
            self.unregister_group(gid);
            info!("Removing reserved download task #{}", gid.value());
            self.stopped.add(group.create_download_result());
        }
        Ok(())
    }

    /// Request immediate removal of an active group.
    ///
    /// The engine still owns task abortion and completion accounting; this
    /// method only publishes the C++ force-halt intent on the group.
    pub fn force_remove_group(&self, gid: GroupId) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        if let Some(group_lock) = self.active.get(&gid).map(|entry| entry.value().clone()) {
            group_lock
                .recover()
                .request_force_halt(HaltReason::UserRequest);
            info!(
                "Requested force removal of active download task #{}",
                gid.value()
            );
            return Ok(());
        }
        // Reserved groups have no in-flight task, so remove them synchronously.
        self.remove_reserved_group(gid)
    }

    /// Mark an active command as timed out while preserving finalization.
    pub fn timeout_group(&self, gid: GroupId) -> bool {
        if let Some(group_lock) = self.active.get(&gid).map(|entry| entry.value().clone()) {
            let group = group_lock.recover();
            group.request_halt(HaltReason::Timeout);
            group.set_last_error(
                crate::request::request_group::DownloadResultCode::TimeOut,
                "Download timed out",
            );
            return true;
        }
        false
    }

    /// Handle a promoted group whose download task failed to spawn.
    ///
    /// `fill_from_reserver()` inserts the group into the active DashMap, but
    /// if no command can be created for it (e.g. an empty URI list or an
    /// unsupported scheme) there is no running task to ever demote it. This
    /// removes the group from active, records an error, and stores a stopped
    /// result so the group does not stay in the active list forever.
    ///
    /// Mirrors C++ `createInitialCommand()` failure handling which stops the
    /// group with an error.
    pub fn fail_spawned_group(&self, gid: GroupId, message: &str) -> bool {
        let _lifecycle = self.lifecycle_guard();
        if let Some((_, group)) = self.active.remove(&gid) {
            self.unregister_group(gid);
            group.recover_mut().mark_error(message.to_string());
            let result = group.recover().create_download_result();
            self.stopped.add(result);
            debug!(
                gid = gid.value(),
                "Removed failed-spawn group from active and recorded error"
            );
            true
        } else {
            warn!(gid = gid.value(), "Failed-spawn group not found in active");
            false
        }
    }

    /// Remove a reserved dependency payload that cannot ever be promoted.
    ///
    /// This is deliberately separate from `fail_spawned_group`: no command
    /// exists yet, so leaving the group in `reserved` would make it appear as
    /// waiting forever and prevent the engine from reaching an idle state.
    pub(super) fn fail_reserved_group_with_code(
        &self,
        gid: GroupId,
        code: crate::request::request_group::DownloadResultCode,
        message: String,
    ) -> bool {
        let _lifecycle = self.lifecycle_guard();
        let Some(group) = self.reserved.remove_by_gid(gid) else {
            warn!(gid = gid.value(), "Failed reserved group not found");
            return false;
        };

        self.unregister_group(gid);
        group.recover().mark_error_with_code(code, message);
        self.stopped.add(group.recover().create_download_result());
        info!(
            gid = gid.value(),
            "Recorded failed reserved dependency group"
        );
        true
    }

    #[cfg(feature = "bittorrent")]
    pub(super) fn fail_reserved_group(&self, gid: GroupId, message: &str) -> bool {
        self.fail_reserved_group_with_code(
            gid,
            crate::request::request_group::DownloadResultCode::BittorrentParseError,
            message.to_string(),
        )
    }

    /// Return both sides of a standard Metalink metadata/payload graph.
    ///
    /// Session restore materializes the metadata prerequisite and payload as
    /// separate Rust groups, while the persisted task identity is the
    /// metadata GID. Keep lifecycle operations on that identity coherent
    /// without treating arbitrary `belongs_to` follow children as one task.
    fn metalink_graph_groups(&self, gid: GroupId) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
        let Some(target) = self.find_group(gid) else {
            return Vec::new();
        };

        let (metadata_gid, payload_gid) =
            if let Some(metadata_info) = target.recover().metadata_info() {
                let Some(metadata_gid) = metadata_info.gid() else {
                    return vec![Arc::clone(&target)];
                };
                let Some(metadata) = self.find_group(metadata_gid) else {
                    return vec![Arc::clone(&target)];
                };
                if metadata.recover().belongs_to_gid() != Some(gid) {
                    return vec![Arc::clone(&target)];
                }
                (metadata_gid, gid)
            } else if let Some(payload_gid) = target.recover().belongs_to_gid() {
                let Some(payload) = self.find_group(payload_gid) else {
                    return vec![Arc::clone(&target)];
                };
                let Some(metadata_gid) = payload
                    .recover()
                    .metadata_info()
                    .and_then(|info| info.gid())
                else {
                    return vec![Arc::clone(&target)];
                };
                if metadata_gid != gid {
                    return vec![Arc::clone(&target)];
                }
                (gid, payload_gid)
            } else {
                return vec![Arc::clone(&target)];
            };

        let Some(metadata) = self.find_group(metadata_gid) else {
            return vec![Arc::clone(&target)];
        };
        let Some(payload) = self.find_group(payload_gid) else {
            return vec![Arc::clone(&target)];
        };
        vec![metadata, payload]
    }

    // ── Pause/Unpause ───────────────────────────────────────────────────

    pub fn pause_group(&self, gid: GroupId) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        let group_lock = self.find_group(gid).ok_or_else(|| {
            crate::error::Aria2Error::InvalidArgument(format!("GID {} not found", gid.value()))
        })?;
        let group = group_lock.recover();
        if !matches!(
            group.status(),
            DownloadStatus::Active | DownloadStatus::Waiting
        ) {
            return Err(crate::error::Aria2Error::InvalidArgument(format!(
                "GID#{} cannot be paused now",
                gid.to_hex_string()
            )));
        }
        drop(group);
        for group_lock in self.metalink_graph_groups(gid) {
            group_lock.recover_mut().pause()?;
        }
        info!("Pausing download task #{}", gid.value());
        Ok(())
    }

    pub fn unpause_group(&self, gid: GroupId) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        let group_lock = self.find_group(gid).ok_or_else(|| {
            crate::error::Aria2Error::InvalidArgument(format!("GID {} not found", gid.value()))
        })?;
        let group = group_lock.recover();
        if !group.status().is_paused() {
            return Err(crate::error::Aria2Error::InvalidArgument(format!(
                "GID#{} cannot be unpaused now",
                gid.to_hex_string()
            )));
        }
        drop(group);
        for group_lock in self.metalink_graph_groups(gid) {
            group_lock.recover_mut().resume()?;
        }
        info!("Resuming download task #{}", gid.value());
        Ok(())
    }

    pub fn force_pause_group(&self, gid: GroupId) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        let group_lock = self.find_group(gid).ok_or_else(|| {
            crate::error::Aria2Error::InvalidArgument(format!("GID {} not found", gid.value()))
        })?;
        let group = group_lock.recover();
        if !matches!(
            group.status(),
            DownloadStatus::Active | DownloadStatus::Waiting
        ) {
            return Err(crate::error::Aria2Error::InvalidArgument(format!(
                "GID#{} cannot be paused now",
                gid.to_hex_string()
            )));
        }
        drop(group);
        for group_lock in self.metalink_graph_groups(gid) {
            group_lock.recover_mut().force_pause()?;
        }
        Ok(())
    }

    pub fn pause_all(&self) {
        let _lifecycle = self.lifecycle_guard();
        let gids: Vec<_> = self.groups.iter().map(|entry| *entry.key()).collect();
        let mut visited = HashSet::with_capacity(gids.len());
        for gid in gids {
            for group_lock in self.metalink_graph_groups(gid) {
                let related_gid = group_lock.recover().gid();
                if visited.insert(related_gid) {
                    let _ = group_lock.recover_mut().pause();
                }
            }
        }
    }

    pub fn force_pause_all(&self) {
        let _lifecycle = self.lifecycle_guard();
        let gids: Vec<_> = self.groups.iter().map(|entry| *entry.key()).collect();
        let mut visited = HashSet::with_capacity(gids.len());
        for gid in gids {
            for group_lock in self.metalink_graph_groups(gid) {
                let related_gid = group_lock.recover().gid();
                if visited.insert(related_gid) {
                    let _ = group_lock.recover_mut().force_pause();
                }
            }
        }
    }

    pub fn unpause_all(&self) {
        let _lifecycle = self.lifecycle_guard();
        let gids: Vec<_> = self.groups.iter().map(|entry| *entry.key()).collect();
        let mut visited = HashSet::with_capacity(gids.len());
        for gid in gids {
            for group_lock in self.metalink_graph_groups(gid) {
                let related_gid = group_lock.recover().gid();
                if visited.insert(related_gid) {
                    let _ = group_lock.recover_mut().resume();
                }
            }
        }
    }

    // ── Halt ────────────────────────────────────────────────────────────

    pub fn halt_all(&self, reason: HaltReason) {
        for entry in self.groups.iter() {
            let group = entry.recover();
            group.request_halt(reason);
        }
    }

    pub fn force_halt_all(&self, reason: HaltReason) {
        if matches!(reason, HaltReason::ShutdownSignal) {
            self.force_shutdown_requested.store(true, Ordering::Release);
        }
        for entry in self.groups.iter() {
            let group = entry.recover();
            group.request_force_halt(reason);
        }
    }

    /// Whether the process was explicitly asked to force-shutdown.
    pub fn force_shutdown_requested(&self) -> bool {
        self.force_shutdown_requested.load(Ordering::Acquire)
    }

    /// Remove all groups that have not started yet.
    ///
    /// A force shutdown must not leave queued work behind while the engine is
    /// terminating. Active groups are deliberately untouched here: the engine
    /// still owns their command handles and will force-halt them through
    /// [`Self::force_halt_all`].
    pub fn force_remove_reserved(&self) -> usize {
        let _lifecycle = self.lifecycle_guard();
        let groups = self.reserved.drain();
        let removed = groups.len();
        for group_lock in groups {
            let gid = group_lock.recover().gid();
            let mut group = group_lock.recover_mut();
            let _ = group.remove();
            self.unregister_group(gid);
            self.stopped.add(group.create_download_result());
        }
        if removed > 0 {
            info!(removed, "Removed reserved downloads during force shutdown");
        }
        removed
    }

    // ── Option Updates ──────────────────────────────────────────────────

    pub fn update_group_options(
        &self,
        gid_hex: &str,
        changes: HashMap<String, serde_json::Value>,
    ) -> std::result::Result<(), String> {
        let group = self
            .group_by_hex(gid_hex)
            .ok_or_else(|| format!("GID {} not found", gid_hex))?;

        group.recover_mut().apply_runtime_options(changes)
    }

    /// Apply task-level runtime changes with aria2-compatible
    /// immediate/pending semantics. The lifecycle transition belongs here so
    /// RPC, C API, and future adapters cannot disagree about when a change is
    /// visible or when an active command must be restarted.
    pub fn change_group_options(
        &self,
        gid_hex: &str,
        changes: HashMap<String, serde_json::Value>,
    ) -> std::result::Result<(), String> {
        let group = self
            .group_by_hex(gid_hex)
            .ok_or_else(|| format!("GID {} not found", gid_hex))?;
        let gid = group.recover().gid();
        let classified = group.recover().classify_runtime_options(changes)?;
        let should_restart =
            !classified.pending.is_empty() && group.recover().status().is_running();

        {
            let mut group = group.recover_mut();
            group.apply_runtime_options(classified.immediate)?;
            if !classified.pending.is_empty() {
                group.set_pending_options(classified.pending);
            }
        }

        if should_restart {
            self.pause_group(gid).map_err(|error| error.to_string())?;
            self.find_group(gid)
                .ok_or_else(|| format!("GID {} disappeared during option update", gid.value()))?
                .recover()
                .request_restart();
        }
        Ok(())
    }
}

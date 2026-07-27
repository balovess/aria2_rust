//! Demotion logic: active → stopped group transition.
//!
//! Mirrors C++ `RequestGroupMan::removeStoppedGroup()`. When a group's
//! `num_commands` drops to 0 and its status is terminal (Complete, Error,
//! Removed), the engine removes it from the active DashMap and stores
//! the result for RPC `tellStopped` queries.

use std::sync::Arc;
use tracing::{debug, info, warn};

use super::RequestGroup;
use crate::request::request_group::DownloadStatus;
use crate::util::rwlock_ext::RwLockRecover;

/// Result of a demotion check: groups that should be removed from active.
pub struct DemotedGroup {
    pub group: Arc<std::sync::RwLock<RequestGroup>>,
    pub result: crate::request::request_group::DownloadResult,
}

impl super::RequestGroupMan {
    /// Scan the active DashMap for groups that have no more in-flight commands
    /// and are in a terminal state. Returns the list of demoted groups.
    ///
    /// The engine should call this each tick and then:
    /// 1. Remove each group from the active DashMap
    /// 2. Add each `DownloadResult` to the stopped storage
    /// 3. Fire any on_download_complete events
    ///
    /// Mirrors C++ `ProcessStoppedRequestGroup` functor logic.
    pub fn find_stopped_groups(&self) -> Vec<DemotedGroup> {
        let mut demoted = Vec::new();

        for entry in self.active.iter() {
            let g = entry.recover();
            let gid = g.gid();
            let num_cmd = g.num_commands();
            let status = g.status();

            // A group is "stopped" when num_commands == 0 AND in terminal state.
            // C++ checks `numCommand_ == 0` in the AbstractCommand destructor path.
            let is_terminal = matches!(
                status,
                DownloadStatus::Complete | DownloadStatus::Error(_) | DownloadStatus::Removed
            );

            if num_cmd == 0 && is_terminal {
                debug!(
                    gid = gid.value(),
                    ?status,
                    "Found stopped group (num_commands=0, terminal status)"
                );
                let result = g.create_download_result();
                demoted.push(DemotedGroup {
                    group: Arc::clone(entry.value()),
                    result,
                });
            }
        }

        demoted
    }

    /// Remove a demoted group from the active DashMap and add its result
    /// to stopped storage. Returns `true` if the group was removed.
    pub fn demote_group(
        &self,
        gid: crate::request::request_group::GroupId,
        result: crate::request::request_group::DownloadResult,
    ) -> bool {
        if let Some((_, group)) = self.active.remove(&gid) {
            // Release runtime resources (C++ `releaseRuntimeResource()`).
            // In Rust, this means clearing the download context and
            // releasing the rate limiter handle.
            {
                let g = group.recover();
                *g.download_context.recover_mut() = None;
                g.rate_limiter.recover_mut().take(); // Drop rate limiter
            }

            self.stopped.add(result);

            info!(gid = gid.value(), "Demoted group from active to stopped");
            true
        } else {
            warn!(
                gid = gid.value(),
                "Tried to demote group not found in active"
            );
            false
        }
    }

    /// Process all stopped groups: find them, demote them, and return
    /// the list of demoted GIDs for event notification.
    ///
    /// This is the main entry point the engine should call each tick.
    pub fn remove_stopped_groups(&self) -> Vec<crate::request::request_group::GroupId> {
        let demoted = self.find_stopped_groups();
        let mut gids = Vec::with_capacity(demoted.len());

        for dg in demoted {
            let gid = dg.group.recover().gid();
            self.demote_group(gid, dg.result);
            gids.push(gid);
        }

        gids
    }

    /// Resolve any `CompletionDependency` waiting on the given GID.
    ///
    /// When a group completes and is demoted to stopped, we need to
    /// find any reserved groups that were waiting for this GID and
    /// mark their dependency as resolved. This enables them to be
    /// promoted on the next tick.
    ///
    /// Mirrors C++ `RequestGroupMan` dependency resolution that happens
    /// inside `fillRequestGroupFromReserver` when it encounters a
    /// dependency whose prerequisite has finished.
    pub fn resolve_dependencies_for(&self, completed_gid: crate::request::request_group::GroupId) {
        for group in self.reserved.iter_snapshot() {
            let g = group.recover();
            let dep_guard = g.dependency.recover();
            if let Some(ref dep) = *dep_guard {
                // Check if this is a CompletionDependency waiting on our GID.
                if let Some(completion_dep) =
                    dep.as_any()
                        .downcast_ref::<crate::request::request_group::CompletionDependency>()
                    && completion_dep.depends_on_gid == completed_gid {
                        completion_dep.mark_resolved();
                        debug!(
                            gid = g.gid().value(),
                            depends_on = completed_gid.value(),
                            "Resolved completion dependency"
                        );
                    }
            }
        }
    }
}

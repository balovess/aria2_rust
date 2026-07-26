//! Promotion logic: reserved → active group transition.
//!
//! Mirrors C++ `RequestGroupMan::fillRequestGroupFromReserver()`.
//! When active slots are available, the engine promotes groups from the
//! reserved queue to the active DashMap and spawns their download commands.

use std::sync::Arc;
use tracing::{debug, info, warn};

use super::RequestGroupMan;
use crate::request::request_group::DownloadStatus;
use crate::util::rwlock_ext::RwLockRecover;

impl RequestGroupMan {
    /// Promote groups from the reserved queue to the active DashMap
    /// until `max_concurrent` is reached or the reserved queue is empty.
    ///
    /// Returns the list of groups that were promoted (so the engine can
    /// create and spawn their download commands).
    ///
    /// Mirrors C++ `RequestGroupMan::fillRequestGroupFromReserver()`.
    pub fn fill_from_reserver(&self) -> Vec<Arc<std::sync::RwLock<super::RequestGroup>>> {
        let max = self.max_concurrent();
        let current_active = self.active_count();
        let slots_available = max.saturating_sub(current_active);

        if slots_available == 0 || self.reserved.is_empty() {
            return Vec::new();
        }

        let mut promoted = Vec::new();

        for _ in 0..slots_available {
            let group = match self.reserved.pop_front() {
                Some(g) => g,
                None => break,
            };

            let gid = group.recover().gid();

            // Skip if pause requested — push back to front and stop loop.
            // In C++ this is: `if((*i)->isPauseRequested()) continue;`
            if group.recover().is_pause_requested() {
                self.reserved.push_front(group);
                debug!(gid = gid.value(), "Skipping paused group, stopping promotion");
                break;
            }

            // Check dependency resolution (C++ `isDependencyResolved()`).
            // If the dependency is not yet resolved, push the group back to
            // the front of the reserved queue and stop promoting — all
            // subsequent groups would also need to wait.
            if !group.recover().is_dependency_resolved() {
                let dep_desc = group
                    .recover()
                    .dependency
                    .recover()
                    .as_ref()
                    .map(|d| d.description())
                    .unwrap_or_default();
                self.reserved.push_front(group);
                debug!(gid = gid.value(), "Dependency not resolved: {}", dep_desc);
                break;
            }

            // Transition: set status Active, move to active DashMap.
            {
                let mut g = group.recover_mut();
                // Only promote groups that are in Waiting or Paused status.
                match g.status() {
                    DownloadStatus::Waiting | DownloadStatus::Paused => {
                        g.start().ok(); // Sets status to Active
                        g.control_flags.clear_pause();
                    }
                    other => {
                        warn!(
                            gid = gid.value(),
                            ?other,
                            "Unexpected status in reserved queue, skipping"
                        );
                        continue;
                    }
                }
            }

            // Insert into active DashMap.
            let gid_val = gid;
            self.active.insert(gid_val, Arc::clone(&group));

            info!(
                gid = gid_val.value(),
                "Promoted group from reserved to active"
            );
            promoted.push(group);
        }

        promoted
    }

    /// Current number of active groups (status == Active).
    /// This counts groups in the active DashMap that have `Active` status,
    /// excluding seed-only groups when `detach_share_only` is set (TODO).
    pub fn active_count(&self) -> usize {
        self.active
            .iter()
            .filter(|entry| {
                let g = entry.recover();
                matches!(g.status(), DownloadStatus::Active)
            })
            .count()
    }

    /// Check whether the number of active downloads exceeds `max_concurrent`.
    pub fn exceeds_max_concurrent(&self) -> bool {
        let max = self.max_concurrent();
        max > 0 && self.active_count() > max
    }

    /// Reduce the number of active downloads to the `max_concurrent` limit
    /// by pausing the excess groups.
    ///
    /// Mirrors C++ `RequestGroupMan::reduceActiveDownloadsToLimit()`.
    pub fn reduce_to_limit(&self) -> usize {
        let max = self.max_concurrent();
        if max == 0 {
            return 0;
        }

        let excess = self.active_count().saturating_sub(max);
        if excess == 0 {
            return 0;
        }

        let mut paused = 0;
        for entry in self.active.iter() {
            if paused >= excess {
                break;
            }
            let mut g = entry.recover_mut();
            if matches!(g.status(), DownloadStatus::Active) {
                g.pause().ok(); // Sets status to Paused
                paused += 1;
            }
        }

        if paused > 0 {
            info!(paused, "Paused excess active downloads to respect max_concurrent limit");
        }
        paused
    }
}

//! Promotion logic: reserved → active group transition.
//!
//! Mirrors C++ `RequestGroupMan::fillRequestGroupFromReserver()`.
//! When active slots are available, the engine promotes groups from the
//! reserved queue to the active DashMap and spawns their download commands.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `fill_from_reserver()` | `fillRequestGroupFromReserver()` |
//! | `configure_request_group()` | `configureRequestGroup()` |
//! | `drop_piece_storage()` | `dropPieceStorage()` |

use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::request::request_group::DownloadStatus;
use crate::util::rwlock_ext::RwLockRecover;

impl super::RequestGroupMan {
    /// Promote groups from the reserved queue to the active DashMap
    /// until `max_concurrent` is reached or the reserved queue is empty.
    ///
    /// Returns the list of groups that were promoted (so the engine can
    /// create and spawn their download commands).
    ///
    /// # C++ Flow
    ///
    /// Mirrors `RequestGroupMan::fillRequestGroupFromReserver()`:
    /// 1. Remove paused/dependency-blocked groups → pending list
    /// 2. `dropPieceStorage()` — release piece storage from paused state
    /// 3. `configureRequestGroup()` — set URI selector (feedback/inorder/adaptive)
    /// 4. Set state to Active, increment numActive
    /// 5. `createInitialCommand()` — create download commands (with error handling)
    /// 6. Fire `on-download-start` hook
    /// 7. Re-insert pending groups at front of reserved queue
    pub fn fill_from_reserver(&self) -> Vec<Arc<std::sync::RwLock<super::RequestGroup>>> {
        let max = self.max_concurrent();
        let current_active = self.active_count();
        let slots_available = max.saturating_sub(current_active);

        if slots_available == 0 || self.reserved.is_empty() {
            return Vec::new();
        }

        let mut promoted = Vec::new();
        let mut pending = Vec::new(); // Paused/dependency-blocked groups

        for _ in 0..slots_available {
            let group = match self.reserved.pop_front() {
                Some(g) => g,
                None => break,
            };

            let gid = group.recover().gid();

            // Skip if pause requested — collect in pending.
            // C++: `if((*i)->isPauseRequested()) continue;`
            // Unlike the old Rust code which broke on the first paused group,
            // C++ continues iterating (skipping paused groups). We match C++
            // by collecting paused groups into `pending` and continuing.
            if group.recover().is_pause_requested() {
                debug!(
                    gid = gid.value(),
                    "Skipping paused group, adding to pending"
                );
                pending.push(group);
                continue;
            }

            // Check dependency resolution (C++ `isDependencyResolved()`).
            // If not resolved, collect in pending — all subsequent groups
            // would also need to wait.
            if !group.recover().is_dependency_resolved() {
                let dep_desc = group
                    .recover()
                    .dependency
                    .recover()
                    .as_ref()
                    .map(|d| d.description())
                    .unwrap_or_default();
                debug!(gid = gid.value(), "Dependency not resolved: {}", dep_desc);
                pending.push(group);
                continue;
            }

            // ── Drop piece storage before promotion ────────────────────
            // C++: `groupToAdd->dropPieceStorage()` — paused downloads
            // hold piece storage references; releasing them prevents stale
            // state when the download restarts.
            group.recover().drop_piece_storage();

            // ── Configure request group ────────────────────────────────
            // C++: `configureRequestGroup(groupToAdd)` — sets URI selector
            // based on the "uri-selector" option (feedback/inorder/adaptive).
            // In Rust, URI selection is handled differently (the selector
            // is embedded in the download command logic rather than stored
            // on the group), but we still need to validate the configuration.
            // No-op for now: URI selector logic lives in mirror_coordinator.

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

        // ── Re-insert pending groups at front of reserved queue ─────────
        // C++: `reservedGroups_.insert(reservedGroups_.begin(), ...)`
        // Pending groups (paused / dependency-blocked) go back to the front
        // so they are checked again on the next tick.
        for group in pending.into_iter().rev() {
            self.reserved.push_front(group);
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
            info!(
                paused,
                "Paused excess active downloads to respect max_concurrent limit"
            );
        }
        paused
    }
}

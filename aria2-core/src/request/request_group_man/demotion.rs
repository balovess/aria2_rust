//! Demotion logic: active → stopped group transition.
//!
//! Mirrors C++ `RequestGroupMan::removeStoppedGroup()`. When a group's
//! `num_commands` drops to 0 and its status is terminal (Complete, Error,
//! Removed), the engine removes it from the active DashMap and stores
//! the result for RPC `tellStopped` queries.

use std::sync::Arc;
use tracing::{debug, info, warn};

use super::RequestGroup;
use crate::engine::download_event_hooks::{
    DownloadEvent, DownloadEventContext, DownloadEventHooks, determine_stop_event,
};
use crate::engine::post_download_handler::{
    build_handler_chain, extract_download_info, run_post_download_processing,
};
use crate::request::request_group::DownloadStatus;
use crate::request::request_group::GroupId;
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

            // C++ processes every zero-command group. A shutdown-halted
            // non-terminal group becomes an IN_PROGRESS result, while normal
            // terminal states become their corresponding stopped result.
            let is_terminal = matches!(
                status,
                DownloadStatus::Complete | DownloadStatus::Error(_) | DownloadStatus::Removed
            );
            let is_halted = g.is_halt_requested();

            if num_cmd == 0 && (is_terminal || is_halted) {
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

    /// Move groups that have no in-flight commands and are not in a terminal
    /// state (Complete / Error / Removed) out of the active DashMap and back
    /// into the reserved queue.
    ///
    /// This closes the pause loop: when `aria2.pause` / `aria2.forcePause`
    /// is issued, the group is marked `Paused` and its download command
    /// terminates. The group must then return to the reserved queue — not to
    /// the stopped results — so it can be unpaused and re-promoted.
    ///
    /// Mirrors C++ `ProcessStoppedRequestGroup` which re-queues groups with
    /// `isPauseRequested()`. Also recovers "orphan" groups whose commands all
    /// ended without a terminal status transition (e.g. a pause that was
    /// undone before the task fully exited), re-queuing them as `Waiting` so
    /// promotion re-spawns them.
    ///
    /// Returns the number of groups re-queued.
    pub fn requeue_non_terminal_groups(&self, event_hooks: Option<&DownloadEventHooks>) -> usize {
        let mut to_move: Vec<(GroupId, Arc<std::sync::RwLock<RequestGroup>>)> = Vec::new();

        for entry in self.active.iter() {
            let g = entry.recover();
            if g.num_commands() != 0 {
                continue;
            }
            let status = g.status();
            if matches!(
                status,
                DownloadStatus::Complete | DownloadStatus::Error(_) | DownloadStatus::Removed
            ) || (g.is_halt_requested() && !g.is_pause_requested())
            {
                continue;
            }
            to_move.push((*entry.key(), entry.value().clone()));
        }

        let mut requeued = 0;
        for (gid, group) in to_move {
            let was_pause_requested = group.recover().is_pause_requested();
            let was_restart_requested = group.recover().is_restart_requested();
            let was_halt_requested = group.recover().is_halt_requested();

            if self.active.remove(&gid).is_none() {
                continue;
            }

            // Release runtime resources (C++ `releaseRuntimeResource()`):
            // drop the rate limiter handle and piece/segment storage so the
            // download can be re-promoted cleanly. Unlike the terminal
            // `demote_group` path, the `download_context` (file entries,
            // piece hashes, BT info) is preserved because a paused download
            // may resume and still needs that metadata.
            {
                let g = group.recover();
                g.drop_piece_storage();
                g.rate_limiter.recover_mut().take(); // Drop rate limiter
            }

            {
                let mut g = group.recover_mut();
                if was_restart_requested {
                    g.apply_pending_options();
                }
                if matches!(g.status(), DownloadStatus::Paused) {
                    if g.is_restart_requested() {
                        // C++ `releaseRuntimeResource()` clears the pause
                        // request for restart-requested groups (paused by
                        // `reduceActiveDownloadsToLimit`) so they auto-resume
                        // once a slot is available. `resume()` also clears the
                        // restart flag.
                        g.resume().ok();
                    }
                } else {
                    // Orphan: no terminal status and no commands left.
                    // Re-queue as Waiting so promotion re-spawns it.
                    g.mark_waiting();
                }
            }

            // Fire on-download-pause hook for groups that are actually
            // pausing (not the reduce-to-limit auto-restart case).
            if let (Some(hooks), true, false, false) = (
                event_hooks,
                was_pause_requested,
                was_restart_requested,
                was_halt_requested,
            ) {
                hooks.fire_event(DownloadEvent::Pause, &group.recover());
            }

            self.reserved.push_front(group);

            requeued += 1;
            debug!(
                gid = gid.value(),
                "Re-queued non-terminal group back to reserved queue"
            );
        }

        if requeued > 0 {
            info!(requeued, "Re-queued non-terminal groups to reserved");
        }
        requeued
    }

    /// Process all stopped groups: find them, demote them, and return
    /// the list of demoted GIDs for event notification.
    ///
    /// This is the main entry point the engine should call each tick.
    /// Groups that are paused (or otherwise have no terminal status) are
    /// first re-queued to the reserved queue; only genuinely terminal groups
    /// are demoted to stopped results. After demoting each group, if the
    /// group completed successfully:
    /// 1. Runs post-download processing (Metalink/BT child group creation)
    /// 2. Resolves any `CompletionDependency` waiting on that GID
    /// 3. Fires the appropriate download event hook (complete/error/pause/stop)
    ///
    /// Mirrors C++ `ProcessStoppedRequestGroup` which calls
    /// `postDownloadProcessing()` on completed groups and
    /// `executeStopHook()` for lifecycle events.
    pub fn remove_stopped_groups(
        &self,
        event_hooks: Option<&DownloadEventHooks>,
    ) -> Vec<crate::request::request_group::GroupId> {
        // Re-queue paused / orphan groups before the stopped scan so the
        // demotion below only sees genuinely terminal groups.
        self.requeue_non_terminal_groups(event_hooks);

        let demoted = self.find_stopped_groups();
        let mut gids = Vec::with_capacity(demoted.len());

        for dg in demoted {
            let gid = dg.group.recover().gid();
            let status = dg.group.recover().status();
            let is_pause_requested = dg.group.recover().is_pause_requested();

            // ── Extract event context BEFORE demoting ──────────────────
            // Must extract while download context is still available.
            // C++ fires hooks while the group is still alive.
            let event_ctx =
                event_hooks.map(|_| DownloadEventContext::from_group(&dg.group.recover()));

            // ── Post-download processing (BEFORE demoting) ──────────────
            // C++: `group->postDownloadProcessing(nextGroups)` is called
            // before the group is removed from the active list, so the
            // disk adaptor and download context are still available.
            let child_groups = if matches!(status, DownloadStatus::Complete) {
                self.run_post_download_processing(&dg.group)
            } else {
                Vec::new()
            };

            // Now demote the group (releases download context, etc.)
            self.demote_group(gid, dg.result);

            // ── Insert child groups into reserved queue ─────────────────
            // C++: `insertReservedGroup(0, nextGroups)` inserts at front
            // so child groups are promoted before other waiting downloads.
            if !child_groups.is_empty() {
                let child_count = child_groups.len();
                self.insert_reserved_at_front(child_groups);
                info!(
                    parent_gid = gid.value(),
                    children = child_count,
                    "Inserted child groups at front of reserved queue"
                );
            }

            // ── Resolve dependencies ────────────────────────────────────
            // When a download completes successfully, resolve any
            // CompletionDependency waiting on this GID.
            if matches!(status, DownloadStatus::Complete) {
                self.resolve_dependencies_for(gid);
            }

            // ── Fire download event hooks ───────────────────────────────
            // C++: `executeStopHook()` is called in ProcessStoppedRequestGroup.
            // If paused → on-download-pause. If complete → on-download-complete.
            // If error → on-download-error. Otherwise → on-download-stop.
            if let (Some(hooks), Some(ctx)) = (event_hooks, event_ctx) {
                let is_complete = matches!(status, DownloadStatus::Complete);
                let is_error = matches!(status, DownloadStatus::Error(_));

                if let Some(event) = determine_stop_event(is_complete, is_error, is_pause_requested)
                {
                    // Resolve the hook command from per-group options
                    // (already extracted in event_ctx) or global hooks.
                    let command = match event {
                        DownloadEvent::Pause => ctx.on_download_pause.as_deref(),
                        DownloadEvent::Complete => ctx.on_download_complete.as_deref(),
                        DownloadEvent::Error => ctx.on_download_error.as_deref(),
                        DownloadEvent::Stop => ctx.on_download_stop.as_deref(),
                        _ => None,
                    };

                    // NOTE: every branch below reaches the hook bus, because
                    // the bus also drives the RPC WebSocket notifications
                    // (aria2.onDownloadComplete / onDownloadError). Skipping
                    // the call when no per-group command is configured — as an
                    // earlier revision did for `Some("")` — silently dropped
                    // those notifications.
                    match command {
                        Some(cmd) if !cmd.is_empty() => {
                            hooks.fire_event_with_params(
                                event,
                                &ctx.gid_hex,
                                ctx.num_files,
                                &ctx.first_file_path,
                                cmd,
                            );
                        }
                        // No per-group command: fall back to global hooks
                        // (and still notify observers unconditionally).
                        _ => hooks.fire_event(event, &dg.group.recover()),
                    }
                }
            }

            gids.push(gid);
        }

        gids
    }

    /// Run post-download processing on a completed group.
    ///
    /// Mirrors C++ `RequestGroup::postDownloadProcessing()`. Extracts
    /// download metadata, builds the handler chain, and creates child
    /// groups if a handler matches. Also sets `followed_by_gids` on
    /// the parent group before it's demoted.
    fn run_post_download_processing(
        &self,
        group: &Arc<std::sync::RwLock<RequestGroup>>,
    ) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
        let g = group.recover();

        // Extract download info while context is still available.
        let info = extract_download_info(&g);

        // Build handler chain based on download options.
        let handlers = build_handler_chain(&info.options);

        // Run the processing chain.
        // Can't use `&[&dyn]` directly from Vec<Box>, need to create refs.
        let handler_refs: Vec<&dyn crate::engine::post_download_handler::PostDownloadHandler> =
            handlers.iter().map(|h| h.as_ref()).collect();

        let child_groups = run_post_download_processing(&info, &handler_refs);

        // Set followed_by_gids on the parent group.
        // C++: `requestGroup->followedBy(std::begin(newRgs), std::end(newRgs))`.
        if !child_groups.is_empty() {
            for child in &child_groups {
                g.add_followed_by_gid(child.recover().gid());
            }
        }

        child_groups
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
                // Resolve generic completion dependencies when the
                // prerequisite group reaches stopped/completed state.
                if let Some(completion_dep) =
                    dep.as_any()
                        .downcast_ref::<crate::request::request_group::CompletionDependency>()
                    && completion_dep.depends_on_gid == completed_gid
                {
                    completion_dep.mark_resolved();
                    debug!(
                        gid = g.gid().value(),
                        depends_on = completed_gid.value(),
                        "Resolved completion dependency"
                    );
                }

                #[cfg(feature = "bittorrent")]
                if let Some(bt_dep) = dep
                    .as_any()
                    .downcast_ref::<crate::request::request_group::BtDependency>()
                    && bt_dep.depends_on_gid() == completed_gid
                    && let Some(metadata_path) = bt_dep.metadata_path()
                    && let Err(error) = bt_dep.resolve_metadata_file(metadata_path)
                {
                    warn!(
                        gid = g.gid().value(),
                        depends_on = completed_gid.value(),
                        error = %error,
                        "Failed to resolve torrent metadata dependency"
                    );
                }
            }
        }
    }
}

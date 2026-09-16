use super::*;

/// Promote reserved groups and create their protocol tasks.
///
/// Promotion is kept in one helper because the engine may need to run it both
/// for groups present before startup and immediately after a completion frees
/// a slot. The latter is the event-driven replacement for the old idle scan.
pub(super) fn promote_reserved_groups(
    ctx: &EngineLoopContext,
    running_downloads: &mut Vec<(GroupId, RunningDownload)>,
    halt_requested: bool,
    force_halt_requested: bool,
    next_generation: &mut CommandGeneration,
    completion_tx: &mpsc::UnboundedSender<(GroupId, CommandGeneration, TaskResult)>,
) {
    // Once a halt has been requested the engine must stop admitting new work.
    let promoted = if halt_requested || force_halt_requested {
        Vec::new()
    } else {
        ctx.group_man.fill_from_reserver()
    };

    for group in &promoted {
        let gid = group.recover().gid();
        group.recover().clear_connection_contexts();
        let generation = *next_generation;
        *next_generation = next_generation.wrapping_add(1);
        match spawn_download_task(
            Arc::clone(group),
            CommandDependencies {
                dns_cache: Arc::clone(&ctx.dns_cache),
                global_limiter: ctx.global_limiter.clone(),
                #[cfg(feature = "bittorrent")]
                public_tracker_catalog: Arc::clone(&ctx.public_tracker_catalog),
                #[cfg(feature = "bittorrent")]
                bt_registry: Arc::clone(&ctx.bt_registry),
                #[cfg(feature = "bittorrent")]
                bt_listener: Arc::clone(&ctx.bt_listener),
                #[cfg(feature = "bittorrent")]
                lpd_manager: Arc::clone(&ctx.lpd_manager),
            },
            generation,
            completion_tx.clone(),
        ) {
            Some((handle, shutdown_tx)) => {
                let timeout = group.recover().timeout();
                running_downloads.push((
                    gid,
                    RunningDownload {
                        _handle: handle,
                        shutdown: Some(shutdown_tx),
                        generation,
                        last_activity: Instant::now(),
                        timeout,
                    },
                ));
                debug!(
                    gid = gid.value(),
                    "Spawned download task for promoted group"
                );

                // Fire on-download-start hook.
                // C++: `util::executeHookByOptName(groupToAdd, e->getOption(),
                //            PREF_ON_DOWNLOAD_START)`
                ctx.event_hooks
                    .fire_event(DownloadEvent::Start, &group.recover());
            }
            None => {
                warn!(
                    gid = gid.value(),
                    "Failed to spawn download task for promoted group"
                );
                ctx.group_man
                    .fail_spawned_group(gid, "Failed to spawn download task");
            }
        }
    }

    if !promoted.is_empty() {
        debug!("Promoted {} groups from reserved to active", promoted.len());
        mark_session_dirty(ctx);
    }
}

pub(super) trait EngineCommandQueue {
    fn try_command(&mut self) -> Result<EngineCommand, EngineCommandTryRecvError>;
}

impl EngineCommandQueue for EngineCommandReceiver {
    fn try_command(&mut self) -> Result<EngineCommand, EngineCommandTryRecvError> {
        self.try_recv()
    }
}

pub(super) struct PrefetchedEngineCommand<'a, R> {
    pub(super) first: Option<EngineCommand>,
    pub(super) receiver: &'a mut R,
}

impl<R: EngineCommandQueue> EngineCommandQueue for PrefetchedEngineCommand<'_, R> {
    fn try_command(&mut self) -> Result<EngineCommand, EngineCommandTryRecvError> {
        self.first
            .take()
            .map_or_else(|| self.receiver.try_command(), Ok)
    }
}

/// Process all pending `EngineCommand` messages from the channel.
pub(super) async fn process_engine_commands<R: EngineCommandQueue>(
    ctx: &mut EngineLoopContext,
    cmd_rx: &mut R,
    running_downloads: &mut Vec<(GroupId, RunningDownload)>,
    halt_requested: &mut bool,
    force_halt_requested: &mut bool,
    completion_tx: &mpsc::UnboundedSender<(GroupId, CommandGeneration, TaskResult)>,
) -> bool {
    let mut processed = false;
    while let Ok(cmd) = cmd_rx.try_command() {
        processed = true;
        match cmd {
            EngineCommand::AddDownload { group } => {
                let man = &ctx.group_man;
                let gid = group.recover().gid();
                // Add to the reserved queue; the current event pass promotes
                // it immediately after command processing.
                man.add_group_arc(group);
                mark_session_dirty(ctx);
                info!(gid = gid.value(), "Added download to reserved queue");
            }
            #[cfg(all(feature = "metalink", feature = "bittorrent"))]
            EngineCommand::AddMetalinkGraph { graph } => {
                let man = &ctx.group_man;
                match man.add_metalink_graph(graph) {
                    Ok((metadata_gid, payload_gid)) => {
                        mark_session_dirty(ctx);
                        info!(
                            metadata_gid = metadata_gid.value(),
                            payload_gid = payload_gid.value(),
                            "Added Metalink graph to reserved queue"
                        )
                    }
                    Err(error) => warn!(%error, "Failed to add Metalink graph"),
                }
            }

            EngineCommand::RemoveDownload { gid } => {
                let man = &ctx.group_man;
                if let Err(e) = man.remove_group(gid) {
                    warn!(gid = gid.value(), error = %e, "Failed to remove download");
                    continue;
                }
                mark_session_dirty(ctx);
                // Let the command observe the RequestGroup halt signal. This
                // preserves the protocol-owned cleanup seam: HTTP downloaders
                // cancel requests, flush queued writes, and save progress
                // before reporting the user removal.
            }

            EngineCommand::ForceRemoveDownload { gid } => {
                let man = &ctx.group_man;
                if let Err(e) = man.force_remove_group(gid) {
                    warn!(gid = gid.value(), error = %e, "Failed to force-remove download");
                    continue;
                }
                mark_session_dirty(ctx);
                // Force removal still travels through the command's halt
                // check so protocol-specific writers can persist a coherent
                // checkpoint before the task is accounted as removed.
            }

            EngineCommand::Pause { gid } => {
                let man = &ctx.group_man;
                let should_apply = man.find_group(gid).is_some_and(|group| {
                    matches!(
                        group.recover().status(),
                        DownloadStatus::Active | DownloadStatus::Waiting
                    )
                });
                if should_apply {
                    if let Err(e) = man.pause_group(gid) {
                        warn!(gid = gid.value(), error = %e, "Failed to pause download");
                    } else {
                        mark_session_dirty(ctx);
                    }
                }
            }

            EngineCommand::ForcePause { gid } => {
                let man = &ctx.group_man;
                let should_apply = man.find_group(gid).is_some_and(|group| {
                    matches!(
                        group.recover().status(),
                        DownloadStatus::Active | DownloadStatus::Waiting
                    )
                });
                if should_apply {
                    if let Err(e) = man.force_pause_group(gid) {
                        warn!(gid = gid.value(), error = %e, "Failed to force-pause download");
                    } else {
                        mark_session_dirty(ctx);
                    }
                }
                // NOTE: the running task is intentionally NOT aborted here.
                // force_pause_group() marks the group Paused; the download
                // loop observes this via check_cancelled() and terminates by
                // itself. Aborting the handle would skip the completion
                // notification, leaving num_commands stuck above 0 so the
                // paused group could never be re-queued to the reserved list.
            }

            EngineCommand::Unpause { gid } => {
                let man = &ctx.group_man;
                let should_apply = man
                    .find_group(gid)
                    .is_some_and(|group| group.recover().status().is_paused());
                if should_apply {
                    if let Err(e) = man.unpause_group(gid) {
                        warn!(gid = gid.value(), error = %e, "Failed to unpause download");
                    } else {
                        mark_session_dirty(ctx);
                    }
                }
            }

            EngineCommand::TaskCompleted { gid, result: _ } => {
                // NOTE: TaskCompleted via the engine command channel is NOT the
                // primary completion path. Spawned tasks report completion via
                // the `completion_tx` channel, which is handled by
                // `process_task_completions`. This variant exists for external
                // callers (e.g., RPC) that need to signal completion without
                // going through the completion channel. To avoid a double
                // decrement of `num_commands`, we do NOT decrement here.
                debug!(
                    gid = gid.value(),
                    "Received TaskCompleted via engine command channel (external signal)"
                );
            }

            EngineCommand::PauseAll => {
                let man = &ctx.group_man;
                man.pause_all();
                mark_session_dirty(ctx);
            }

            EngineCommand::ForcePauseAll => {
                let man = &ctx.group_man;
                man.force_pause_all();
                mark_session_dirty(ctx);
                // Like ForcePause, tasks are left to terminate on their own
                // via the Paused status so num_commands stays balanced and
                // the groups can return to the reserved queue.
            }

            EngineCommand::UnpauseAll => {
                let man = &ctx.group_man;
                man.unpause_all();
                mark_session_dirty(ctx);
            }

            EngineCommand::HaltAll { reason } => {
                let man = &ctx.group_man;
                man.halt_all(reason);
                mark_session_dirty(ctx);
                *halt_requested = true;
            }

            EngineCommand::ForceHaltAll { reason } => {
                let man = &ctx.group_man;
                man.force_halt_all(reason);
                mark_session_dirty(ctx);
                let removed = man.force_remove_reserved();
                if removed > 0 {
                    mark_session_dirty(ctx);
                }
                *force_halt_requested = true;

                // Allocation waiters are not driven by the protocol
                // cancellation token. Wake them before waiting for the
                // owning task, then repeat after the wait for allocations
                // queued during task teardown.
                cancel_running_file_allocations(ctx, running_downloads).await;
                for (gid, running) in running_downloads.iter_mut() {
                    let generation = running.generation;
                    let completed = request_shutdown_and_wait(running, FORCE_SHUTDOWN_WAIT).await;
                    if !completed {
                        // A timed-out task is aborted and cannot publish its
                        // normal completion. Feed the same completion path so
                        // command accounting and group demotion stay correct.
                        let _ = completion_tx.send((*gid, generation, TaskResult::Cancelled));
                    }
                }
                cancel_running_file_allocations(ctx, running_downloads).await;
                // Every entry has either completed or been explicitly
                // aborted above. The completion queue still owns lifecycle
                // accounting; removing the handles here lets the engine
                // reach the force-halt exit check in the same pass.
                running_downloads.clear();
            }

            EngineCommand::SetMaxConcurrent { max } => {
                let man = &ctx.group_man;
                let old_max = man.max_concurrent();
                man.set_max_concurrent(max);
                mark_session_dirty(ctx);
                info!(
                    "Max concurrent downloads set to {}",
                    if max == 0 {
                        "unlimited".to_string()
                    } else {
                        max.to_string()
                    }
                );

                // Mirrors C++ `RequestGroupMan::reduceActiveDownloadsToLimit()`.
                // When the limit is reduced at runtime (via changeGlobalOption),
                // immediately pause excess active downloads.
                if max > 0 && (old_max == 0 || (max as usize) < old_max) {
                    let paused = man.reduce_to_limit();
                    if paused > 0 {
                        info!(
                            paused,
                            "Paused excess active downloads after max-concurrent reduction"
                        );
                    }
                }
            }

            EngineCommand::SetGlobalRateLimit {
                download_limit,
                upload_limit,
            } => {
                let limiter = ctx
                    .global_limiter
                    .get_or_insert_with(RateLimiter::unlimited);
                limiter.set_download_rate(download_limit);
                limiter.set_upload_rate(upload_limit);

                // Keep the manager's option snapshot aligned with the live
                // limiter. This is also used by status/reporting code.
                let man = &ctx.group_man;
                man.set_global_speed_limit(download_limit, upload_limit);
                mark_session_dirty(ctx);
                info!(
                    download_limit = ?download_limit,
                    upload_limit = ?upload_limit,
                    "Global speed limits updated"
                );
            }

            #[cfg(feature = "bittorrent")]
            EngineCommand::SetPublicTrackerSources { sources } => {
                let mut config = ctx.public_tracker_catalog.config().await;
                config.sources = sources
                    .split([',', '\n'])
                    .map(str::trim)
                    .filter(|source| !source.is_empty())
                    .map(str::to_string)
                    .collect();
                ctx.public_tracker_catalog.set_config(config).await;
                mark_session_dirty(ctx);
                info!("Public tracker sources updated at runtime");
            }

            #[cfg(feature = "bittorrent")]
            EngineCommand::SetPublicTrackerUpdateInterval { seconds } => {
                let mut config = ctx.public_tracker_catalog.config().await;
                config.update_interval = Duration::from_secs(seconds.max(1));
                ctx.public_tracker_catalog.set_config(config).await;
                mark_session_dirty(ctx);
                info!(seconds, "Public tracker update interval changed at runtime");
            }

            #[cfg(feature = "bittorrent")]
            EngineCommand::SetPublicTrackersEnabled { enabled } => {
                let mut config = ctx.public_tracker_catalog.config().await;
                config.enabled = enabled;
                ctx.public_tracker_catalog.set_config(config).await;
                mark_session_dirty(ctx);
                info!(
                    enabled,
                    "Public tracker catalog enabled state changed at runtime"
                );
            }
        }
    }
    processed
}

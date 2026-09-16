use super::*;

/// Return the earliest maintenance deadline that can be derived from live
/// engine state. With no inactivity timeout and no pending save, the engine has
/// no maintenance timer and waits only for notifications.
pub(super) async fn next_maintenance_deadline(
    ctx: &EngineLoopContext,
    running_downloads: &[(GroupId, RunningDownload)],
) -> Option<Instant> {
    let timeout_deadline = running_downloads
        .iter()
        .filter_map(|(_, running)| {
            running
                .timeout
                .and_then(|timeout| running.last_activity.checked_add(timeout))
        })
        .min();

    let has_pending_downloads = !ctx.group_man.download_finished() || !running_downloads.is_empty();
    let save_deadline = if let Some(auto_save) = &ctx.auto_save {
        let save = auto_save.lock().await;
        save.next_deadline(has_pending_downloads)
    } else {
        None
    };

    [timeout_deadline, save_deadline]
        .into_iter()
        .flatten()
        .chain(ctx.server_stat_next_save)
        .min()
}

pub(super) async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Run maintenance work whose deadline has fired.
///
/// Timeouts and both persistence features are scheduled at their configured
/// deadlines. There is deliberately no fixed-rate scan here: after the work
/// is handled, the next loop iteration recomputes the next actual deadline.
pub(super) async fn run_deadline_maintenance(
    ctx: &mut EngineLoopContext,
    running_downloads: &mut [(GroupId, RunningDownload)],
) {
    // ── Timeout enforcement ──────────────────────────────────────────────
    // Abort tasks whose configured inactivity timeout has elapsed. The
    // timestamp comes from the protocol data path, not from disk progress;
    // payload may be buffered, verified, or waiting for a writer.
    let now = Instant::now();
    let mut timed_out = Vec::new();
    for (gid, rd) in running_downloads.iter_mut() {
        let last_network_activity = ctx
            .group_man
            .find_group(*gid)
            .map(|group| group.recover().last_network_activity());
        if let Some(last_network_activity) = last_network_activity
            && last_network_activity > rd.last_activity
        {
            rd.last_activity = last_network_activity;
        }

        if let Some(timeout) = rd.timeout
            && now.duration_since(rd.last_activity) >= timeout
        {
            // The group is now responsible for graceful shutdown. Clear the
            // deadline so a slow cleanup cannot turn the event loop into a
            // tight retry loop while the task winds down.
            rd.timeout = None;
            timed_out.push(*gid);
        }
    }

    if !timed_out.is_empty() {
        let man = &ctx.group_man;
        for gid in timed_out {
            if let Some(group) = man.get_group(gid) {
                let request_context = group.recover().latest_connection_context();
                let uris = group.recover().get_all_uris();
                if let Some(uri) = uris.first()
                    && let Ok(parsed) = reqwest::Url::parse(uri)
                    && let Some(host) = parsed.host_str()
                {
                    let protocol = parsed.scheme().to_ascii_lowercase();
                    ctx.server_stat_man
                        .mark_failure_with_protocol(host, &protocol, 408);
                    if let Some(context) = request_context
                        && group.recover().options().async_dns
                    {
                        let mut dns = ctx.dns_cache.lock().await;
                        dns.mark_bad_context(&context);
                        if !dns.has_good_address(&context.endpoint) {
                            dns.remove_cached(context.endpoint.hostname(), context.endpoint.port());
                        }
                    }
                }
            }
            if man.timeout_group(gid) {
                // Timeout is a graceful halt: the command observes the halt
                // flag, flushes its writer, saves resumable progress, and
                // publishes the single completion used for accounting. Do not
                // abort here; aborting would bypass protocol-specific cleanup
                // and can leave buffered bytes newer than the control file.
                warn!(
                    gid = gid.value(),
                    "Download task timed out, requesting graceful halt"
                );
            }
        }
    }

    // ── Unified persistence deadlines ───────────────────────────────────
    let has_pending_downloads = !ctx.group_man.download_finished() || !running_downloads.is_empty();
    if let Some(ref auto_save) = ctx.auto_save {
        let mut save = auto_save.lock().await;
        save.run_due(has_pending_downloads).await;
    }

    if let (Some(path), Some(interval), Some(deadline)) = (
        ctx.server_stat_save_path.as_ref(),
        ctx.server_stat_save_interval,
        ctx.server_stat_next_save,
    ) && now >= deadline
    {
        ctx.server_stat_next_save = Some(now + interval);
        match ctx.server_stat_man.save_to_file_async(path).await {
            Ok(count) => debug!(count, path = %path.display(), "Saved server statistics"),
            Err(error) => warn!(%error, path = %path.display(), "Failed to save server statistics"),
        }
    }
}

/// Perform cleanup that is caused by a completed download event.
///
/// These stores are bounded or event-owned, so scanning them on every idle
/// engine wake is unnecessary. A demotion is the natural point to prune old
/// results, stale server statistics, and idle FTP connections.
pub(super) async fn run_event_cleanup(ctx: &EngineLoopContext) {
    let pruned = ctx.group_man.prune_stopped_results(MAX_STOPPED_RESULTS);
    if pruned > 0 {
        debug!("Pruned {} excess stopped results", pruned);
    }

    // aria2_original removes statistics older than the configured freshness
    // window from the long-lived ServerStatMan.
    let stale_stats = ctx
        .server_stat_max_age
        .map(|max_age| ctx.server_stat_man.remove_stale(max_age))
        .unwrap_or(0);
    if stale_stats > 0 {
        debug!("Removed {} stale server statistics", stale_stats);
    }

    // reqwest owns the HTTP/TLS pool and enforces its idle timeout internally.
    // The FTP pool is engine-owned, so clean it when a download event gives
    // the engine a useful point to do the bounded scan.
    let evicted = ctx.ftp_pool.cleanup_stale_count().await;
    if evicted > 0 {
        debug!("Evicted {} stale FTP connections", evicted);
    }
}

pub(super) async fn request_shutdown_and_wait(
    running: &mut RunningDownload,
    wait: Duration,
) -> bool {
    if let Some(shutdown) = running.shutdown.take() {
        shutdown.cancel();
    }
    let completed = match tokio::time::timeout(wait, &mut running._handle).await {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            warn!(%error, "Download task panicked during shutdown");
            false
        }
        Err(_) => {
            warn!("Download task shutdown timed out");
            running._handle.abort();
            false
        }
    };
    if !completed {
        // A previously-consumed shutdown token must not turn cleanup into a
        // no-op. Force the same bounded lifecycle regardless of token state.
        running._handle.abort();
    }
    completed
}

/// Wake allocation waiters before waiting for their owning download task.
///
/// A download command may be suspended inside `enqueue_path` or
/// `enqueue_multi`. Those functions wait on the allocation manager's result,
/// so cancelling the protocol task first cannot wake them promptly.
pub(super) async fn cancel_running_file_allocations(
    ctx: &EngineLoopContext,
    running_downloads: &[(GroupId, RunningDownload)],
) {
    for (gid, _) in running_downloads {
        let cancelled =
            crate::filesystem::file_allocation_man::cancel_gid(&ctx.file_alloc_man, gid.value())
                .await;
        if cancelled > 0 {
            debug!(
                gid = gid.value(),
                cancelled, "Cancelled file allocations for engine-owned task"
            );
        }
    }
}

/// Cleanup on engine exit.
///
/// Mirrors C++ `onEndOfRun()`: remove stopped groups, close files, save.
pub(super) async fn on_end_of_run(
    ctx: &EngineLoopContext,
    running_downloads: &mut Vec<(GroupId, RunningDownload)>,
) {
    info!("Engine loop cleanup: removing stopped groups and saving state");

    // Cancel only allocations owned by this engine. The allocation manager is
    // process-wide, so cancelling every entry here would interrupt a
    // download running in another engine.
    cancel_running_file_allocations(ctx, running_downloads).await;

    // Demote any remaining stopped groups.
    let demoted = {
        let man = &ctx.group_man;
        man.remove_stopped_groups(Some(&ctx.event_hooks))
    };
    if !demoted.is_empty() {
        info!("Demoted {} final groups on shutdown", demoted.len());
    }

    // Request protocol-level shutdown and wait for each task before dropping
    // the engine, bounded so a broken command cannot hang engine teardown.
    for (gid, mut rd) in running_downloads.drain(..) {
        let completed = request_shutdown_and_wait(&mut rd, SHUTDOWN_WAIT).await;
        debug!(
            gid = gid.value(),
            completed, "Finished running task shutdown"
        );
    }

    #[cfg(feature = "bittorrent")]
    {
        ctx.lpd_manager.stop_background_announce();
        ctx.lpd_manager.stop_receive_loop().await;
    }

    // Final control-file and session saves.
    if let Some(ref auto_save) = ctx.auto_save {
        let mut save = auto_save.lock().await;
        save.force_save().await;
    }

    if let Some(path) = &ctx.server_stat_save_path {
        match ctx.server_stat_man.save_to_file_async(path).await {
            Ok(count) => {
                debug!(count, path = %path.display(), "Saved server statistics on shutdown")
            }
            Err(error) => {
                warn!(%error, path = %path.display(), "Failed to save server statistics on shutdown")
            }
        }
    }
}

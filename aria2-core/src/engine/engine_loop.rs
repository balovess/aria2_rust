//! Engine main loop: promotion/demotion, EngineCommand dispatch, and
//! periodic housekeeping.
//!
//! Mirrors the C++ `DownloadEngine::run()` loop structure. Each tick:
//! 1. Process incoming `EngineCommand`s (add/remove/pause/unpause/halt etc.)
//! 2. Promote reserved groups to active when slots are available
//! 3. Spawn download tasks for promoted groups via `task_spawner`
//! 4. Collect completed task notifications and decrement `num_commands`
//! 5. Demote stopped groups from active to stopped results
//! 6. Run periodic housekeeping (session auto-save, socket pool eviction, etc.)
//! 7. Check exit condition

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::download_event_hooks::{DownloadEvent, DownloadEventHooks};
use super::engine_command::{EngineCommand, TaskResult};
use super::task_spawner::spawn_download_task;
use crate::dns::dns_cache::DnsCache;
use crate::filesystem::file_allocation_man::FileAllocationMan;
use crate::ftp::FtpConnectionPool;
use crate::request::request_group::GroupId;
use crate::request::request_group_man::RequestGroupMan;
use crate::session::auto_save_session::AutoSaveSession;
use crate::util::rwlock_ext::RwLockRecover;

/// Interval for periodic housekeeping tasks (session save, stats, etc.).
/// Mirrors C++ `DEFAULT_REFRESH_INTERVAL` (1 second).
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(1);

/// Maximum number of stopped results to keep before pruning.
/// Mirrors C++ `MAX_DOWNLOAD_RESULT` (default 1000).
const MAX_STOPPED_RESULTS: usize = 1000;

/// Context passed into the engine loop, holding shared state that the
/// loop needs to coordinate between EngineCommand processing, promotion,
/// demotion, and periodic tasks.
pub struct EngineLoopContext {
    /// The request group manager (active/reserved/stopped queues).
    pub group_man: Arc<tokio::sync::RwLock<RequestGroupMan>>,

    /// FTP connection pool for dependency injection into download commands.
    pub ftp_pool: Arc<FtpConnectionPool>,

    /// DNS cache for dependency injection.
    pub dns_cache: Arc<tokio::sync::Mutex<DnsCache>>,

    /// Auto-save session manager (optional).
    pub auto_save: Option<Arc<tokio::sync::Mutex<AutoSaveSession>>>,

    /// Download event hooks for firing on-download-start/complete/error/pause/stop.
    /// Mirrors C++ `util::executeHookByOptName()`.
    pub event_hooks: Arc<DownloadEventHooks>,

    /// File allocation manager for sequential disk pre-allocation.
    /// Mirrors C++ `DownloadEngine::fileAllocationMan_` (a `SequentialPicker`).
    /// When a download needs file allocation, the entry is queued here and
    /// processed one at a time to avoid disk thrashing.
    pub file_alloc_man: Arc<tokio::sync::RwLock<FileAllocationMan>>,

    /// Whether the engine should stay alive even with no active downloads
    /// (used for RPC listen mode). Mirrors C++ `keepRunning_`.
    pub keep_alive: bool,
}

/// Tracks a spawned download task for timeout enforcement and cleanup.
struct RunningDownload {
    /// JoinHandle for the spawned tokio task.
    _handle: JoinHandle<()>,
    /// Instant the task was spawned.
    started: Instant,
    /// Per-command timeout. `None` means the task never times out.
    timeout: Option<Duration>,
}

/// Run the main engine loop.
///
/// This function runs until:
/// - No active/reserved downloads remain AND `keep_alive` is false, OR
/// - A shutdown signal is received via `shutdown_rx`.
///
/// The loop processes `EngineCommand`s from `cmd_rx`, task completion
/// notifications from `completion_rx`, and runs periodic housekeeping.
pub async fn run_engine_loop(
    ctx: EngineLoopContext,
    mut cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    tick_interval: Duration,
) {
    info!("Engine loop started (tick={:?})", tick_interval);

    let mut running_downloads: Vec<(GroupId, RunningDownload)> = Vec::new();
    let mut last_housekeeping = Instant::now();
    let mut halt_requested = false;
    let mut force_halt_requested = false;

    // Completion channel: spawned tasks send (GID, TaskResult) here when done.
    let (completion_tx, mut completion_rx) = mpsc::unbounded_channel::<(GroupId, TaskResult)>();

    let mut ticker = tokio::time::interval(tick_interval);

    loop {
        // ── 1. Process all incoming EngineCommands ───────────────────────
        // Drain the command channel before doing anything else, so that
        // batch RPC requests (e.g. addUri followed by unpause) are applied
        // atomically within the same tick.
        process_engine_commands(
            &ctx,
            &mut cmd_rx,
            &mut running_downloads,
            &mut halt_requested,
            &mut force_halt_requested,
        )
        .await;

        // ── 2. Promote reserved → active + spawn download tasks ──────────
        // Mirrors C++ `fillRequestGroupFromReserver()`.
        let promoted = {
            let man = ctx.group_man.read().await;
            man.fill_from_reserver()
        };

        for group in &promoted {
            let gid = group.recover().gid();
            match spawn_download_task(
                Arc::clone(group),
                Arc::clone(&ctx.ftp_pool),
                Arc::clone(&ctx.dns_cache),
                completion_tx.clone(),
            )
            .await
            {
                Some(handle) => {
                    let timeout = group.recover().timeout();
                    running_downloads.push((
                        gid,
                        RunningDownload {
                            _handle: handle,
                            started: Instant::now(),
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
                    ctx.event_hooks.fire_event(DownloadEvent::Start, &group.recover());
                }
                None => {
                    warn!(
                        gid = gid.value(),
                        "Failed to spawn download task for promoted group"
                    );
                }
            }
        }

        if !promoted.is_empty() {
            debug!("Promoted {} groups from reserved to active", promoted.len());
        }

        // ── 3. Collect completed task notifications ──────────────────────
        // Process all pending task completion messages.
        process_task_completions(&ctx, &mut completion_rx, &mut running_downloads).await;

        // ── 4. Demote stopped groups (active → stopped results) ──────────
        // Mirrors C++ `removeStoppedGroup()`.
        let demoted_gids = {
            let man = ctx.group_man.read().await;
            man.remove_stopped_groups(Some(&ctx.event_hooks))
        };

        if !demoted_gids.is_empty() {
            debug!("Demoted {} groups to stopped", demoted_gids.len());
        }

        // ── 5. Periodic housekeeping ─────────────────────────────────────
        if last_housekeeping.elapsed() >= HOUSEKEEPING_INTERVAL {
            run_housekeeping(&ctx, &mut running_downloads).await;
            last_housekeeping = Instant::now();
        }

        // ── 6. Check exit condition ──────────────────────────────────────
        let man = ctx.group_man.read().await;
        let all_done = man.download_finished() && running_downloads.is_empty();
        drop(man);

        if (all_done && !ctx.keep_alive) || force_halt_requested {
            if force_halt_requested {
                info!("Force halt requested, shutting down engine");
            } else {
                info!("All downloads completed, engine shutting down");
            }
            break;
        }

        // ── 7. Wait for next tick or shutdown signal ─────────────────────
        tokio::select! {
            _ = ticker.tick() => {
                // Next tick
            }
            Ok(_) = &mut shutdown_rx => {
                info!("Shutdown signal received");
                // Process graceful halt
                let man = ctx.group_man.read().await;
                man.halt_all(crate::request::request_group::HaltReason::UserRequest);
                drop(man);
                halt_requested = true;

                // Give running tasks a chance to finish gracefully.
                // In C++ this is handled by the next iteration detecting
                // numCommand_ == 0 on halted groups.
            }
        }
    }

    // ── Cleanup on exit ──────────────────────────────────────────────────
    // Mirrors C++ `onEndOfRun()`.
    on_end_of_run(&ctx, &mut running_downloads).await;

    info!("Engine loop exited");
}

/// Process all pending `EngineCommand` messages from the channel.
async fn process_engine_commands(
    ctx: &EngineLoopContext,
    cmd_rx: &mut mpsc::UnboundedReceiver<EngineCommand>,
    running_downloads: &mut Vec<(GroupId, RunningDownload)>,
    halt_requested: &mut bool,
    force_halt_requested: &mut bool,
) {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            EngineCommand::AddDownload { group } => {
                let man = ctx.group_man.read().await;
                let gid = group.recover().gid();
                // Add to reserved queue — promotion happens on next tick.
                man.add_group_arc(group);
                info!(gid = gid.value(), "Added download to reserved queue");
            }

            EngineCommand::RemoveDownload { gid } => {
                let man = ctx.group_man.read().await;
                if let Err(e) = man.remove_group(gid) {
                    warn!(gid = gid.value(), error = %e, "Failed to remove download");
                }
                // Also abort any running task for this GID.
                running_downloads.retain(|(id, rd)| {
                    if *id == gid {
                        rd._handle.abort();
                        false
                    } else {
                        true
                    }
                });
            }

            EngineCommand::Pause { gid } => {
                let man = ctx.group_man.read().await;
                if let Err(e) = man.pause_group(gid) {
                    warn!(gid = gid.value(), error = %e, "Failed to pause download");
                }
            }

            EngineCommand::ForcePause { gid } => {
                let man = ctx.group_man.read().await;
                if let Err(e) = man.force_pause_group(gid) {
                    warn!(gid = gid.value(), error = %e, "Failed to force-pause download");
                }
                // Abort the running task for this GID.
                running_downloads.retain(|(id, rd)| {
                    if *id == gid {
                        rd._handle.abort();
                        false
                    } else {
                        true
                    }
                });
            }

            EngineCommand::Unpause { gid } => {
                let man = ctx.group_man.read().await;
                if let Err(e) = man.unpause_group(gid) {
                    warn!(gid = gid.value(), error = %e, "Failed to unpause download");
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
                let man = ctx.group_man.read().await;
                man.pause_all();
            }

            EngineCommand::ForcePauseAll => {
                let man = ctx.group_man.read().await;
                man.force_pause_all();
                // Abort all running download tasks.
                for (_, rd) in running_downloads.drain(..) {
                    rd._handle.abort();
                }
            }

            EngineCommand::UnpauseAll => {
                let man = ctx.group_man.read().await;
                man.unpause_all();
            }

            EngineCommand::HaltAll { reason } => {
                let man = ctx.group_man.read().await;
                man.halt_all(reason);
                *halt_requested = true;
            }

            EngineCommand::ForceHaltAll { reason } => {
                let man = ctx.group_man.read().await;
                man.force_halt_all(reason);
                *force_halt_requested = true;
                // Abort all running tasks immediately.
                for (_, rd) in running_downloads.drain(..) {
                    rd._handle.abort();
                }
            }

            EngineCommand::SetMaxConcurrent { max } => {
                let man = ctx.group_man.read().await;
                let old_max = man.max_concurrent();
                man.set_max_concurrent(max);
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

            EngineCommand::FileAllocationCompleted { gid } => {
                // Mirrors C++ FileAllocationCommand::executeInternal() when
                // finished() returns true: drop the picked entry and proceed
                // to spawn the actual download task.
                {
                    let mut alloc_man = ctx.file_alloc_man.write().await;
                    alloc_man.complete_current();
                }
                debug!(
                    gid = gid.value(),
                    "File allocation completed, spawning download task"
                );
                // The group is now in the active DashMap; the engine loop's
                // normal promotion/demotion cycle will handle spawning the
                // download task on the next tick.
            }

            EngineCommand::FileAllocationFailed { gid, error } => {
                {
                    let mut alloc_man = ctx.file_alloc_man.write().await;
                    alloc_man.drop_picked();
                }
                warn!(
                    gid = gid.value(),
                    error = %error,
                    "File allocation failed"
                );
                // Try to find the group in active set and mark it as halted.
                let man = ctx.group_man.read().await;
                for group in man.get_active_groups() {
                    if group.recover().gid() == gid {
                        group.recover().request_halt(crate::request::request_group::HaltReason::UserRequest);
                        break;
                    }
                }
            }
        }
    }
}

/// Process all pending task completion notifications.
async fn process_task_completions(
    ctx: &EngineLoopContext,
    completion_rx: &mut mpsc::UnboundedReceiver<(GroupId, TaskResult)>,
    running_downloads: &mut Vec<(GroupId, RunningDownload)>,
) {
    while let Ok((gid, result)) = completion_rx.try_recv() {
        // Remove from running_downloads list.
        running_downloads.retain(|(id, _)| *id != gid);

        // Decrement num_commands and update group status.
        let man = ctx.group_man.read().await;
        if let Some(group) = man.find_group(gid) {
            let prev = group.recover().dec_commands();
            debug!(
                gid = gid.value(),
                prev, "Task completed, decremented num_commands"
            );

            match result {
                TaskResult::Success => {
                    group.recover_mut().mark_complete();
                }
                TaskResult::Failed(e) => {
                    group.recover_mut().mark_error(e.to_string());
                }
                TaskResult::Cancelled => {
                    // Status is already set by the halt/pause handler.
                }
            }
        }
    }
}

/// Run periodic housekeeping tasks.
///
/// Mirrors the C++ refresh-interval-based tasks:
/// - Session auto-save
/// - Socket pool eviction (TODO)
/// - Stats calculation (TODO)
/// - Prune excess stopped results
async fn run_housekeeping(
    ctx: &EngineLoopContext,
    running_downloads: &mut Vec<(GroupId, RunningDownload)>,
) {
    // ── Timeout enforcement ──────────────────────────────────────────────
    // Abort tasks whose per-command timeout has elapsed.
    // Mirrors C++ per-command timeout (though C++ handles this differently
    // via the Command::STATUS_ACTIVE mechanism).
    let now = Instant::now();
    let mut timed_out = Vec::new();
    running_downloads.retain(|(gid, rd)| {
        if let Some(timeout) = rd.timeout
            && now.duration_since(rd.started) > timeout
        {
            timed_out.push(*gid);
            rd._handle.abort();
            return false;
        }
        true
    });

    for gid in timed_out {
        warn!(gid = gid.value(), "Download task timed out, aborting");
    }

    // ── Session auto-save ────────────────────────────────────────────────
    if let Some(ref auto_save) = ctx.auto_save {
        let mut save = auto_save.lock().await;
        save.save_if_dirty().await;
    }

    // ── Prune excess stopped results ─────────────────────────────────────
    {
        let man = ctx.group_man.read().await;
        let pruned = man.prune_stopped_results(MAX_STOPPED_RESULTS);
        if pruned > 0 {
            debug!("Pruned {} excess stopped results", pruned);
        }
    }

    // ── Socket pool eviction ─────────────────────────────────────────────
    // TODO: Implement socket pool eviction like C++ `evictSocketPool()`.
}

/// Cleanup on engine exit.
///
/// Mirrors C++ `onEndOfRun()`: remove stopped groups, close files, save.
async fn on_end_of_run(
    ctx: &EngineLoopContext,
    running_downloads: &mut Vec<(GroupId, RunningDownload)>,
) {
    info!("Engine loop cleanup: removing stopped groups and saving state");

    // Demote any remaining stopped groups.
    let demoted = {
        let man = ctx.group_man.read().await;
        man.remove_stopped_groups(Some(&ctx.event_hooks))
    };
    if !demoted.is_empty() {
        info!("Demoted {} final groups on shutdown", demoted.len());
    }

    // Abort any remaining running tasks.
    for (gid, rd) in running_downloads.drain(..) {
        rd._handle.abort();
        debug!(gid = gid.value(), "Aborted running task on shutdown");
    }

    // Final session save.
    if let Some(ref auto_save) = ctx.auto_save {
        let mut save = auto_save.lock().await;
        save.force_save().await;
    }
}

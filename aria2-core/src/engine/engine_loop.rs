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

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::download_event_hooks::{DownloadEvent, DownloadEventHooks};
use super::engine_command::{EngineCommand, TaskResult};
use super::task_spawner::spawn_download_task;
use crate::dns::dns_cache::DnsCache;
use crate::error::{Aria2Error, RecoverableError};
use crate::filesystem::file_allocation_man::FileAllocationMan;
use crate::ftp::FtpConnectionPool;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadResultCode, DownloadStatus, GroupId};
use crate::request::request_group_man::RequestGroupMan;
use crate::selector::server_stat_man::ServerStatMan;
use crate::session::auto_save_session::AutoSaveSession;
use crate::util::rwlock_ext::RwLockRecover;

/// Interval for periodic housekeeping tasks (session save, stats, etc.).
/// Mirrors C++ `DEFAULT_REFRESH_INTERVAL` (1 second).
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(1);

/// Maximum number of stopped results to keep before pruning.
/// Mirrors C++ `MAX_DOWNLOAD_RESULT` (default 1000).
const MAX_STOPPED_RESULTS: usize = 1000;
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

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

    /// Shared server statistics used by URI selectors and housekeeping.
    pub server_stat_man: Arc<ServerStatMan>,

    /// Process-wide rate limiter shared across all downloads.
    /// When `Some`, passed to each spawned `DownloadCommand` so that
    /// `ThrottledWriter` and segment download loops enforce a global
    /// bandwidth ceiling in addition to per-download limits.
    pub global_limiter: Option<RateLimiter>,
}

/// Tracks a spawned download task for timeout enforcement and cleanup.
type CommandGeneration = u64;

struct RunningDownload {
    /// JoinHandle for the spawned tokio task.
    _handle: JoinHandle<()>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// Stable identity of this command instance, independent of its GID.
    generation: CommandGeneration,
    /// Instant the task was spawned.
    started: Instant,
    /// Per-command timeout. `None` means the task never times out.
    timeout: Option<Duration>,
}

/// Mark the auto-save session as dirty so the next periodic housekeeping tick
/// (subject to `save-session-interval`) actually persists state.
///
/// C++ aria2's `AutoSaveCommand` unconditionally saves every interval; our
/// `AutoSaveSession` adds a dirty gate to avoid redundant disk writes. Every
/// caller that mutates download state (queue membership, status, options,
/// progress) must flip this flag or `save_if_dirty()` never writes.
fn mark_session_dirty(ctx: &EngineLoopContext) {
    if let Some(ref auto_save) = ctx.auto_save
        && let Ok(save) = auto_save.try_lock()
    {
        save.mark_dirty();
    }
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
    mut ctx: EngineLoopContext,
    mut cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    tick_interval: Duration,
) {
    info!("Engine loop started (tick={:?})", tick_interval);

    let mut running_downloads: Vec<(GroupId, RunningDownload)> = Vec::new();
    let mut completed_generations: HashSet<CommandGeneration> = HashSet::new();
    let mut next_generation: CommandGeneration = 1;
    let mut last_housekeeping = Instant::now();
    let mut halt_requested = false;
    let mut force_halt_requested = false;
    let mut shutdown_received = false;

    // Completion channel: spawned tasks send (GID, TaskResult) here when done.
    let (completion_tx, mut completion_rx) =
        mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();

    let mut ticker = tokio::time::interval(tick_interval);

    loop {
        // ── 1. Process all incoming EngineCommands ───────────────────────
        // Drain the command channel before doing anything else, so that
        // batch RPC requests (e.g. addUri followed by unpause) are applied
        // atomically within the same tick.
        process_engine_commands(
            &mut ctx,
            &mut cmd_rx,
            &mut running_downloads,
            &mut halt_requested,
            &mut force_halt_requested,
            &completion_tx,
        )
        .await;

        // ── 2. Promote reserved → active + spawn download tasks ──────────
        // Mirrors C++ `fillRequestGroupFromReserver()`.
        //
        // Once a halt has been requested the engine must stop admitting new
        // work. C++ enforces this in `FillRequestGroupCommand::execute()`,
        // which returns early when `e_->isHaltRequested()` — without this
        // gate a graceful shutdown would keep promoting reserved groups and
        // never converge.
        let promoted = if halt_requested || force_halt_requested {
            Vec::new()
        } else {
            let man = ctx.group_man.read().await;
            man.fill_from_reserver()
        };

        for group in &promoted {
            let gid = group.recover().gid();
            group.recover().clear_connection_contexts();
            let generation = next_generation;
            next_generation = next_generation.wrapping_add(1);
            match spawn_download_task(
                Arc::clone(group),
                Arc::clone(&ctx.ftp_pool),
                Arc::clone(&ctx.dns_cache),
                ctx.global_limiter.clone(),
                generation,
                completion_tx.clone(),
            )
            .await
            {
                Some((handle, shutdown_tx)) => {
                    let timeout = group.recover().timeout();
                    running_downloads.push((
                        gid,
                        RunningDownload {
                            _handle: handle,
                            shutdown_tx: Some(shutdown_tx),
                            generation,
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
                    ctx.event_hooks
                        .fire_event(DownloadEvent::Start, &group.recover());
                }
                None => {
                    warn!(
                        gid = gid.value(),
                        "Failed to spawn download task for promoted group"
                    );
                    // The group was inserted into the active DashMap by
                    // fill_from_reserver() but no command could be created
                    // (e.g. empty URI list or unsupported scheme). Remove it
                    // from active and record an error so it does not stay
                    // in the active list forever.
                    let man = ctx.group_man.read().await;
                    man.fail_spawned_group(gid, "Failed to spawn download task");
                }
            }
        }

        if !promoted.is_empty() {
            debug!("Promoted {} groups from reserved to active", promoted.len());
            // Promotion moved groups between queues: persist the new layout.
            mark_session_dirty(&ctx);
        }

        // ── 3. Collect completed task notifications ──────────────────────
        // Process all pending task completion messages.
        process_task_completions(
            &ctx,
            &mut completion_rx,
            &mut running_downloads,
            &mut completed_generations,
        )
        .await;

        // ── 4. Demote stopped groups (active → stopped results) ──────────
        // Mirrors C++ `removeStoppedGroup()`.
        let demoted_gids = {
            let man = ctx.group_man.read().await;
            man.remove_stopped_groups(Some(&ctx.event_hooks))
        };

        if !demoted_gids.is_empty() {
            debug!("Demoted {} groups to stopped", demoted_gids.len());
            // Demotion moved groups to stopped results: persist the change.
            mark_session_dirty(&ctx);
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

        // A graceful halt must wind the engine down even in keep-alive (RPC)
        // mode. C++ achieves this because every routine command (RPC
        // listener, fill-request-group, …) returns `true` — removing itself
        // from `commands_` — once `isHaltRequested()`, so `run()`'s
        // `while (!commands_.empty())` terminates. Without this branch
        // `aria2.shutdown` would hang forever whenever `--enable-rpc` is set,
        // since `all_done && !keep_alive` can never be true there.
        let graceful_done = halt_requested && running_downloads.is_empty();

        let force_done = force_halt_requested && running_downloads.is_empty();
        if force_done || graceful_done || (all_done && !ctx.keep_alive) {
            if force_halt_requested {
                info!("Force halt completed, shutting down engine");
            } else if graceful_done {
                info!("Graceful halt completed, engine shutting down");
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
            Ok(_) = &mut shutdown_rx, if !shutdown_received => {
                shutdown_received = true;
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
    ctx: &mut EngineLoopContext,
    cmd_rx: &mut mpsc::UnboundedReceiver<EngineCommand>,
    running_downloads: &mut [(GroupId, RunningDownload)],
    halt_requested: &mut bool,
    force_halt_requested: &mut bool,
    completion_tx: &mpsc::UnboundedSender<(GroupId, CommandGeneration, TaskResult)>,
) {
    while let Ok(cmd) = cmd_rx.try_recv() {
        // Every EngineCommand mutates session state (queue membership,
        // per-group status, or options), so mark the session dirty to make
        // the periodic auto-save persist these changes.
        mark_session_dirty(ctx);
        match cmd {
            EngineCommand::AddDownload { group } => {
                let man = ctx.group_man.read().await;
                let gid = group.recover().gid();
                // Add to reserved queue — promotion happens on next tick.
                man.add_group_arc(group);
                info!(gid = gid.value(), "Added download to reserved queue");
            }
            #[cfg(all(feature = "metalink", feature = "bittorrent"))]
            EngineCommand::AddMetalinkGraph { graph } => {
                let man = ctx.group_man.read().await;
                match man.add_metalink_graph(graph) {
                    Ok((metadata_gid, payload_gid)) => info!(
                        metadata_gid = metadata_gid.value(),
                        payload_gid = payload_gid.value(),
                        "Added Metalink graph to reserved queue"
                    ),
                    Err(error) => warn!(%error, "Failed to add Metalink graph"),
                }
            }

            EngineCommand::RemoveDownload { gid } => {
                let remove_result = {
                    let man = ctx.group_man.read().await;
                    man.remove_group(gid)
                };
                if let Err(e) = remove_result {
                    warn!(gid = gid.value(), error = %e, "Failed to remove download");
                    continue;
                }
                // Graceful removal is observed by the protocol command. If the
                // transport is waiting for a response, wake it through the
                // command's shutdown channel so the completion can be recorded
                // without waiting for a network timeout.
                for (_, running) in running_downloads.iter_mut().filter(|(id, _)| *id == gid) {
                    if !request_shutdown_and_wait(running).await {
                        let _ =
                            completion_tx.send((gid, running.generation, TaskResult::Cancelled));
                    }
                }
            }

            EngineCommand::ForceRemoveDownload { gid } => {
                let remove_result = {
                    let man = ctx.group_man.read().await;
                    man.force_remove_group(gid)
                };
                if let Err(e) = remove_result {
                    warn!(gid = gid.value(), error = %e, "Failed to force-remove download");
                    continue;
                }
                for (_, running) in running_downloads.iter_mut().filter(|(id, _)| *id == gid) {
                    if !request_shutdown_and_wait(running).await {
                        let _ =
                            completion_tx.send((gid, running.generation, TaskResult::Cancelled));
                    }
                }
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
                // NOTE: the running task is intentionally NOT aborted here.
                // force_pause_group() marks the group Paused; the download
                // loop observes this via check_cancelled() and terminates by
                // itself. Aborting the handle would skip the completion
                // notification, leaving num_commands stuck above 0 so the
                // paused group could never be re-queued to the reserved list.
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
                // Like ForcePause, tasks are left to terminate on their own
                // via the Paused status so num_commands stays balanced and
                // the groups can return to the reserved queue.
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
                {
                    let man = ctx.group_man.read().await;
                    man.force_halt_all(reason);
                }
                *force_halt_requested = true;

                for (_, running) in running_downloads.iter_mut() {
                    let _ = request_shutdown_and_wait(running).await;
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
                let man = ctx.group_man.read().await;
                man.set_global_speed_limit(download_limit, upload_limit);
                info!(
                    download_limit = ?download_limit,
                    upload_limit = ?upload_limit,
                    "Global speed limits updated"
                );
            }
        }
    }
}

/// Process all pending task completion notifications.
fn map_error_code(error: &Aria2Error) -> DownloadResultCode {
    match error {
        Aria2Error::Recoverable(RecoverableError::Timeout) => DownloadResultCode::TimeOut,
        Aria2Error::Checksum(_) => DownloadResultCode::ChecksumError,
        Aria2Error::JsonParse(_) => DownloadResultCode::JsonParseError,
        Aria2Error::MetalinkParse(_) => DownloadResultCode::MetalinkParseError,
        Aria2Error::BencodeParse(_) => DownloadResultCode::BencodeParseError,
        Aria2Error::BittorrentParse(_) => DownloadResultCode::BittorrentParseError,
        Aria2Error::MagnetParse(_) => DownloadResultCode::MagnetParseError,
        Aria2Error::Recoverable(RecoverableError::CannotResume) => DownloadResultCode::CannotResume,
        Aria2Error::FtpProtocol(_) => DownloadResultCode::FtpProtocolError,
        Aria2Error::HttpProtocol(_) => DownloadResultCode::HttpProtocolError,
        Aria2Error::Recoverable(RecoverableError::FtpProtocolError { .. }) => {
            DownloadResultCode::FtpProtocolError
        }
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError { .. }) => {
            DownloadResultCode::HttpProtocolError
        }
        Aria2Error::Recoverable(RecoverableError::HttpAuthFailed { .. }) => {
            DownloadResultCode::HttpAuthFailed
        }
        Aria2Error::Recoverable(RecoverableError::HttpTooManyRedirects { .. }) => {
            DownloadResultCode::HttpTooManyRedirects
        }
        Aria2Error::Recoverable(RecoverableError::ServerError { code })
            if *code == 401 || *code == 407 =>
        {
            DownloadResultCode::HttpAuthFailed
        }
        Aria2Error::Recoverable(RecoverableError::ServerError { code })
            if matches!(*code, 502..=504) =>
        {
            DownloadResultCode::HttpServiceUnavailable
        }
        Aria2Error::Recoverable(RecoverableError::ServerError { code }) if *code == 404 => {
            DownloadResultCode::ResourceNotFound
        }
        Aria2Error::Recoverable(RecoverableError::ServerError { code }) if *code == 503 => {
            DownloadResultCode::HttpServiceUnavailable
        }
        Aria2Error::Recoverable(RecoverableError::ServerError { .. }) => {
            DownloadResultCode::NetworkProblem
        }
        Aria2Error::Network(_) => DownloadResultCode::NetworkProblem,
        Aria2Error::FileOpen(_) => DownloadResultCode::FileOpenError,
        Aria2Error::FileCreate(_) => DownloadResultCode::FileCreateError,
        Aria2Error::FileIo(_) => DownloadResultCode::FileIoError,
        Aria2Error::DirCreate(_) => DownloadResultCode::DirCreateError,
        Aria2Error::NameResolve(_) => DownloadResultCode::NameResolveError,
        Aria2Error::Io(_) => DownloadResultCode::FileIoError,
        Aria2Error::InvalidArgument(_) => DownloadResultCode::OptionError,
        Aria2Error::Parse(_) => DownloadResultCode::UnknownError,
        Aria2Error::Fatal(crate::error::FatalError::Config(_)) => DownloadResultCode::OptionError,
        Aria2Error::Fatal(crate::error::FatalError::DiskSpaceExhausted) => {
            DownloadResultCode::NotEnoughDiskSpace
        }
        Aria2Error::Recoverable(_) => DownloadResultCode::NetworkProblem,
        _ => DownloadResultCode::UnknownError,
    }
}

async fn process_task_completions(
    ctx: &EngineLoopContext,
    completion_rx: &mut mpsc::UnboundedReceiver<(GroupId, CommandGeneration, TaskResult)>,
    running_downloads: &mut Vec<(GroupId, RunningDownload)>,
    completed_generations: &mut HashSet<CommandGeneration>,
) {
    while let Ok((gid, generation, result)) = completion_rx.try_recv() {
        // A task may race with force-remove/timeout cleanup and publish more
        // than one terminal notification. Account for exactly one completion.
        if !completed_generations.insert(generation) {
            debug!(
                gid = gid.value(),
                generation, "Ignoring duplicate task completion"
            );
            continue;
        }

        // A task finished: its status/progress changed, so persist it.
        mark_session_dirty(ctx);

        // Remove only this command instance. A RequestGroup may have several
        // active commands, just as C++ tracks several AbstractCommand objects
        // under one RequestGroup.
        running_downloads.retain(|(id, running)| *id != gid || running.generation != generation);

        // Decrement num_commands and update group status.
        let man = ctx.group_man.read().await;
        if let Some(group) = man.find_group(gid) {
            let prev = group.recover().dec_commands();
            let last_command = prev == 1;
            debug!(
                gid = gid.value(),
                prev, last_command, "Task completed, decremented num_commands"
            );

            match result {
                TaskResult::Success if last_command => {
                    let had_failure = group
                        .recover()
                        .command_failure
                        .load(std::sync::atomic::Ordering::Acquire);
                    if had_failure {
                        let message = group.recover().get_last_error_message();
                        let code = group.recover().get_last_error_code();
                        group.recover().mark_error_with_code(code, message);
                    } else {
                        let halt_reason = group.recover().get_halt_reason();
                        match halt_reason {
                            crate::request::request_group::HaltReason::UserRequest => {
                                group.recover().mark_removed();
                            }
                            crate::request::request_group::HaltReason::Timeout => {
                                group.recover().mark_timeout();
                            }
                            crate::request::request_group::HaltReason::ShutdownSignal => {}
                            crate::request::request_group::HaltReason::None => {
                                group.recover_mut().mark_complete();
                            }
                        }
                    }
                }
                TaskResult::Success => {
                    // C++ removes a RequestGroup only after its final
                    // AbstractCommand is destroyed. Keep the group active
                    // while other command instances are still running.
                }
                TaskResult::Failed(e) if !last_command => {
                    // A non-final command failure is recorded for the group,
                    // but terminal state is deferred until all commands have
                    // exited, matching C++ numCommand_ semantics.
                    let message = e.to_string();
                    let code = map_error_code(&e);
                    group.recover().set_last_error(code, message);
                    group
                        .recover()
                        .command_failure
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                TaskResult::Failed(e) => {
                    // Handle failures from the final command, including pause-induced
                    // termination, before treating them as errors.
                    // (`aria2.pause` / `aria2.forcePause`
                    // marks the group Paused and the download loop aborts via
                    // check_cancelled) must leave the group Paused — resumable —
                    // rather than recording an Error that can never be unpaused.
                    // Mirrors C++ where pause-requested groups return to the
                    // reserved queue.
                    let group_state = group.recover();
                    let was_pause_requested =
                        group_state.is_pause_requested() || group_state.is_paused_flag();
                    let is_pause_error = matches!(
                        &e,
                        Aria2Error::DownloadFailed(msg) if msg == "Download paused"
                    );
                    let was_halt_requested = group_state.is_halt_requested();
                    drop(group_state);

                    if was_pause_requested {
                        group.recover_mut().mark_paused();
                    } else if was_halt_requested {
                        // User removal is terminal and must become a REMOVED
                        // result after the command counter reaches zero.
                        match group.recover().get_halt_reason() {
                            crate::request::request_group::HaltReason::UserRequest => {
                                group.recover().mark_removed();
                            }
                            crate::request::request_group::HaltReason::Timeout => {
                                group.recover().mark_timeout();
                            }
                            crate::request::request_group::HaltReason::ShutdownSignal
                            | crate::request::request_group::HaltReason::None => {}
                        }
                        // A shutdown halt remains non-terminal so its result
                        // maps to IN_PROGRESS and is not re-queued after exit.
                    } else if is_pause_error {
                        // A pause was requested and then undone (`unpause`)
                        // before the task fully exited. Leave the group
                        // Waiting so the demotion layer re-queues it and
                        // promotion re-spawns the download.
                        let status = group.recover().status();
                        if !matches!(
                            status,
                            DownloadStatus::Complete
                                | DownloadStatus::Error(_)
                                | DownloadStatus::Removed
                        ) {
                            group.recover().mark_waiting();
                        }
                    } else {
                        group
                            .recover()
                            .mark_error_with_code(map_error_code(&e), e.to_string());
                    }
                    group
                        .recover()
                        .command_failure
                        .store(false, std::sync::atomic::Ordering::Release);
                }
                TaskResult::Cancelled => {
                    // Synthetic cancellation is emitted before Tokio abort, so
                    // finalize the group here rather than relying on the
                    // cancelled task to mutate its status.
                    let group_state = group.recover();
                    let was_pause_requested =
                        group_state.is_pause_requested() || group_state.is_paused_flag();
                    let halt_reason = group_state.get_halt_reason();
                    drop(group_state);

                    if was_pause_requested {
                        group.recover_mut().mark_paused();
                    } else if !last_command {
                        group
                            .recover()
                            .command_failure
                            .store(true, std::sync::atomic::Ordering::Release);
                    } else {
                        match halt_reason {
                            crate::request::request_group::HaltReason::UserRequest => {
                                group.recover().mark_removed();
                            }
                            crate::request::request_group::HaltReason::Timeout => {
                                group.recover().mark_timeout();
                            }
                            crate::request::request_group::HaltReason::ShutdownSignal
                            | crate::request::request_group::HaltReason::None => {}
                        }
                    }
                }
            }
        }
    }
}

/// Run periodic housekeeping tasks.
///
/// Mirrors the C++ refresh-interval-based tasks:
/// - Session auto-save
/// - Socket pool eviction
/// - Server statistics pruning
/// - Prune excess stopped results
async fn run_housekeeping(
    ctx: &EngineLoopContext,
    running_downloads: &mut [(GroupId, RunningDownload)],
) {
    // ── Timeout enforcement ──────────────────────────────────────────────
    // Abort tasks whose per-command timeout has elapsed.
    // Mirrors C++ per-command timeout (though C++ handles this differently
    // via the Command::STATUS_ACTIVE mechanism).
    let now = Instant::now();
    let mut timed_out = Vec::new();
    for (gid, rd) in running_downloads.iter() {
        if let Some(timeout) = rd.timeout
            && now.duration_since(rd.started) >= timeout
        {
            timed_out.push(*gid);
        }
    }

    for gid in timed_out {
        let timeout_context = {
            let man = ctx.group_man.read().await;
            man.get_group(gid).map(|group| {
                (
                    group.recover().connection_contexts(),
                    group.recover().get_all_uris(),
                )
            })
        };

        if let Some((request_contexts, uris)) = timeout_context
            && let Some(uri) = uris.first()
            && let Ok(parsed) = reqwest::Url::parse(uri)
            && let Some(host) = parsed.host_str()
        {
            let protocol = parsed.scheme().to_ascii_lowercase();
            ctx.server_stat_man
                .mark_failure_with_protocol(host, &protocol, 408);
            if !request_contexts.is_empty() {
                let mut dns = ctx.dns_cache.lock().await;
                for context in request_contexts {
                    dns.mark_bad_context(&context);
                    if !dns.has_good_address(&context.endpoint) {
                        dns.remove_cached(context.endpoint.hostname(), context.endpoint.port());
                    }
                }
            }
        }

        let timed_out_now = {
            let man = ctx.group_man.read().await;
            man.timeout_group(gid)
        };
        if timed_out_now {
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

    // ── Session auto-save ────────────────────────────────────────────────
    // While downloads are running, progress changes every tick; keep the
    // session dirty so `save_if_dirty` (gated by `save-session-interval`)
    // actually persists the latest progress.
    if !running_downloads.is_empty() {
        mark_session_dirty(ctx);
    }
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

    // ── Server statistics housekeeping ────────────────────────────────────
    // aria2_original removes statistics older than the configured freshness
    // window from the long-lived ServerStatMan.
    let stale_stats = ctx
        .server_stat_man
        .remove_stale(Duration::from_secs(24 * 60 * 60));
    if stale_stats > 0 {
        debug!("Removed {} stale server statistics", stale_stats);
    }

    // ── Socket pool eviction ─────────────────────────────────────────────
    // reqwest owns the HTTP/TLS pool and enforces its idle timeout internally.
    // The FTP pool is engine-owned, so it must be explicitly swept here to
    // mirror C++ `evictSocketPool()` and release stale FTP sessions.
    let evicted = ctx.ftp_pool.cleanup_stale_count().await;
    if evicted > 0 {
        debug!("Evicted {} stale FTP connections", evicted);
    }
}

async fn request_shutdown_and_wait(running: &mut RunningDownload) -> bool {
    let Some(shutdown_tx) = running.shutdown_tx.take() else {
        return false;
    };
    let _ = shutdown_tx.send(());
    match tokio::time::timeout(SHUTDOWN_WAIT, &mut running._handle).await {
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
    }
}

/// Cleanup on engine exit.
///
/// Mirrors C++ `onEndOfRun()`: remove stopped groups, close files, save.
async fn on_end_of_run(
    ctx: &EngineLoopContext,
    running_downloads: &mut Vec<(GroupId, RunningDownload)>,
) {
    info!("Engine loop cleanup: removing stopped groups and saving state");

    // Cancel any pending file allocations so commands waiting on the
    // completion channel are woken with an error instead of hanging forever.
    // Mirrors C++ where the engine's commands are all dropped at exit.
    crate::filesystem::file_allocation_man::cancel_all(&ctx.file_alloc_man).await;

    // Demote any remaining stopped groups.
    let demoted = {
        let man = ctx.group_man.read().await;
        man.remove_stopped_groups(Some(&ctx.event_hooks))
    };
    if !demoted.is_empty() {
        info!("Demoted {} final groups on shutdown", demoted.len());
    }

    // Request protocol-level shutdown and wait for each task before dropping
    // the engine, bounded so a broken command cannot hang engine teardown.
    for (gid, mut rd) in running_downloads.drain(..) {
        let completed = request_shutdown_and_wait(&mut rd).await;
        debug!(
            gid = gid.value(),
            completed, "Finished running task shutdown"
        );
    }

    // Final session save.
    if let Some(ref auto_save) = ctx.auto_save {
        let mut save = auto_save.lock().await;
        save.force_save().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::{DownloadOptions, DownloadStatus, HaltReason};

    /// Build a context with no downloads queued. `keep_alive` mirrors
    /// `--enable-rpc`, which is where the halt semantics used to break.
    fn test_ctx(keep_alive: bool) -> EngineLoopContext {
        EngineLoopContext {
            group_man: Arc::new(tokio::sync::RwLock::new(RequestGroupMan::new())),
            ftp_pool: Arc::new(FtpConnectionPool::new(1)),
            dns_cache: Arc::new(tokio::sync::Mutex::new(DnsCache::new())),
            auto_save: None,
            event_hooks: Arc::new(DownloadEventHooks::new()),
            file_alloc_man: Arc::new(tokio::sync::RwLock::new(FileAllocationMan::new())),
            keep_alive,
            server_stat_man: ServerStatMan::shared().clone(),
            global_limiter: None,
        }
    }

    /// Drive the loop until it exits, failing the test if it outlives
    /// `budget`. Guards the exact regression this suite exists for: a halt
    /// that never converges would otherwise hang CI instead of failing.
    async fn run_until_exit(
        ctx: EngineLoopContext,
        cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
        budget: Duration,
    ) {
        let loop_fut = run_engine_loop(ctx, cmd_rx, shutdown_rx, Duration::from_millis(5));
        tokio::time::timeout(budget, loop_fut)
            .await
            .expect("engine loop failed to terminate after halt");
    }

    #[tokio::test]
    async fn graceful_halt_exits_even_in_keep_alive_mode() {
        // Regression: `halt_requested` used to be write-only, so the exit
        // condition `(all_done && !keep_alive) || force_halt` could never fire
        // under `--enable-rpc` and `aria2.shutdown` hung forever.
        let (tx, rx) = mpsc::unbounded_channel();
        let (_sd_tx, sd_rx) = tokio::sync::oneshot::channel();

        tx.send(EngineCommand::HaltAll {
            reason: HaltReason::ShutdownSignal,
        })
        .unwrap();

        run_until_exit(test_ctx(true), rx, sd_rx, Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn force_halt_exits_in_keep_alive_mode() {
        let (tx, rx) = mpsc::unbounded_channel();
        let (_sd_tx, sd_rx) = tokio::sync::oneshot::channel();

        tx.send(EngineCommand::ForceHaltAll {
            reason: HaltReason::ShutdownSignal,
        })
        .unwrap();

        run_until_exit(test_ctx(true), rx, sd_rx, Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn shutdown_signal_exits_in_keep_alive_mode() {
        // The Ctrl+C path sets `halt_requested` directly rather than going
        // through an EngineCommand, so it needs its own coverage.
        let (_tx, rx) = mpsc::unbounded_channel();
        let (sd_tx, sd_rx) = tokio::sync::oneshot::channel();

        sd_tx.send(()).unwrap();

        run_until_exit(test_ctx(true), rx, sd_rx, Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn keep_alive_without_halt_does_not_exit() {
        // The flip side: keep-alive must still hold the loop open when no halt
        // was requested, otherwise an idle RPC server would shut itself down.
        let (_tx, rx) = mpsc::unbounded_channel();
        let (_sd_tx, sd_rx) = tokio::sync::oneshot::channel();

        let loop_fut = run_engine_loop(test_ctx(true), rx, sd_rx, Duration::from_millis(5));
        assert!(
            tokio::time::timeout(Duration::from_millis(200), loop_fut)
                .await
                .is_err(),
            "keep-alive loop exited without a halt request"
        );
    }

    #[tokio::test]
    async fn idle_loop_exits_without_keep_alive() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let (_sd_tx, sd_rx) = tokio::sync::oneshot::channel();

        run_until_exit(test_ctx(false), rx, sd_rx, Duration::from_secs(5)).await;
    }

    /// Regression for the periodic auto-save being dead: `mark_session_dirty`
    /// had no callers, so the dirty flag stayed `false` and `save_if_dirty`
    /// never wrote the session file. This test drives a state-changing
    /// `EngineCommand` through `process_engine_commands` and verifies the
    /// session is marked dirty AND that `save_if_dirty` then writes the file.
    #[tokio::test]
    async fn state_changing_command_marks_dirty_and_persists() {
        use crate::request::request_group::{GroupId, RequestGroup};
        use crate::session::auto_save_session::AutoSaveSession;

        let man = Arc::new(tokio::sync::RwLock::new(RequestGroupMan::new()));
        let dir = std::env::temp_dir();
        let path = dir.join(format!("test_engine_autosave_{}.sess", std::process::id()));
        let _ = tokio::fs::remove_file(&path).await;

        let auto_save = Arc::new(tokio::sync::Mutex::new(AutoSaveSession::new(
            path.clone(),
            Duration::from_millis(0),
            man.clone(),
        )));

        // The auto-save must share the SAME group manager that the engine
        // commands mutate, otherwise it serializes a stale/empty snapshot.
        let mut ctx = EngineLoopContext {
            group_man: man,
            ftp_pool: Arc::new(FtpConnectionPool::new(1)),
            dns_cache: Arc::new(tokio::sync::Mutex::new(DnsCache::new())),
            auto_save: Some(auto_save.clone()),
            event_hooks: Arc::new(DownloadEventHooks::new()),
            file_alloc_man: Arc::new(tokio::sync::RwLock::new(FileAllocationMan::new())),
            keep_alive: false,
            server_stat_man: Arc::new(ServerStatMan::new()),
            global_limiter: None,
        };

        // Send an AddDownload command through the engine-command channel.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(42),
            vec!["http://example.com/engine-autosave.bin".to_string()],
            DownloadOptions::default(),
        )));
        tx.send(EngineCommand::AddDownload { group }).unwrap();

        let mut halt_requested = false;
        let mut force_halt_requested = false;
        let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
        process_engine_commands(
            &mut ctx,
            &mut rx,
            &mut Vec::new(),
            &mut halt_requested,
            &mut force_halt_requested,
            &completion_tx,
        )
        .await;

        // The AddDownload command must have flipped the dirty flag.
        assert!(
            auto_save.lock().await.is_dirty(),
            "AddDownload should mark the session dirty"
        );

        // With interval=0 and dirty=true, save_if_dirty() writes the file.
        auto_save.lock().await.save_if_dirty().await;
        assert!(path.exists(), "save_if_dirty should write the session file");
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            content.contains("http://example.com/engine-autosave.bin"),
            "session file should contain the added URI"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    // ── Pause semantics ─────────────────────────────────────────────────
    // Regression: a paused download's command terminates with
    // TaskResult::Failed("Download paused") (the download loop observes the
    // Paused status via check_cancelled). Before the fix the engine recorded
    // an Error, which is terminal and can never be unpaused.

    #[tokio::test]
    async fn global_rate_limit_command_updates_shared_limiter() {
        let mut ctx = test_ctx(false);
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(EngineCommand::SetGlobalRateLimit {
            download_limit: Some(2_000),
            upload_limit: Some(1_000),
        })
        .unwrap();

        let (completion_tx, _completion_rx) =
            mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
        let mut running_downloads = Vec::new();
        process_engine_commands(
            &mut ctx,
            &mut rx,
            &mut running_downloads,
            &mut false,
            &mut false,
            &completion_tx,
        )
        .await;

        let limiter = ctx
            .global_limiter
            .as_ref()
            .expect("runtime updates create the shared limiter")
            .clone();
        let config = limiter.config().await;
        assert_eq!(config.download_rate(), Some(2_000));
        assert_eq!(config.upload_rate(), Some(1_000));

        tx.send(EngineCommand::SetGlobalRateLimit {
            download_limit: None,
            upload_limit: None,
        })
        .unwrap();
        process_engine_commands(
            &mut ctx,
            &mut rx,
            &mut running_downloads,
            &mut false,
            &mut false,
            &completion_tx,
        )
        .await;
        let config = limiter.config().await;
        assert_eq!(config.download_rate(), None);
        assert_eq!(config.upload_rate(), None);

        let man = ctx.group_man.read().await;
        assert_eq!(man.global_download_limit(), None);
        assert_eq!(man.global_upload_limit(), None);
    }

    #[tokio::test]
    async fn paused_task_failure_keeps_group_paused() {
        let ctx = test_ctx(false);

        let gid = {
            let man = ctx.group_man.read().await;
            man.add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap()
        };

        // Promote to active (fill_from_reserver calls start() → Active).
        {
            let man = ctx.group_man.read().await;
            let promoted = man.fill_from_reserver();
            assert_eq!(promoted.len(), 1);
            assert!(man.find_group(gid).is_some());
        }

        // aria2.pause marks the group Paused.
        {
            let man = ctx.group_man.read().await;
            man.pause_group(gid).unwrap();
            let g = man.find_group(gid).unwrap();
            assert!(g.recover().status().is_paused());
            // Simulate a spawned task that has not yet reported completion.
            g.recover().inc_commands();
        }

        // The download command terminates because it observed the pause.
        let (completion_tx, mut completion_rx) =
            mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
        completion_tx
            .send((
                gid,
                1,
                TaskResult::Failed(Aria2Error::DownloadFailed("Download paused".into())),
            ))
            .unwrap();

        let mut running_downloads = Vec::new();
        let mut completed_generations = HashSet::new();
        process_task_completions(
            &ctx,
            &mut completion_rx,
            &mut running_downloads,
            &mut completed_generations,
        )
        .await;

        let status = {
            let man = ctx.group_man.read().await;
            man.find_group(gid).unwrap().recover().status()
        };
        assert_eq!(
            status,
            DownloadStatus::Paused,
            "a pause-induced task failure must keep the group Paused (resumable), not Error"
        );
    }

    #[tokio::test]
    async fn duplicate_completion_decrements_command_once() {
        let ctx = test_ctx(false);
        let gid = {
            let man = ctx.group_man.read().await;
            let gid = man
                .add_group(
                    vec!["http://example.com/file.bin".to_string()],
                    DownloadOptions::default(),
                )
                .unwrap();
            man.fill_from_reserver();
            man.find_group(gid).unwrap().recover().inc_commands();
            gid
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
        tx.send((
            gid,
            1,
            TaskResult::Failed(Aria2Error::Network("failed".into())),
        ))
        .unwrap();
        tx.send((
            gid,
            1,
            TaskResult::Failed(Aria2Error::Network("duplicate".into())),
        ))
        .unwrap();

        let mut running_downloads: Vec<(GroupId, RunningDownload)> = Vec::new();
        let mut completed_generations: HashSet<CommandGeneration> = HashSet::new();
        process_task_completions(
            &ctx,
            &mut rx,
            &mut running_downloads,
            &mut completed_generations,
        )
        .await;

        let man = ctx.group_man.read().await;
        let group = man.find_group(gid).unwrap();
        assert_eq!(group.recover().num_commands(), 0);
        assert_eq!(completed_generations.len(), 1);
    }

    #[tokio::test]
    async fn same_gid_commands_have_independent_completion_generations() {
        let ctx = test_ctx(false);
        let gid = {
            let man = ctx.group_man.read().await;
            let gid = man
                .add_group(
                    vec!["http://example.com/file.bin".to_string()],
                    DownloadOptions::default(),
                )
                .unwrap();
            man.fill_from_reserver();
            let group = man.find_group(gid).unwrap();
            group.recover().inc_commands();
            group.recover().inc_commands();
            gid
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
        tx.send((
            gid,
            1,
            TaskResult::Failed(Aria2Error::Network("first command".into())),
        ))
        .unwrap();
        tx.send((
            gid,
            2,
            TaskResult::Failed(Aria2Error::Network("second command".into())),
        ))
        .unwrap();

        let mut running_downloads = Vec::new();
        let mut completed_generations = HashSet::new();
        process_task_completions(
            &ctx,
            &mut rx,
            &mut running_downloads,
            &mut completed_generations,
        )
        .await;

        let man = ctx.group_man.read().await;
        assert_eq!(man.find_group(gid).unwrap().recover().num_commands(), 0);
        assert_eq!(completed_generations.len(), 2);
    }

    #[tokio::test]
    async fn non_final_command_failure_waits_for_final_completion() {
        let ctx = test_ctx(false);
        let gid = {
            let man = ctx.group_man.read().await;
            let gid = man
                .add_group(
                    vec!["http://example.com/file.bin".to_string()],
                    DownloadOptions::default(),
                )
                .unwrap();
            man.fill_from_reserver();
            let group = man.find_group(gid).unwrap();
            group.recover().inc_commands();
            group.recover().inc_commands();
            gid
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
        tx.send((
            gid,
            1,
            TaskResult::Failed(Aria2Error::Network("first command failed".into())),
        ))
        .unwrap();

        let mut running_downloads = Vec::new();
        let mut completed_generations = HashSet::new();
        process_task_completions(
            &ctx,
            &mut rx,
            &mut running_downloads,
            &mut completed_generations,
        )
        .await;

        {
            let man = ctx.group_man.read().await;
            let group = man.find_group(gid).unwrap();
            assert_eq!(group.recover().num_commands(), 1);
            assert!(matches!(group.recover().status(), DownloadStatus::Active));
        }

        tx.send((gid, 2, TaskResult::Success)).unwrap();
        process_task_completions(
            &ctx,
            &mut rx,
            &mut running_downloads,
            &mut completed_generations,
        )
        .await;

        let man = ctx.group_man.read().await;
        let group = man.find_group(gid).unwrap();
        assert_eq!(group.recover().num_commands(), 0);
        assert!(matches!(group.recover().status(), DownloadStatus::Error(_)));
    }

    #[test]
    fn error_code_mapping_preserves_aria2_semantics() {
        assert_eq!(
            map_error_code(&Aria2Error::Recoverable(RecoverableError::Timeout)),
            DownloadResultCode::TimeOut
        );
        assert_eq!(
            map_error_code(&Aria2Error::Recoverable(RecoverableError::CannotResume)),
            DownloadResultCode::CannotResume
        );
        assert_eq!(
            map_error_code(&Aria2Error::Recoverable(RecoverableError::ServerError {
                code: 404
            })),
            DownloadResultCode::ResourceNotFound
        );
        for code in [502, 503, 504] {
            assert_eq!(
                map_error_code(&Aria2Error::Recoverable(RecoverableError::ServerError {
                    code
                })),
                DownloadResultCode::HttpServiceUnavailable
            );
        }
        for code in [401, 407] {
            assert_eq!(
                map_error_code(&Aria2Error::Recoverable(RecoverableError::ServerError {
                    code
                })),
                DownloadResultCode::HttpAuthFailed
            );
        }
        assert_eq!(
            map_error_code(&Aria2Error::Recoverable(
                RecoverableError::HttpTooManyRedirects { count: 20 }
            )),
            DownloadResultCode::HttpTooManyRedirects
        );
        assert_eq!(
            map_error_code(&Aria2Error::Checksum("bad digest".into())),
            DownloadResultCode::ChecksumError
        );
        assert_eq!(
            map_error_code(&Aria2Error::Recoverable(
                RecoverableError::FtpProtocolError {
                    message: "550".into(),
                }
            )),
            DownloadResultCode::FtpProtocolError
        );
        assert_eq!(
            map_error_code(&Aria2Error::Io("disk write".into())),
            DownloadResultCode::FileIoError
        );
        assert_eq!(
            map_error_code(&Aria2Error::Fatal(
                crate::error::FatalError::DiskSpaceExhausted
            )),
            DownloadResultCode::NotEnoughDiskSpace
        );
        assert_eq!(
            map_error_code(&Aria2Error::HttpProtocol("bad status".into())),
            DownloadResultCode::HttpProtocolError
        );
        assert_eq!(
            map_error_code(&Aria2Error::FtpProtocol("bad PASV".into())),
            DownloadResultCode::FtpProtocolError
        );
        assert_eq!(
            map_error_code(&Aria2Error::DirCreate("permission denied".into())),
            DownloadResultCode::DirCreateError
        );
        assert_eq!(
            map_error_code(&Aria2Error::FileOpen("cannot open".into())),
            DownloadResultCode::FileOpenError
        );
    }

    #[tokio::test]
    async fn genuine_task_failure_still_marks_error() {
        let ctx = test_ctx(false);

        let gid = {
            let man = ctx.group_man.read().await;
            man.add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap()
        };
        {
            let man = ctx.group_man.read().await;
            let promoted = man.fill_from_reserver();
            assert_eq!(promoted.len(), 1);
            let g = man.find_group(gid).unwrap();
            g.recover().inc_commands();
        }

        let (completion_tx, mut completion_rx) =
            mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();
        completion_tx
            .send((
                gid,
                1,
                TaskResult::Failed(Aria2Error::Network("connection refused".into())),
            ))
            .unwrap();

        let mut running_downloads = Vec::new();
        let mut completed_generations = HashSet::new();
        process_task_completions(
            &ctx,
            &mut completion_rx,
            &mut running_downloads,
            &mut completed_generations,
        )
        .await;

        let status = {
            let man = ctx.group_man.read().await;
            man.find_group(gid).unwrap().recover().status()
        };
        assert!(
            matches!(status, DownloadStatus::Error(_)),
            "a genuine network failure must still record an Error, got {:?}",
            status
        );
    }
}

//! Engine main loop: promotion/demotion, EngineCommand dispatch, and
//! deadline-driven maintenance.
//!
//! Mirrors the C++ `DownloadEngine::run()` loop structure. Each pass:
//! 1. Process incoming `EngineCommand`s (add/remove/pause/unpause/halt etc.)
//! 2. Collect completed task notifications and decrement `num_commands`
//! 3. Demote stopped groups from active to stopped results
//! 4. Promote reserved groups and spawn download tasks via `task_spawner`
//! 5. Run deadline-driven maintenance (timeouts and session auto-save)
//! 6. Check exit condition

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::download_event_hooks::{DownloadEvent, DownloadEventHooks};
use super::engine_command::{
    EngineCommand, EngineCommandReceiver, EngineCommandTryRecvError, TaskResult,
};
use super::task_spawner::{CommandDependencies, spawn_download_task};
use crate::dns::dns_cache::DnsCache;
use crate::error::{Aria2Error, RecoverableError};
use crate::filesystem::file_allocation_man::FileAllocationMan;
use crate::network::ConnectionContext;
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadResultCode, DownloadStatus, GroupId, HaltReason};
use crate::request::request_group_man::RequestGroupMan;
use crate::selector::server_stat_man::ServerStatMan;
use crate::session::auto_save_coordinator::AutoSaveCoordinator;
use crate::util::rwlock_ext::RwLockRecover;

/// Maximum number of stopped results to keep before pruning.
/// Mirrors C++ `MAX_DOWNLOAD_RESULT` (default 1000).
const MAX_STOPPED_RESULTS: usize = 1000;
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
// forceShutdown is an emergency path. Give protocol cleanup a short bounded
// window, then abort the task so process shutdown cannot be held open by a
// broken or unresponsive protocol implementation.
const FORCE_SHUTDOWN_WAIT: Duration = Duration::from_secs(1);

fn should_mark_failed_connection(error: &Aria2Error) -> bool {
    matches!(
        error,
        Aria2Error::Network(_)
            | Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure { .. } | RecoverableError::Timeout
            )
    )
}

async fn mark_failed_connection(
    dns_cache: &tokio::sync::Mutex<DnsCache>,
    error: &Aria2Error,
    context: &ConnectionContext,
    use_async_dns: bool,
) {
    if !use_async_dns || !should_mark_failed_connection(error) {
        return;
    }

    let mut dns = dns_cache.lock().await;
    dns.mark_bad_context(context);
    if !dns.has_good_address(&context.endpoint) {
        dns.remove_cached(context.endpoint.hostname(), context.endpoint.port());
    }
}

/// Context passed into the engine loop, holding shared state that the
/// loop needs to coordinate between EngineCommand processing, promotion,
/// demotion, and deadline-driven maintenance.
pub struct EngineLoopContext {
    /// The request group manager (active/reserved/stopped queues).
    pub group_man: Arc<RequestGroupMan>,

    /// DNS cache for dependency injection.
    pub dns_cache: Arc<tokio::sync::Mutex<DnsCache>>,

    /// Unified deadline-driven coordinator for session and control-file saves.
    pub auto_save: Option<Arc<tokio::sync::Mutex<AutoSaveCoordinator>>>,

    /// Lock-free session dirty signal used when `auto_save` is busy writing.
    pub auto_save_dirty_signal: Option<Arc<std::sync::atomic::AtomicBool>>,

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

    /// Maximum age for server-stat cleanup. `None` means unlimited.
    pub server_stat_max_age: Option<Duration>,

    /// Optional server-stat output and its deadline-driven save state.
    pub server_stat_save_path: Option<PathBuf>,
    pub server_stat_save_interval: Option<Duration>,
    pub server_stat_next_save: Option<Instant>,

    /// Process-wide rate limiter shared across all downloads.
    /// When `Some`, passed to each spawned `DownloadCommand` so that
    /// `ThrottledWriter` and segment download loops enforce a global
    /// bandwidth ceiling in addition to per-download limits.
    pub global_limiter: Option<RateLimiter>,

    /// Process-wide public tracker catalog shared by BT commands.
    #[cfg(feature = "bittorrent")]
    pub public_tracker_catalog:
        Arc<aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList>,

    /// Engine-owned registry shared by all BitTorrent commands.
    #[cfg(feature = "bittorrent")]
    pub bt_registry: Arc<std::sync::RwLock<crate::engine::bt_registry::BtRegistry>>,

    /// Process-level BitTorrent TCP listener and info-hash router.
    #[cfg(feature = "bittorrent")]
    pub bt_listener: Arc<crate::engine::bt_peer_listener::BtPeerListenerManager>,

    /// Process-level Local Peer Discovery manager and receive loop.
    #[cfg(feature = "bittorrent")]
    pub lpd_manager: Arc<crate::engine::lpd_manager::LpdManager>,
}

/// Tracks a spawned download task for timeout enforcement and cleanup.
type CommandGeneration = u64;

struct RunningDownload {
    /// JoinHandle for the spawned tokio task.
    _handle: JoinHandle<()>,
    shutdown: Option<CancellationToken>,
    /// Stable identity of this command instance, independent of its GID.
    generation: CommandGeneration,
    /// Instant at which the last network payload was received.
    last_activity: Instant,
    /// Inactivity timeout. `None` means the task never times out.
    timeout: Option<Duration>,
}

/// Mark the auto-save session as dirty so the next configured save deadline
/// (subject to `save-session-interval`) actually persists state.
///
/// C++ aria2's `AutoSaveCommand` unconditionally saves every interval; our
/// `AutoSaveSession` adds a dirty gate to avoid redundant disk writes. Every
/// caller that mutates download state (queue membership, status, options,
/// progress) must flip this flag or `save_if_dirty()` never writes.
fn mark_session_dirty(ctx: &EngineLoopContext) {
    if let Some(signal) = &ctx.auto_save_dirty_signal {
        signal.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Run the main engine loop.
///
/// This function runs until:
/// - No active/reserved downloads remain AND `keep_alive` is false, OR
/// - A shutdown signal is received via `shutdown_rx`.
///
/// The loop processes `EngineCommand`s from `cmd_rx`, task completion
/// notifications from `completion_rx`, and runs deadline-driven maintenance.
pub async fn run_engine_loop(
    ctx: EngineLoopContext,
    cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    run_engine_loop_with_receiver(
        ctx,
        EngineCommandReceiver::from_unbounded(cmd_rx),
        shutdown_rx,
    )
    .await;
}

pub(crate) async fn run_engine_loop_with_receiver(
    mut ctx: EngineLoopContext,
    mut cmd_rx: EngineCommandReceiver,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    info!("Engine loop started (event-driven dispatch)");

    let mut running_downloads: Vec<(GroupId, RunningDownload)> = Vec::new();
    let mut completed_generations: HashSet<CommandGeneration> = HashSet::new();
    let mut next_generation: CommandGeneration = 1;
    let mut halt_requested = false;
    let mut force_halt_requested = false;
    let mut shutdown_received = false;
    let mut command_closed = false;
    let mut completion_closed = false;
    let mut first_pass = true;
    let mut schedule_on_next_pass = false;

    // Completion channel: spawned tasks send (GID, TaskResult) here when done.
    let (completion_tx, mut completion_rx) =
        mpsc::unbounded_channel::<(GroupId, CommandGeneration, TaskResult)>();

    loop {
        // ── 1. Process all incoming EngineCommands ───────────────────────
        // Drain the command channel before doing anything else, so that
        // batch RPC requests (e.g. addUri followed by unpause) are applied
        // atomically within the same event-processing pass.
        let commands_processed = process_engine_commands(
            &mut ctx,
            &mut cmd_rx,
            &mut running_downloads,
            &mut halt_requested,
            &mut force_halt_requested,
            &completion_tx,
        )
        .await;

        // Promote groups that were already queued before the engine started.
        // Later passes promote after completion/demotion so a requeued group
        // is scheduled without relying on a fixed-rate wake-up.
        if first_pass {
            promote_reserved_groups(
                &ctx,
                &mut running_downloads,
                halt_requested,
                force_halt_requested,
                &mut next_generation,
                &completion_tx,
            );
        }

        // ── 2. Collect completed task notifications ──────────────────────
        // Process all pending task completion messages.
        let completions_processed = process_task_completions(
            &ctx,
            &mut completion_rx,
            &mut running_downloads,
            &mut completed_generations,
        )
        .await;

        // ── 3. Demote stopped groups (active → stopped results) ──────────
        // Mirrors C++ `removeStoppedGroup()`.
        let demoted_gids = { ctx.group_man.remove_stopped_groups(Some(&ctx.event_hooks)) };

        if !demoted_gids.is_empty() {
            debug!("Demoted {} groups to stopped", demoted_gids.len());
            // Demotion moved groups to stopped results: persist the change.
            mark_session_dirty(&ctx);
            run_event_cleanup(&ctx).await;
        }

        // ── 4. Promote groups made runnable by this pass or the preceding
        // event wake-up. ──────────────────────────────────────────────────
        // A group requeued from a completion must be promoted before the
        // engine parks. The first-pass orphan requeue is deliberately left
        // for the next external event so an already-pending shutdown cannot
        // turn it into new work.
        let needs_follow_up_promotion = (!first_pass && commands_processed)
            || completions_processed
            || !demoted_gids.is_empty()
            || schedule_on_next_pass;
        if needs_follow_up_promotion {
            promote_reserved_groups(
                &ctx,
                &mut running_downloads,
                halt_requested,
                force_halt_requested,
                &mut next_generation,
                &completion_tx,
            );
        }

        first_pass = false;
        schedule_on_next_pass = false;

        // ── 5. Check exit condition ──────────────────────────────────────
        let all_done = ctx.group_man.download_finished() && running_downloads.is_empty();

        // A graceful halt must wind the engine down even in keep-alive (RPC)
        // mode. C++ achieves this because every routine command (RPC
        // listener, fill-request-group, …) returns `true` — removing itself
        // from `commands_` — once `isHaltRequested()`, so `run()`'s
        // `while (!commands_.empty())` terminates. Without this branch
        // `aria2.shutdown` would hang forever whenever `--enable-rpc` is set,
        // since `all_done && !keep_alive` can never be true there.
        let graceful_done = halt_requested && running_downloads.is_empty();

        let force_done =
            force_halt_requested && running_downloads.is_empty() && completion_rx.is_empty();
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

        // ── 6. Wait for a command, task completion, a real maintenance
        // deadline,
        // or shutdown signal. The engine stays parked while idle.
        let maintenance_wait =
            wait_for_deadline(next_maintenance_deadline(&ctx, &running_downloads).await);
        tokio::pin!(maintenance_wait);
        tokio::select! {
            command = cmd_rx.recv(), if !command_closed => {
                match command {
                    Ok(command) => {
                        let mut prefetched = PrefetchedEngineCommand {
                            first: Some(command),
                            receiver: &mut cmd_rx,
                        };
                        schedule_on_next_pass |= process_engine_commands(
                            &mut ctx,
                            &mut prefetched,
                            &mut running_downloads,
                            &mut halt_requested,
                            &mut force_halt_requested,
                            &completion_tx,
                        )
                        .await;
                    }
                    Err(EngineCommandTryRecvError::Closed) => command_closed = true,
                    Err(EngineCommandTryRecvError::Empty) => unreachable!(
                        "async engine command receive cannot return empty"
                    ),
                }
            }
            completion = completion_rx.recv(), if !completion_closed => {
                match completion {
                    Some(completion) => {
                        let mut prefetched = PrefetchedCompletion {
                            first: Some(completion),
                            receiver: &mut completion_rx,
                        };
                        schedule_on_next_pass |= process_task_completions(
                            &ctx,
                            &mut prefetched,
                            &mut running_downloads,
                            &mut completed_generations,
                        )
                        .await;
                    }
                    None => completion_closed = true,
                }
            }
            _ = &mut maintenance_wait => {
                run_deadline_maintenance(&mut ctx, &mut running_downloads).await;
            }
            Ok(_) = &mut shutdown_rx, if !shutdown_received => {
                shutdown_received = true;
                info!("Shutdown signal received");
                // Process graceful halt
                ctx.group_man
                    .halt_all(crate::request::request_group::HaltReason::ShutdownSignal);
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

mod commands;
mod completions;
mod maintenance;

use commands::{PrefetchedEngineCommand, process_engine_commands, promote_reserved_groups};
use completions::{PrefetchedCompletion, process_task_completions};
use maintenance::{
    cancel_running_file_allocations, next_maintenance_deadline, on_end_of_run,
    request_shutdown_and_wait, run_deadline_maintenance, run_event_cleanup, wait_for_deadline,
};

#[cfg(test)]
#[path = "../engine_loop_tests/mod.rs"]
mod tests;

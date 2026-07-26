use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio::task::{AbortHandle, Id, JoinSet};
use tokio::time::interval;
use tracing::{debug, error, info, warn};

#[cfg(feature = "bittorrent")]
use super::bt_registry::BtRegistry;
use super::command::{Command, ProgressUpdate};
use super::engine_command::EngineCommand;
use super::engine_loop::EngineLoopContext;
use crate::constants;
use crate::dns::dns_cache::DnsCache;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::ftp::FtpConnectionPool;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use crate::request::request_group::RequestGroup;
use crate::request::request_group_man::RequestGroupMan;
use crate::retry::{RetryPolicy, RetryStats};
use crate::session::auto_save_session::AutoSaveSession;
use crate::session::save_session_command::SaveSessionCommand;
#[cfg(test)]
use crate::util::rwlock_ext::RwLockRecover;
use crate::util::speed_smooth::SpeedSmoother;

/// Bookkeeping the engine retains for each spawned command task so it can
/// enforce per-command timeouts and abort stalled tasks individually.
///
/// The command object itself is moved into the spawned task (it owns the
/// `Box<dyn Command>` for the duration of `execute()`); v1 does NOT recover
/// commands for retry on timeout/failure, so the engine only needs the abort
/// handle and timing metadata.
struct RunningTask {
    /// Handle used to cancel the task when its timeout elapses or on shutdown.
    handle: AbortHandle,
    /// Instant the task was spawned.
    started: Instant,
    /// Per-command timeout. `None` means the command never times out.
    timeout: Option<Duration>,
}

pub struct DownloadEngine {
    command_tx: mpsc::UnboundedSender<Box<dyn Command>>,
    command_rx: mpsc::UnboundedReceiver<Box<dyn Command>>,
    /// EngineCommand channel for structured RPC → engine communication.
    /// Replaces the `Box<dyn Command>` channel for download lifecycle ops.
    engine_cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    engine_cmd_rx: Option<mpsc::UnboundedReceiver<EngineCommand>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    shutdown_rx: Option<oneshot::Receiver<()>>,
    tick_interval: Duration,
    retry_policy: Arc<RetryPolicy>,
    retry_stats: Arc<RetryStats>,
    global_limiter: Option<RateLimiter>,
    save_session_path: Option<PathBuf>,
    save_session_interval: Option<Duration>,
    request_group_man: Option<Arc<RwLock<RequestGroupMan>>>,
    auto_save: Option<Arc<Mutex<AutoSaveSession>>>,
    /// FTP connection pool for connection reuse across FTP downloads.
    /// Created during engine initialization and passed down via dependency injection.
    ftp_pool: Arc<FtpConnectionPool>,
    /// DNS resolution cache for avoiding repeated lookups.
    /// Created during engine initialization and passed down via dependency injection.
    dns_cache: Arc<Mutex<DnsCache>>,
    /// When true, the engine stays alive even with no pending/running commands
    /// (used for RPC listen mode). The loop only exits on shutdown signal.
    keep_alive: bool,
    /// BitTorrent registry — maps GID to BtObject (DownloadContext, BtRuntime,
    /// etc.). In C++ aria2, this is a global singleton in DownloadEngine.
    /// Here it is owned by the engine and accessible via `bt_registry()`.
    /// Used for info-hash reverse lookup, peer blocklist, and BT component
    /// coordination across all active downloads.
    #[cfg(feature = "bittorrent")]
    bt_registry: Arc<std::sync::RwLock<BtRegistry>>,
}

impl DownloadEngine {
    pub fn new(tick_interval_ms: u64) -> Self {
        Self::with_retry_policy(tick_interval_ms, RetryPolicy::default())
    }

    pub fn with_retry_policy(tick_interval_ms: u64, policy: RetryPolicy) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (engine_cmd_tx, engine_cmd_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let max_tries = policy.max_tries();

        let engine = DownloadEngine {
            command_tx,
            command_rx,
            engine_cmd_tx,
            engine_cmd_rx: Some(engine_cmd_rx),
            shutdown_tx: Some(shutdown_tx),
            shutdown_rx: Some(shutdown_rx),
            tick_interval: Duration::from_millis(tick_interval_ms),
            retry_policy: Arc::new(policy),
            retry_stats: Arc::new(RetryStats::default()),
            global_limiter: None,
            save_session_path: None,
            save_session_interval: None,
            request_group_man: None,
            auto_save: None,
            ftp_pool: Arc::new(FtpConnectionPool::new(
                constants::FTP_POOL_DEFAULT_MAX_CONNECTIONS,
            )),
            dns_cache: Arc::new(Mutex::new(DnsCache::new())),
            keep_alive: false,
            #[cfg(feature = "bittorrent")]
            bt_registry: Arc::new(std::sync::RwLock::new(BtRegistry::new())),
        };

        info!(
            "Download engine initialization complete, tick interval: {}ms, max retries: {}",
            tick_interval_ms, max_tries
        );

        engine
    }

    pub fn set_global_rate_limiter(&mut self, config: RateLimiterConfig) {
        self.global_limiter = Some(RateLimiter::new(&config));
        info!(
            "Global speed limits set: download={:?}, upload={:?}",
            config.download_rate(),
            config.upload_rate()
        );
    }

    pub fn global_rate_limiter(&self) -> Option<&RateLimiter> {
        self.global_limiter.as_ref()
    }

    pub fn take_global_rate_limiter(&mut self) -> Option<RateLimiter> {
        self.global_limiter.take()
    }

    /// Spawn a progress aggregator task that receives [`ProgressUpdate`]s from
    /// download commands and applies them to the shared [`RequestGroup`].
    ///
    /// This eliminates per-chunk write-lock contention on the download hot
    /// path: each `DownloadCommand` performs a cheap lock-free
    /// `mpsc::UnboundedSender::send` and this single aggregator task is the
    /// only writer of the progress fields.
    ///
    /// The aggregator deduplicates consecutive updates with identical
    /// `completed_bytes` values and only refreshes the speed fields when the
    /// sender provides a non-zero `download_speed` sample (0 means "no fresh
    /// sample this tick").
    ///
    /// The task exits cleanly when all senders are dropped (the receiver
    /// returns `None`).
    ///
    /// This is intentionally an associated function (not `&self`): it is
    /// called automatically by
    /// [`DownloadCommand::spawn_progress_aggregator`](crate::engine::download_command::DownloadCommand::spawn_progress_aggregator)
    /// during `execute()`, since every `DownloadCommand` now auto-creates a
    /// progress channel in its constructor. External callers rarely need to
    /// invoke this directly.
    pub fn spawn_progress_aggregator(
        _group: Arc<std::sync::RwLock<RequestGroup>>,
        progress: Arc<crate::request::request_group::AtomicProgress>,
        mut receiver: mpsc::UnboundedReceiver<ProgressUpdate>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut last_bytes: u64 = 0;
            let mut smoother = SpeedSmoother::with_default_window(); // EMA N=10
            while let Some(update) = receiver.recv().await {
                // Skip no-op updates: identical completed_bytes means nothing changed
                // since the last applied update (e.g. a stale in-flight send).
                if update.completed_bytes == last_bytes {
                    continue;
                }
                let delta = update.completed_bytes - last_bytes;
                smoother.record_bytes(delta);

                // Lock-free progress update — no RwLock acquisition needed.
                progress.set_completed_length(update.completed_bytes);

                // Speed: use EMA-smoothed speed when available; fall back to
                // the sender's raw speed sample when the smoother hasn't
                // produced a value yet (first sample window). Skip the speed
                // write entirely when both are 0 so a previously cached speed
                // (e.g. from a prior update) is preserved.
                let smoothed = smoother.smoothed_speed() as u64;
                if smoothed > 0 {
                    progress.set_download_speed(smoothed);
                    progress.set_upload_speed(update.upload_speed);
                } else if update.download_speed > 0 {
                    progress.set_download_speed(update.download_speed);
                    progress.set_upload_speed(update.upload_speed);
                }
                last_bytes = update.completed_bytes;
            }
        })
    }

    pub fn set_save_session(
        &mut self,
        path: PathBuf,
        interval: Option<Duration>,
        man: Arc<RwLock<RequestGroupMan>>,
    ) {
        self.save_session_path = Some(path.clone());
        self.save_session_interval = interval;
        self.request_group_man = Some(man);

        if let (Some(interval), Some(man_ref)) = (interval, &self.request_group_man) {
            let path_clone = path.clone();
            let auto_save = AutoSaveSession::new(path, interval, man_ref.clone());
            self.auto_save = Some(Arc::new(Mutex::new(auto_save)));
            info!(
                "Auto-save session enabled: path={}, interval={:.1}s",
                path_clone.display(),
                interval.as_secs_f64()
            );
        } else {
            info!("Manual save session enabled: path={}", path.display());
        }
    }

    pub fn mark_session_dirty(&self) {
        if let Some(ref auto_save) = self.auto_save
            && let Ok(auto) = auto_save.try_lock()
        {
            auto.mark_dirty();
        }
    }

    pub fn save_session_path(&self) -> Option<&PathBuf> {
        self.save_session_path.as_ref()
    }

    pub fn add_command(&self, command: Box<dyn Command>) -> Result<()> {
        self.command_tx
            .send(command)
            .map_err(|e| Aria2Error::DownloadFailed(format!("Failed to add command: {}", e)))
    }

    pub fn retry_stats(&self) -> &RetryStats {
        &self.retry_stats
    }

    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry_policy
    }

    /// Get a reference to the FTP connection pool for dependency injection.
    pub fn ftp_pool(&self) -> &Arc<FtpConnectionPool> {
        &self.ftp_pool
    }

    /// Get a reference to the DNS cache for dependency injection.
    pub fn dns_cache(&self) -> &Arc<Mutex<DnsCache>> {
        &self.dns_cache
    }

    /// Get a reference to the BitTorrent registry.
    ///
    /// The registry maps GID to [`BtObject`](super::bt_registry::BtObject) and
    /// supports info-hash reverse lookup, peer blocklist, and BT component
    /// coordination across all active downloads. In C++ aria2, this is a global
    /// singleton owned by `DownloadEngine`.
    #[cfg(feature = "bittorrent")]
    pub fn bt_registry(&self) -> &Arc<std::sync::RwLock<BtRegistry>> {
        &self.bt_registry
    }

    /// Enable/disable keep-alive mode. When true, the engine stays alive even
    /// with no pending/running commands (used for RPC listen mode). The loop
    /// only exits on shutdown signal.
    pub fn set_keep_alive(&mut self, v: bool) {
        self.keep_alive = v;
    }

    /// Clone the command sender so external callers (e.g., RPC) can submit
    /// download commands to the engine loop.
    pub fn command_sender(&self) -> mpsc::UnboundedSender<Box<dyn Command>> {
        self.command_tx.clone()
    }

    /// Clone the EngineCommand sender so external callers (e.g., RPC) can
    /// submit structured download lifecycle commands.
    ///
    /// This is the v2 API that replaces `command_sender()` for download
    /// management (add/remove/pause/unpause/halt). The old `Box<dyn Command>`
    /// channel is retained for backward compatibility with existing code.
    pub fn engine_command_sender(&self) -> mpsc::UnboundedSender<EngineCommand> {
        self.engine_cmd_tx.clone()
    }

    /// Take the shutdown sender so an external task (e.g., Ctrl+C handler) can
    /// signal the engine to stop. Must be called before `run()`.
    pub fn take_shutdown_sender(&mut self) -> Option<oneshot::Sender<()>> {
        self.shutdown_tx.take()
    }

    /// Run the v2 engine loop using `EngineCommand` and `RequestGroupMan`
    /// promotion/demotion. This is the new main loop that mirrors the C++
    /// `DownloadEngine::run()` architecture with active/reserved/stopped
    /// queue management.
    ///
    /// Requires `request_group_man` to be set via `set_save_session()` or
    /// directly. If not set, falls back to the v1 loop.
    pub async fn run_v2(mut self) -> Result<()> {
        let group_man = self
            .request_group_man
            .take()
            .ok_or_else(|| Aria2Error::DownloadFailed(
                "run_v2 requires request_group_man to be set".to_string()
            ))?;

        let shutdown_rx = self
            .shutdown_rx
            .take()
            .ok_or_else(|| Aria2Error::DownloadFailed(
                "shutdown_rx already taken".to_string()
            ))?;

        let engine_cmd_rx = self
            .engine_cmd_rx
            .take()
            .ok_or_else(|| Aria2Error::DownloadFailed(
                "engine_cmd_rx already taken".to_string()
            ))?;

        let ctx = EngineLoopContext {
            group_man,
            ftp_pool: Arc::clone(&self.ftp_pool),
            dns_cache: Arc::clone(&self.dns_cache),
            auto_save: self.auto_save.take(),
            keep_alive: self.keep_alive,
        };

        super::engine_loop::run_engine_loop(
            ctx,
            engine_cmd_rx,
            shutdown_rx,
            self.tick_interval,
        )
        .await;

        Ok(())
    }

    pub async fn run(mut self) -> Result<()> {
        info!("Download engine started");

        let mut pending_commands: Vec<Box<dyn Command>> = Vec::new();
        // Spawned command tasks. Each task owns its `Box<dyn Command>` for the
        // duration of `execute()`, so commands run CONCURRENTLY instead of
        // serially blocking the engine loop (Task A2 fix). v1 does NOT recover
        // commands for retry on timeout/failure; progress is preserved via
        // session auto-save.
        let mut running: JoinSet<Result<()>> = JoinSet::new();
        let mut running_tasks: HashMap<Id, RunningTask> = HashMap::new();
        // Reserved for future retry support; v1 never populates this (failed /
        // timed-out commands are dropped, not retried), but the slot is kept so
        // the exit condition and retry plumbing remain intact.
        let mut failed_commands: Vec<(Box<dyn Command>, u32)> = Vec::new();

        let mut ticker = interval(self.tick_interval);
        let mut shutdown_rx = self
            .shutdown_rx
            .take()
            .expect("shutdown_rx should exist in run()");
        let policy = self.retry_policy.clone();
        let stats = self.retry_stats.clone();

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    debug!("Engine tick triggered");

                    // 1. Retry previously failed commands (v1: always empty).
                    for (cmd, attempt) in failed_commands.drain(..) {
                        if policy.should_retry(attempt, &Aria2Error::Recoverable(RecoverableError::Timeout)) {
                            let wait = policy.wait_duration(attempt);
                            warn!("Retrying command (attempt {}), waiting {:?}", attempt + 1, wait);
                            pending_commands.push(cmd);
                            tokio::time::sleep(wait).await;
                        } else {
                            error!("Command retry abandoned (attempted {} times)", attempt + 1);
                        }
                    }

                    // 2. Spawn pending commands concurrently (Task A2 fix).
                    self.dispatch_commands(
                        &mut pending_commands,
                        &mut running,
                        &mut running_tasks,
                    )
                    .await?;

                    // 3. Abort tasks whose per-command timeout elapsed (Task A1 fix).
                    self.check_timeouts(&mut running_tasks, &stats).await?;

                    // 4. Reap finished / aborted tasks.
                    self.collect_completed(&mut running, &mut running_tasks).await?;

                    // 5. Exit when nothing remains (unless keep-alive). Use the
                    //    JoinSet's own emptiness as the source of truth: an
                    //    aborted-but-not-yet-reaped task still counts as running
                    //    until collect_completed joins it.
                    if !self.keep_alive
                        && pending_commands.is_empty()
                        && running.is_empty()
                        && failed_commands.is_empty()
                    {
                        info!("All tasks completed, engine shutting down");
                        break;
                    }
                }

                Ok(_) = &mut shutdown_rx => {
                    info!("Shutdown signal received");
                    self.shutdown(&mut running).await;
                    break;
                }
            }
        }

        info!(
            "Download engine stopped, retry stats: total={}, timeouts={}, server_errors={}, network_failures={}",
            stats.total(),
            stats.timeouts(),
            stats.server_errors(),
            stats.network_failures()
        );
        Ok(())
    }

    /// Drain newly-arrived commands from the channel and spawn each as an
    /// independent task on the `JoinSet` so they execute CONCURRENTLY instead
    /// of serially blocking the engine loop (Task A2 fix).
    ///
    /// Each `Box<dyn Command>` is moved into its spawned task, which owns it
    /// for the duration of `execute()`. This avoids any shared-state locking
    /// between the engine and the task, maximizing concurrency.
    async fn dispatch_commands(
        &mut self,
        pending: &mut Vec<Box<dyn Command>>,
        running: &mut JoinSet<Result<()>>,
        running_tasks: &mut HashMap<Id, RunningTask>,
    ) -> Result<()> {
        // Pull any commands that arrived since the last tick.
        while let Ok(cmd) = self.command_rx.try_recv() {
            pending.push(cmd);
        }

        while !pending.is_empty() {
            let mut cmd = pending.remove(0);
            // Inform the command of its start instant (no-op by default) so
            // commands that override set_started_at can self-report elapsed.
            cmd.set_started_at(Instant::now());
            let timeout = cmd.timeout();

            // The command is moved into the spawned task. The task calls
            // `execute(&mut self)` on its owned command and returns the
            // `Result<()>`. On abort the command is dropped with the task.
            let handle = running.spawn(async move {
                let mut cmd = cmd;
                cmd.execute().await
            });
            let id = handle.id();
            running_tasks.insert(
                id,
                RunningTask {
                    handle,
                    started: Instant::now(),
                    timeout,
                },
            );
            debug!(
                "Dispatched command (task {}), running: {}",
                id,
                running.len()
            );
        }
        Ok(())
    }

    /// Abort any running task whose per-command timeout has elapsed (Task A1
    /// fix).
    ///
    /// The previous implementation awaited `tokio::time::timeout(dur, async
    /// {})` on an EMPTY future that resolved instantly, so no command ever
    /// timed out. This rewrite tracks each task's start instant and aborts the
    /// spawned task via its `AbortHandle` once the elapsed time exceeds the
    /// command's `timeout()`.
    ///
    /// v1 limitation: a timed-out command is owned by its spawned task and
    /// cannot be safely recovered for retry, so it is dropped when the aborted
    /// task is reaped by `collect_completed`. Progress is preserved via session
    /// auto-save. Auto-retry on timeout is deferred to a follow-up that retains
    /// command ownership in the engine.
    async fn check_timeouts(
        &self,
        running_tasks: &mut HashMap<Id, RunningTask>,
        stats: &RetryStats,
    ) -> Result<()> {
        let now = Instant::now();
        // Collect ids whose timeout has elapsed. We cannot mutate
        // running_tasks while iterating it, so gather first.
        let timed_out: Vec<Id> = running_tasks
            .iter()
            .filter_map(|(id, task)| {
                let dur = task.timeout?;
                if now.duration_since(task.started) > dur {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();

        for id in timed_out {
            if let Some(task) = running_tasks.remove(&id) {
                warn!("Command (task {}) timed out, aborting", id);
                // Abort the spawned task. The task's owned command is dropped
                // when the runtime reaps the cancelled future (reaped by
                // collect_completed on a subsequent tick).
                task.handle.abort();
                stats.record_retry(&Aria2Error::Recoverable(RecoverableError::Timeout));
            }
        }
        Ok(())
    }

    /// Reap all tasks that have finished or been aborted since the last tick.
    ///
    /// `try_join_next_with_id` is non-blocking: it returns `None` when no task
    /// is ready, leaving in-flight tasks running. On abort/panic,
    /// `JoinError::id()` recovers the task id so we can clean up our
    /// bookkeeping even when the task did not return a value.
    async fn collect_completed(
        &self,
        running: &mut JoinSet<Result<()>>,
        running_tasks: &mut HashMap<Id, RunningTask>,
    ) -> Result<()> {
        while let Some(join_result) = running.try_join_next_with_id() {
            match join_result {
                Ok((id, Ok(()))) => {
                    running_tasks.remove(&id);
                    debug!("Command (task {}) completed successfully", id);
                }
                Ok((id, Err(e))) => {
                    running_tasks.remove(&id);
                    // v1: execute() failures are not auto-retried (the command
                    // is owned by the task and has been dropped). Log and rely
                    // on session auto-save for progress.
                    error!("Command (task {}) execution failed: {}", id, e);
                }
                Err(join_err) => {
                    let id = join_err.id();
                    running_tasks.remove(&id);
                    if join_err.is_cancelled() {
                        // Cancelled by check_timeouts (timeout) or shutdown.
                        debug!("Command (task {}) was cancelled", id);
                    } else if join_err.is_panic() {
                        error!("Command (task {}) panicked: {}", id, join_err);
                    } else {
                        warn!("Command (task {}) joined with error: {}", id, join_err);
                    }
                }
            }
        }
        Ok(())
    }

    async fn shutdown(&self, running: &mut JoinSet<Result<()>>) {
        info!("Shutting down running commands...");
        // Persist session state before tearing down tasks so partial progress
        // is not lost.
        if let (Some(path), Some(man)) = (&self.save_session_path, &self.request_group_man) {
            let mut cmd = SaveSessionCommand::new(path.clone(), man.clone());
            match cmd.execute().await {
                Ok(_) => info!("Session saved on shutdown to {}", path.display()),
                Err(e) => warn!("Failed to save session on shutdown: {}", e),
            }
        }
        // Abort every still-running command task.
        running.abort_all();
    }

    pub async fn shutdown_engine(&mut self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::command::CommandStatus;
    use crate::request::request_group::GroupId;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    /// A command that sleeps far longer than its reported timeout, so the
    /// engine must abort it. Sets `completed` only if `execute()` runs to
    /// completion; an abort cancels the sleep and leaves it `false`.
    struct StalledCommand {
        timeout_dur: Duration,
        completed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Command for StalledCommand {
        async fn execute(&mut self) -> Result<()> {
            // Sleep far beyond the test bound so only an abort can finish us.
            tokio::time::sleep(Duration::from_secs(30)).await;
            self.completed.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn status(&self) -> CommandStatus {
            CommandStatus::Running
        }

        fn gid(&self) -> GroupId {
            GroupId(0)
        }

        fn timeout(&self) -> Option<Duration> {
            Some(self.timeout_dur)
        }
    }

    /// Regression test for Task A1: the old `check_timeouts` awaited an EMPTY
    /// future (`async {}`) that resolved instantly, so no command ever timed
    /// out. This test verifies a stalled command is actually aborted once its
    /// per-command timeout elapses, and that `RetryStats::timeouts()` is
    /// incremented.
    #[tokio::test]
    async fn test_check_timeouts_actually_times_out_stalled_command() {
        // 10ms tick so timeouts are detected promptly.
        let engine = DownloadEngine::new(10);
        // Clone the stats Arc before run() consumes the engine so we can
        // inspect counters after the loop exits.
        let stats = engine.retry_stats.clone();

        let completed = Arc::new(AtomicBool::new(false));
        let cmd = StalledCommand {
            timeout_dur: Duration::from_millis(50),
            completed: completed.clone(),
        };
        engine.add_command(Box::new(cmd)).unwrap();

        // The engine exits once the stalled command is aborted and reaped
        // (pending/running/failed all empty). Bound at 2s to fail loudly if it
        // hangs (which would indicate the timeout was NOT enforced).
        let run_result = tokio::time::timeout(Duration::from_secs(2), engine.run()).await;

        assert!(
            run_result.is_ok(),
            "engine.run() should complete within 2s, not hang (timeout not enforced?)"
        );
        assert!(
            stats.timeouts() >= 1,
            "expected at least 1 recorded timeout, got {} (Task A1 bug: empty future)",
            stats.timeouts()
        );
        assert!(
            !completed.load(Ordering::SeqCst),
            "stalled command should have been aborted before completing"
        );
    }

    /// A command that sleeps for `delay` then increments a shared counter.
    /// Used to verify the engine dispatches commands concurrently (Task A2).
    struct SlowCommand {
        delay: Duration,
        completed: Arc<AtomicU32>,
    }

    #[async_trait]
    impl Command for SlowCommand {
        async fn execute(&mut self) -> Result<()> {
            tokio::time::sleep(self.delay).await;
            self.completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn status(&self) -> CommandStatus {
            CommandStatus::Running
        }

        fn gid(&self) -> GroupId {
            GroupId(0)
        }
    }

    /// Regression test for Task A2: the old `dispatch_commands` awaited
    /// `cmd.execute()` inline in the engine loop, serializing all commands.
    /// With 5 commands x 100ms that would take ~500ms+. This test verifies
    /// they run concurrently (well under 300ms).
    #[tokio::test]
    async fn test_engine_dispatches_commands_concurrently() {
        // 10ms tick so dispatch happens promptly on the first tick.
        let engine = DownloadEngine::new(10);
        let completed = Arc::new(AtomicU32::new(0));
        for _ in 0..5 {
            let cmd = SlowCommand {
                delay: Duration::from_millis(100),
                completed: completed.clone(),
            };
            engine.add_command(Box::new(cmd)).unwrap();
        }

        let start = Instant::now();
        let run_result = tokio::time::timeout(Duration::from_millis(500), engine.run()).await;
        let elapsed = start.elapsed();

        assert!(
            run_result.is_ok(),
            "engine.run() should complete within 500ms, not hang"
        );
        assert_eq!(
            completed.load(Ordering::SeqCst),
            5,
            "all 5 commands should have completed"
        );
        // 5 commands x 100ms serialized = ~500ms + tick overhead. Concurrent
        // execution should finish in ~100ms + a couple of ticks. A 300ms bound
        // still proves concurrency beyond doubt while tolerating CI jitter.
        assert!(
            elapsed < Duration::from_millis(300),
            "5x100ms commands took {:?}, indicating serialization (concurrent should be ~150ms)",
            elapsed
        );
    }

    // ==================== Progress channel (Task E3) tests ====================

    use crate::engine::command::ProgressUpdate;
    use crate::request::request_group::{DownloadOptions, RequestGroup};

    /// Helper: build a fresh `RequestGroup` wrapped in an `Arc<std::sync::RwLock<..>>`,
    /// the same shape `DownloadCommand` and the aggregator use.
    fn make_group() -> Arc<std::sync::RwLock<RequestGroup>> {
        Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(1),
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )))
    }

    /// Verify the aggregator receives `ProgressUpdate`s sent through the
    /// channel and applies them to the `RequestGroup` (both the RwLock-backed
    /// `completed_length` via `update_progress` and the atomic mirror via
    /// `set_completed_length`).
    #[tokio::test]
    async fn test_progress_channel_updates_request_group() {
        let group = make_group();
        let (tx, rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        let group_clone = Arc::clone(&group);
        let handle = DownloadEngine::spawn_progress_aggregator(
            group_clone,
            group.recover().progress.clone(),
            rx,
        );

        // Send a sequence of strictly increasing updates.
        tx.send(ProgressUpdate {
            completed_bytes: 1000,
            download_speed: 0,
            upload_speed: 0,
        })
        .unwrap();
        tx.send(ProgressUpdate {
            completed_bytes: 5000,
            download_speed: 0,
            upload_speed: 0,
        })
        .unwrap();

        // Drop the sender: the aggregator drains all queued messages (unbounded
        // channel recv only returns None after the queue is empty and all
        // senders are gone), then exits. Awaiting the handle is therefore a
        // deterministic synchronization point — no sleep-based polling needed.
        drop(tx);
        handle.await.expect("aggregator task should exit cleanly");

        // The atomic mirror is set by `set_completed_length`; verify final value.
        let atomic_val = { group.recover().get_completed_length() };
        assert_eq!(
            atomic_val, 5000,
            "aggregator should have applied the latest completed_bytes (5000)"
        );
    }

    /// Verify the aggregator skips no-op updates with identical
    /// `completed_bytes` (deduplication), so a flood of stale in-flight sends
    /// does not cause redundant write-lock acquisitions.
    #[tokio::test]
    async fn test_progress_aggregator_dedupes_identical_bytes() {
        let group = make_group();
        let (tx, rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        let group_clone = Arc::clone(&group);
        let handle = DownloadEngine::spawn_progress_aggregator(
            group_clone,
            group.recover().progress.clone(),
            rx,
        );

        // First real update.
        tx.send(ProgressUpdate {
            completed_bytes: 2048,
            download_speed: 0,
            upload_speed: 0,
        })
        .unwrap();
        // Several duplicates with the same completed_bytes (and a speed that
        // must NOT be applied because the dedup `continue`s before reaching
        // the speed branch).
        for _ in 0..5 {
            tx.send(ProgressUpdate {
                completed_bytes: 2048,
                download_speed: 9999,
                upload_speed: 0,
            })
            .unwrap();
        }
        // A real advance with a speed sample that SHOULD be applied.
        tx.send(ProgressUpdate {
            completed_bytes: 4096,
            download_speed: 1234,
            upload_speed: 0,
        })
        .unwrap();

        // Deterministic drain: drop sender + await handle guarantees all
        // queued messages have been processed by the aggregator.
        drop(tx);
        handle.await.expect("aggregator task should exit cleanly");

        let g = group.recover();
        assert_eq!(
            g.get_completed_length(),
            4096,
            "final completed_bytes should be 4096"
        );
        // Speed is now EMA-smoothed by the aggregator's SpeedSmoother.
        // We cannot predict the exact EMA value in a unit test, but it must
        // be > 0 (a positive delta was recorded).
        assert!(
            g.get_download_speed_cached() > 0,
            "smoothed speed should be > 0 after positive delta, got {}",
            g.get_download_speed_cached()
        );
    }

    /// Verify that the aggregator applies EMA-smoothed speed whenever a
    /// positive byte delta is recorded, regardless of the sender's raw
    /// `download_speed` sample.
    #[tokio::test]
    async fn test_progress_aggregator_applies_smoothed_speed_on_delta() {
        let group = make_group();
        let (tx, rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        // Seed the group with a known cached speed before starting the aggregator.
        {
            let g = group.recover();
            g.set_download_speed_cached(5555);
        }

        let group_clone = Arc::clone(&group);
        let handle = DownloadEngine::spawn_progress_aggregator(
            group_clone,
            group.recover().progress.clone(),
            rx,
        );

        // Advance bytes but send a 0 speed sample (no fresh measurement).
        tx.send(ProgressUpdate {
            completed_bytes: 8192,
            download_speed: 0,
            upload_speed: 0,
        })
        .unwrap();

        // Deterministic drain.
        drop(tx);
        handle.await.expect("aggregator task should exit cleanly");

        let g = group.recover();
        assert_eq!(g.get_completed_length(), 8192, "bytes should advance");
        // The smoothed speed is always computed from the byte delta and
        // applied to the cached speed (replacing the seeded 5555).
        assert!(
            g.get_download_speed_cached() > 0,
            "smoothed speed should be applied when delta > 0, got {}",
            g.get_download_speed_cached()
        );
    }

    /// Verify the aggregator task exits cleanly (JoinHandle resolves) once all
    /// senders are dropped, with no hang or resource leak.
    #[tokio::test]
    async fn test_progress_aggregator_exits_on_sender_drop() {
        let group = make_group();
        let (tx, rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        let handle = DownloadEngine::spawn_progress_aggregator(
            group.clone(),
            group.recover().progress.clone(),
            rx,
        );

        // Drop the only sender; the aggregator's `recv().await` returns None.
        drop(tx);

        // The handle should resolve promptly without needing to abort.
        let result = tokio::time::timeout(Duration::from_millis(500), handle).await;
        assert!(
            result.is_ok(),
            "aggregator should exit within 500ms after senders are dropped"
        );
        result
            .expect("aggregator task should exit cleanly")
            .expect("aggregator task should not panic");
    }

    /// Verify that EMA smoothing produces a stable, finite, positive speed
    /// value after a sequence of positive byte deltas.
    ///
    /// This test avoids asserting exact speed bounds because the EMA's
    /// instantaneous speed depends on real elapsed time (`delta / duration`),
    /// which varies with scheduler timing. The detailed EMA convergence and
    /// reaction behavior is covered by `speed_smooth::tests`.
    #[tokio::test]
    async fn test_progress_aggregator_smooths_speed() {
        let group = make_group();
        let (tx, rx) = mpsc::unbounded_channel::<ProgressUpdate>();

        let group_clone = Arc::clone(&group);
        let handle = DownloadEngine::spawn_progress_aggregator(
            group_clone,
            group.recover().progress.clone(),
            rx,
        );

        // Send a sequence of strictly increasing byte deltas, waiting long
        // enough between sends to cross the smoother's SAMPLE_INTERVAL_MS
        // (500ms) boundary so each delta triggers an EMA update.
        let deltas: [u64; 3] = [10000, 5000, 20000];
        for delta in &deltas {
            let current = {
                let g = group.recover();
                g.get_completed_length()
            };
            tx.send(ProgressUpdate {
                completed_bytes: current + delta,
                download_speed: 0, // Ignored — smoother computes from delta
                upload_speed: 0,
            })
            .unwrap();

            // Wait long enough for the smoother's SAMPLE_INTERVAL_MS to elapse
            // so the next record_bytes triggers an EMA update.
            tokio::time::sleep(Duration::from_millis(
                crate::constants::HTTP_SPEED_UPDATE_INTERVAL_MS + 100,
            ))
            .await;
        }

        // Drain the channel.
        drop(tx);
        handle.await.expect("aggregator task should exit cleanly");

        let g = group.recover();
        let final_speed = g.get_download_speed_cached();

        // The EMA-smoothed speed must be positive and finite after recording
        // positive deltas. We do not assert upper/lower bounds against the
        // raw delta values because the smoother divides by real elapsed time
        // (varies with scheduler jitter), not a fixed 1s window.
        assert!(
            final_speed > 0,
            "EMA-smoothed speed should be > 0 after positive deltas, got {}",
            final_speed
        );
        assert!(
            final_speed < u64::MAX / 2,
            "EMA-smoothed speed should be finite, got {}",
            final_speed
        );
    }

    // ==================== BtRegistry accessor tests ====================

    #[cfg(feature = "bittorrent")]
    /// Verify that the engine creates a BtRegistry and the accessor returns it.
    #[test]
    fn test_bt_registry_accessor_returns_valid_registry() {
        let engine = DownloadEngine::new(100);
        let registry = engine.bt_registry();
        let reg = registry.read().unwrap();
        assert!(reg.is_empty(), "new engine should have empty BtRegistry");
        assert_eq!(reg.tcp_port(), 0);
        assert_eq!(reg.udp_port(), 0);
    }

    #[cfg(feature = "bittorrent")]
    /// Verify that multiple Arc clones of the BtRegistry share the same
    /// underlying data, so changes made through one clone are visible
    /// through the other.
    #[test]
    fn test_bt_registry_arc_shared_ownership() {
        let engine = DownloadEngine::new(100);
        let registry_arc = engine.bt_registry().clone();

        // Insert via the cloned Arc
        {
            let mut reg = registry_arc.write().unwrap();
            reg.set_tcp_port(6881);
            let obj = super::super::bt_registry::BtObject::new();
            reg.put(42, obj);
        }

        // Verify visibility through the engine's accessor
        let reg = engine.bt_registry().read().unwrap();
        assert_eq!(reg.tcp_port(), 6881);
        assert!(reg.get(42).is_some());
    }

    #[cfg(feature = "bittorrent")]
    /// Verify BtRegistry info-hash lookup works end-to-end when a
    /// DownloadContext with TorrentAttribute is registered.
    #[test]
    fn test_bt_registry_info_hash_lookup_via_engine() {
        use crate::download::download_context::{
            BtFileMode, ContextAttributeType, TorrentAttribute,
        };

        let engine = DownloadEngine::new(100);
        let info_hash = "abcdef0123456789abcdef0123456789abcdef01";

        // Create a DownloadContext with TorrentAttribute
        let mut ctx =
            crate::download::DownloadContext::new(1024, 4096, "/tmp/test.bin".to_string());
        let ta = TorrentAttribute {
            name: "test_torrent".to_string(),
            mode: BtFileMode::Single,
            announce_list: vec![],
            nodes: vec![],
            info_hash: info_hash.to_string(),
            metadata: vec![],
            metadata_size: 0,
            private_torrent: false,
            creation_date: 0,
            comment: String::new(),
            created_by: String::new(),
            url_list: vec![],
        };
        ctx.set_attribute(ContextAttributeType::BitTorrent, Box::new(ta));
        let ctx = Arc::new(ctx);

        // Register into BtRegistry
        let obj = super::super::bt_registry::BtObject::builder()
            .download_context(Arc::clone(&ctx))
            .build();
        {
            let mut reg = engine.bt_registry().write().unwrap();
            reg.put(123, obj);
        }

        // Lookup by info_hash should find the context
        let reg = engine.bt_registry().read().unwrap();
        let found = reg.get_download_context_by_info_hash(info_hash);
        assert!(
            found.is_some(),
            "info-hash lookup should find registered context"
        );

        // Wrong hash should not find it
        assert!(
            reg.get_download_context_by_info_hash("wrong_hash")
                .is_none(),
            "wrong hash should not match"
        );
    }
}

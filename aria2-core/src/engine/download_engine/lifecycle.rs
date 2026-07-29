use std::collections::HashMap;
use std::sync::Arc;

use tokio::task::{Id, JoinSet};
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use super::DownloadEngine;
use crate::engine::command::Command;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::session::save_session_command::SaveSessionCommand;

impl DownloadEngine {
    /// Run the v2 engine loop using `EngineCommand` and `RequestGroupMan`
    /// promotion/demotion. This is the new main loop that mirrors the C++
    /// `DownloadEngine::run()` architecture with active/reserved/stopped
    /// queue management.
    ///
    /// Requires `request_group_man` to be set via `set_save_session()` or
    /// directly. If not set, falls back to the v1 loop.
    pub async fn run_v2(mut self) -> Result<()> {
        let group_man = self.request_group_man.take().ok_or_else(|| {
            Aria2Error::DownloadFailed("run_v2 requires request_group_man to be set".to_string())
        })?;

        let shutdown_rx = self
            .shutdown_rx
            .take()
            .ok_or_else(|| Aria2Error::DownloadFailed("shutdown_rx already taken".to_string()))?;

        let engine_cmd_rx = self
            .engine_cmd_rx
            .take()
            .ok_or_else(|| Aria2Error::DownloadFailed("engine_cmd_rx already taken".to_string()))?;

        let ctx = super::super::engine_loop::EngineLoopContext {
            group_man,
            ftp_pool: Arc::clone(&self.ftp_pool),
            dns_cache: Arc::clone(&self.dns_cache),
            auto_save: self.auto_save.take(),
            event_hooks: Arc::new(super::super::download_event_hooks::DownloadEventHooks::new()),
            keep_alive: self.keep_alive,
        };

        super::super::engine_loop::run_engine_loop(ctx, engine_cmd_rx, shutdown_rx, self.tick_interval)
            .await;

        Ok(())
    }

    pub async fn run(mut self) -> Result<()> {
        info!("Download engine started");

        let mut pending_commands: Vec<Box<dyn super::super::command::Command>> = Vec::new();
        // Spawned command tasks. Each task owns its `Box<dyn Command>` for the
        // duration of `execute()`, so commands run CONCURRENTLY instead of
        // serially blocking the engine loop (Task A2 fix). v1 does NOT recover
        // commands for retry on timeout/failure; progress is preserved via
        // session auto-save.
        let mut running: JoinSet<Result<()>> = JoinSet::new();
        let mut running_tasks: HashMap<Id, super::RunningTask> = HashMap::new();
        // Reserved for future retry support; v1 never populates this (failed /
        // timed-out commands are dropped, not retried), but the slot is kept so
        // the exit condition and retry plumbing remain intact.
        let mut failed_commands: Vec<(Box<dyn super::super::command::Command>, u32)> = Vec::new();

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

    pub(crate) async fn shutdown(&self, running: &mut JoinSet<Result<()>>) {
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
    use crate::engine::command::{Command, CommandStatus};
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
        let engine = super::DownloadEngine::new(10);
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
        let engine = super::DownloadEngine::new(10);
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
}

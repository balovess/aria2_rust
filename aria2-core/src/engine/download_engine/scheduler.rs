use std::collections::HashMap;

use tokio::task::{Id, JoinSet};
use tracing::{debug, error, warn};

use super::DownloadEngine;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::retry::RetryStats;

impl DownloadEngine {
    /// Drain newly-arrived commands from the channel and spawn each as an
    /// independent task on the `JoinSet` so they execute CONCURRENTLY instead
    /// of serially blocking the engine loop (Task A2 fix).
    ///
    /// Each `Box<dyn Command>` is moved into its spawned task, which owns it
    /// for the duration of `execute()`. This avoids any shared-state locking
    /// between the engine and the task, maximizing concurrency.
    pub(crate) async fn dispatch_commands(
        &mut self,
        pending: &mut Vec<Box<dyn super::super::command::Command>>,
        running: &mut JoinSet<Result<()>>,
        running_tasks: &mut HashMap<Id, super::RunningTask>,
    ) -> Result<()> {
        // Pull any commands that arrived since the last tick.
        while let Ok(cmd) = self.command_rx.try_recv() {
            pending.push(cmd);
        }

        while !pending.is_empty() {
            let mut cmd = pending.remove(0);
            // Inform the command of its start instant (no-op by default) so
            // commands that override set_started_at can self-report elapsed.
            cmd.set_started_at(std::time::Instant::now());
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
                super::RunningTask {
                    handle,
                    started: std::time::Instant::now(),
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
    pub(crate) async fn check_timeouts(
        &self,
        running_tasks: &mut HashMap<Id, super::RunningTask>,
        stats: &RetryStats,
    ) -> Result<()> {
        let now = std::time::Instant::now();
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
    pub(crate) async fn collect_completed(
        &self,
        running: &mut JoinSet<Result<()>>,
        running_tasks: &mut HashMap<Id, super::RunningTask>,
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
}

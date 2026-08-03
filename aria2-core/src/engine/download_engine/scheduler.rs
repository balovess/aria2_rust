use std::collections::HashMap;
use std::sync::Arc;

use tokio::task::{Id, JoinSet};
use tracing::{debug, error, warn};

use super::DownloadEngine;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::request::request_group::{DownloadStatus, RequestGroup};
use crate::retry::RetryStats;
use crate::util::rwlock_ext::RwLockRecover;

/// Record a terminal `Error` status on `group` unless it already reached a
/// terminal or user-driven state.
///
/// A download task can end with `Err` for reasons that are *not* download
/// errors — `aria2.remove` sets `Removed` and `aria2.pause` sets `Paused`,
/// both of which cancel the in-flight task. Overwriting those with `Error`
/// would misreport the download and emit a bogus `aria2.onDownloadError`
/// notification, so they are left untouched. Mirrors C++
/// `RequestGroupMan::executeStopHook()`, which only maps a result to
/// `EVENT_ON_DOWNLOAD_ERROR` when it is neither `IN_PROGRESS` nor `REMOVED`.
fn record_command_failure(group: &Arc<std::sync::RwLock<RequestGroup>>, reason: &str) {
    let g = group.recover();
    if matches!(
        g.status(),
        DownloadStatus::Complete | DownloadStatus::Error(_) | DownloadStatus::Removed
    ) || g.is_pause_requested()
    {
        return;
    }
    g.mark_error(reason.to_string());
}

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
            let group = cmd.request_group();
            let task_group = group.clone();

            // The command is moved into the spawned task. The task calls
            // `execute(&mut self)` on its owned command and returns the
            // `Result<()>`. On abort the command is dropped with the task.
            let handle = running.spawn(async move {
                let mut cmd = cmd;
                let result = cmd.execute().await;
                // Record the failure on the group *inside* the task, while the
                // error value is still available. This is what gives the v1
                // loop an `Error` status transition — and with it the
                // `aria2.onDownloadError` notification, which is published by
                // the observer attached to `RequestGroup::mark_error()`.
                if let (Err(e), Some(g)) = (&result, &task_group) {
                    record_command_failure(g, &e.to_string());
                }
                result
            });
            let id = handle.id();
            running_tasks.insert(
                id,
                super::RunningTask {
                    handle,
                    started: std::time::Instant::now(),
                    timeout,
                    group,
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
                // The aborted future is cancelled at an await point and can
                // never run its own failure path, so the timeout is recorded
                // on the group here instead.
                if let Some(ref group) = task.group {
                    record_command_failure(group, "Download timed out");
                }
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

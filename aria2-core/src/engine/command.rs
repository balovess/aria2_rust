use crate::error::Result;
use crate::request::request_group::{GroupId, RequestGroup};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum CommandStatus {
    Pending,
    Running,
    Completed,
}

/// Progress update sent from a download command to the engine via channel.
/// This decouples the download task from the RequestGroup RwLock: instead of
/// acquiring `group.write().await` on every batched progress checkpoint, the
/// download task performs a cheap lock-free `send` and a single aggregator
/// task applies the update to the `RequestGroup`.
///
/// Fields:
/// - `completed_bytes`: total bytes downloaded so far for this command.
/// - `download_speed`: current download speed in bytes/sec. `0` means the
///   sender did not refresh the speed sample this tick (the aggregator keeps
///   the previously cached value).
/// - `upload_speed`: upload speed in bytes/sec (BT only; `0` for HTTP/FTP).
#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    /// Total bytes downloaded so far for this command.
    pub completed_bytes: u64,
    /// Current download speed in bytes/sec (0 if not yet calculated).
    pub download_speed: u64,
    /// Upload speed in bytes/sec (for BT, 0 for HTTP).
    pub upload_speed: u64,
}

#[async_trait]
pub trait Command: Send + Sync {
    async fn execute(&mut self) -> Result<()>;

    fn status(&self) -> CommandStatus;

    fn timeout(&self) -> Option<Duration> {
        None
    }

    /// Returns the instant at which this command started executing, if the
    /// engine has informed the command via [`set_started_at`].
    ///
    /// The default implementation reports `None`; commands that wish to
    /// self-report elapsed time (e.g. for `status()` queries) may override
    /// this together with [`set_started_at`].
    fn started_at(&self) -> Option<Instant> {
        None
    }

    /// Called by the engine immediately before the command is spawned onto a
    /// background task. The default implementation is a no-op; commands that
    /// store the instant can expose it through [`started_at`].
    fn set_started_at(&mut self, _instant: Instant) {}

    /// Returns the `GroupId` of the `RequestGroup` this command belongs to.
    /// Used by the engine loop to track which group a completed task belongs
    /// to, so it can decrement the `num_commands` counter and check whether
    /// the group should be demoted from active to stopped.
    fn gid(&self) -> GroupId;

    /// Returns the `RequestGroup` this command drives, when it has one.
    ///
    /// Protocol commands use this association for shared progress and
    /// cancellation state. Bookkeeping commands have no group and keep the
    /// default `None`.
    fn request_group(&self) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        None
    }
}

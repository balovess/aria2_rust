use std::collections::HashMap;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::super::engine_command::EngineCommand;
use super::{DownloadManager, DownloadManagerError};
use crate::error::Aria2Error;
use crate::request::request_group::{
    DownloadResult, DownloadStatus, DownloadStatusSnapshot, GroupId,
};
use crate::request::request_group_man::PositionMode;
use crate::util::rwlock_ext::RwLockRecover;
/// A stable identity for one submitted download.
#[derive(Clone)]
pub struct DownloadHandle {
    pub(super) gid: GroupId,
    pub(super) manager: DownloadManager,
}

impl DownloadHandle {
    pub fn gid(&self) -> GroupId {
        self.gid
    }

    pub fn gid_hex(&self) -> String {
        self.gid.to_hex_string()
    }

    /// Capture the current live-task progress without exposing internal locks.
    pub fn status_snapshot(&self) -> Option<DownloadStatusSnapshot> {
        self.manager
            .group_man
            .find_group(self.gid)
            .map(|group| group.recover().status_snapshot())
    }

    /// Return the richest available snapshot for this GID.
    ///
    /// A live group is converted in place; after demotion the retained stopped
    /// result is returned. `None` means the GID has not been accepted by the
    /// manager or its stopped result has already been purged.
    pub fn download_result(&self) -> Option<DownloadResult> {
        self.manager
            .group_man
            .find_group(self.gid)
            .map(|group| group.recover().create_download_result())
            .or_else(|| self.manager.group_man.find_stopped_result(&self.gid_hex()))
    }

    /// Return the current file metadata snapshot for this download.
    ///
    /// For magnet links, call this after receiving
    /// [`MetadataResolvedEvent`](crate::MetadataResolvedEvent). Before
    /// metadata is available the result may contain only the fallback URI
    /// entry, just like the corresponding `DownloadResult` snapshot.
    pub fn get_files(&self) -> Option<Vec<crate::request::request_group::FileEntry>> {
        self.download_result().map(|result| result.files)
    }

    /// Return the flattened URI snapshot for all requested files.
    ///
    /// The returned entries preserve the per-file order from [`Self::get_files`]
    /// and use aria2-compatible `waiting`/`used`/`spent` status strings.
    pub fn get_uris(&self) -> Option<Vec<crate::request::request_group::UriEntry>> {
        self.get_files()
            .map(|files| files.into_iter().flat_map(|file| file.uris).collect())
    }

    /// Return task-level options that have already been applied to the live
    /// request group.
    pub fn runtime_options(&self) -> Option<HashMap<String, serde_json::Value>> {
        self.manager
            .group_man
            .find_group(self.gid)
            .map(|group| group.recover().runtime_options())
    }

    /// Return task-level options queued for the next command generation.
    pub fn pending_options(&self) -> Option<HashMap<String, serde_json::Value>> {
        self.manager
            .group_man
            .find_group(self.gid)
            .map(|group| group.recover().pending_options())
    }

    /// Change the URI set for one file entry.
    ///
    /// `file_index` is one-based, matching aria2's `changeUri` contract.
    /// Deletions happen before additions; when `position` is `Some`, added
    /// URIs are inserted at that zero-based position. The returned pair is
    /// `(deleted_count, added_count)`.
    pub fn change_uris(
        &self,
        file_index: usize,
        delete_uris: &[String],
        add_uris: &[String],
        position: Option<usize>,
    ) -> std::result::Result<(usize, usize), DownloadManagerError> {
        let group = self.manager.group_man.find_group(self.gid).ok_or_else(|| {
            DownloadManagerError::State(Aria2Error::InvalidArgument(
                "download is not live".to_string(),
            ))
        })?;
        group
            .recover_mut()
            .change_uris(file_index, delete_uris, add_uris, position)
            .map_err(DownloadManagerError::State)
    }

    /// Apply aria2-compatible runtime option changes to this download.
    ///
    /// The manager validates each option and applies immediate changes in
    /// place. Changes that require a new command generation are retained as
    /// pending options and trigger the existing restart semantics when the
    /// task is active.
    pub fn change_options(
        &self,
        changes: HashMap<String, serde_json::Value>,
    ) -> std::result::Result<(), DownloadManagerError> {
        self.manager
            .group_man
            .change_group_options(&self.gid_hex(), changes)
            .map_err(|error| DownloadManagerError::State(Aria2Error::InvalidArgument(error)))
    }

    /// Change this download's position in the reserved queue.
    ///
    /// The operation is valid only while the group is waiting to be promoted.
    /// The returned position is zero-based, matching the request-group
    /// manager and the RPC `aria2.changePosition` contract.
    pub fn change_position(
        &self,
        position: i32,
        mode: PositionMode,
    ) -> std::result::Result<usize, DownloadManagerError> {
        self.manager
            .group_man
            .change_position(self.gid, position, mode)
            .map_err(DownloadManagerError::State)
    }

    /// Queue a graceful pause command.
    ///
    /// The current state is validated before the command is queued. Success
    /// means the command was accepted by the engine queue; a concurrent state
    /// transition may still make the engine ignore it. Use
    /// [`Self::wait_for_status`] with [`DownloadStatus::Paused`] when the
    /// caller must wait until the state transition is visible.
    pub fn pause(&self) -> std::result::Result<(), DownloadManagerError> {
        self.require_status("pause", |status| {
            matches!(status, DownloadStatus::Active | DownloadStatus::Waiting)
        })?;
        self.send_control(EngineCommand::Pause { gid: self.gid })
    }

    /// Queue a forced pause command after validating the current state.
    pub fn force_pause(&self) -> std::result::Result<(), DownloadManagerError> {
        self.require_status("force-pause", |status| {
            matches!(status, DownloadStatus::Active | DownloadStatus::Waiting)
        })?;
        self.send_control(EngineCommand::ForcePause { gid: self.gid })
    }

    /// Queue a resume command after validating that the task is paused.
    ///
    /// A resumed task first becomes [`DownloadStatus::Waiting`] and may later
    /// be promoted to [`DownloadStatus::Active`].
    pub fn resume(&self) -> std::result::Result<(), DownloadManagerError> {
        self.require_status("resume", |status| status.is_paused())?;
        self.send_control(EngineCommand::Unpause { gid: self.gid })
    }

    /// Queue a graceful removal command for a live task.
    pub fn remove(&self) -> std::result::Result<(), DownloadManagerError> {
        self.require_live()?;
        self.send_control(EngineCommand::RemoveDownload { gid: self.gid })
    }

    /// Queue a forced removal command for a live task.
    pub fn force_remove(&self) -> std::result::Result<(), DownloadManagerError> {
        self.require_live()?;
        self.send_control(EngineCommand::ForceRemoveDownload { gid: self.gid })
    }

    /// Wait until this download reaches `expected` without polling.
    ///
    /// The comparison is by status kind, so every [`DownloadStatus::Error`]
    /// value matches `Error` regardless of its human-readable message. The
    /// returned result is the freshest snapshot observed at the transition.
    pub async fn wait_for_status(
        &self,
        expected: DownloadStatus,
    ) -> std::result::Result<DownloadResult, DownloadManagerError> {
        self.wait_for_status_with_cancellation(expected, &CancellationToken::new())
            .await
    }

    /// Wait for a status transition for at most `timeout`.
    ///
    /// A timeout only stops this future; it does not change the download.
    pub async fn wait_for_status_with_timeout(
        &self,
        expected: DownloadStatus,
        timeout: Duration,
    ) -> std::result::Result<DownloadResult, DownloadManagerError> {
        tokio::time::timeout(timeout, self.wait_for_status(expected))
            .await
            .map_err(|_| DownloadManagerError::WaitTimeout {
                operation: "a download status",
            })?
    }

    /// Wait for a status transition while allowing the caller to cancel the
    /// wait without changing the download.
    pub async fn wait_for_status_with_cancellation(
        &self,
        expected: DownloadStatus,
        cancellation: &CancellationToken,
    ) -> std::result::Result<DownloadResult, DownloadManagerError> {
        let signal = self.manager.group_man.activity_signal();
        let mut observed = signal.generation();

        loop {
            let result = self
                .download_result()
                .ok_or_else(|| self.not_found_error())?;
            if result.status.as_str() == expected.as_str() {
                return Ok(result);
            }

            tokio::select! {
                _ = cancellation.cancelled() => {
                    return Err(DownloadManagerError::WaitCancelled);
                }
                _ = signal.wait_for_change(&mut observed) => {}
            }
        }
    }

    /// Wait for structured metadata resolution involving this download.
    ///
    /// This is intended for magnet and metadata-backed downloads. It returns
    /// the parent metadata GID and every child download GID created from that
    /// metadata. The wait is safe even when called after submission: the
    /// manager retains a bounded history of recent metadata events and the
    /// receiver is installed before that history is checked.
    ///
    /// Ordinary HTTP downloads do not emit this event and should use
    /// [`Self::wait`] instead.
    pub async fn wait_for_metadata(
        &self,
    ) -> std::result::Result<crate::MetadataResolvedEvent, DownloadManagerError> {
        self.wait_for_metadata_with_cancellation(&CancellationToken::new())
            .await
    }

    /// Wait for metadata resolution for at most `timeout`.
    ///
    /// A timeout only stops this future; it does not pause, remove, or
    /// otherwise change the download. Metadata resolution remains represented
    /// by [`crate::MetadataResolvedEvent`], not by [`DownloadStatus`].
    pub async fn wait_for_metadata_with_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<crate::MetadataResolvedEvent, DownloadManagerError> {
        tokio::time::timeout(timeout, self.wait_for_metadata())
            .await
            .map_err(|_| DownloadManagerError::WaitTimeout {
                operation: "metadata resolution",
            })?
    }

    /// Wait for metadata resolution while allowing the caller to cancel the
    /// wait without changing the download.
    pub async fn wait_for_metadata_with_cancellation(
        &self,
        cancellation: &CancellationToken,
    ) -> std::result::Result<crate::MetadataResolvedEvent, DownloadManagerError> {
        if self.download_result().is_none() {
            return Err(self.not_found_error());
        }

        let mut events = self.manager.subscribe();
        if let Some(event) = self.manager.event_hooks.metadata_event_for(self.gid) {
            return Ok(event);
        }

        let signal = self.manager.group_man.activity_signal();
        let mut observed = signal.generation();
        loop {
            if let Some(error) = self.metadata_resolution_error() {
                return Err(error);
            }

            tokio::select! {
                result = events.recv_metadata_for(self.gid) => {
                    return result.map_err(DownloadManagerError::EventStream);
                }
                _ = signal.wait_for_change(&mut observed) => {}
                _ = cancellation.cancelled() => return Err(DownloadManagerError::WaitCancelled),
            }
        }
    }

    /// Wait for a terminal result without polling the RPC status or file list.
    ///
    /// The manager's generation signal is level-sensitive, so a notification
    /// published before this method starts waiting is still observed. Paused
    /// downloads remain live and therefore do not complete this future.
    pub async fn wait(&self) -> std::result::Result<DownloadResult, DownloadManagerError> {
        self.wait_with_cancellation(&CancellationToken::new()).await
    }

    /// Wait for a terminal result for at most `timeout`.
    ///
    /// A timeout only stops this future; it does not change the download.
    pub async fn wait_with_timeout(
        &self,
        timeout: Duration,
    ) -> std::result::Result<DownloadResult, DownloadManagerError> {
        tokio::time::timeout(timeout, self.wait())
            .await
            .map_err(|_| DownloadManagerError::WaitTimeout {
                operation: "a terminal download result",
            })?
    }

    /// Wait for a terminal result while allowing the caller to cancel the wait.
    ///
    /// Cancelling the token only stops this future; it does not pause, remove,
    /// or otherwise change the download. The same event-driven generation
    /// signal used by [`Self::wait`] wakes the future when the download changes.
    pub async fn wait_with_cancellation(
        &self,
        cancellation: &CancellationToken,
    ) -> std::result::Result<DownloadResult, DownloadManagerError> {
        let signal = self.manager.group_man.activity_signal();
        let mut observed = signal.generation();

        loop {
            let result = self
                .download_result()
                .ok_or_else(|| self.not_found_error())?;
            if result.status.is_terminal() {
                return Ok(result);
            }

            tokio::select! {
                _ = cancellation.cancelled() => {
                    return Err(DownloadManagerError::WaitCancelled);
                }
                _ = signal.wait_for_change(&mut observed) => {}
            }
        }
    }

    fn send_control(
        &self,
        command: EngineCommand,
    ) -> std::result::Result<(), DownloadManagerError> {
        self.manager.command_sender.send(command)?;
        Ok(())
    }

    fn require_live(&self) -> std::result::Result<(), DownloadManagerError> {
        if self.manager.group_man.find_group(self.gid).is_some() {
            Ok(())
        } else {
            Err(self.not_found_error())
        }
    }

    fn require_status(
        &self,
        operation: &str,
        accepts: impl FnOnce(&DownloadStatus) -> bool,
    ) -> std::result::Result<(), DownloadManagerError> {
        let group = self
            .manager
            .group_man
            .find_group(self.gid)
            .ok_or_else(|| self.not_found_error())?;
        let status = group.recover().status();
        if accepts(&status) {
            Ok(())
        } else {
            Err(DownloadManagerError::State(Aria2Error::InvalidArgument(
                format!(
                    "GID#{} cannot be {operation} while status is {}",
                    self.gid_hex(),
                    status.as_str()
                ),
            )))
        }
    }

    fn not_found_error(&self) -> DownloadManagerError {
        DownloadManagerError::State(Aria2Error::InvalidArgument(format!(
            "GID#{} not found",
            self.gid_hex()
        )))
    }

    fn metadata_resolution_error(&self) -> Option<DownloadManagerError> {
        let result = self.download_result()?;
        match result.status {
            DownloadStatus::Error(message) => {
                Some(DownloadManagerError::MetadataResolutionFailed {
                    status: "error".to_string(),
                    message: if message.is_empty() {
                        "download failed before metadata resolved".to_string()
                    } else {
                        message
                    },
                })
            }
            DownloadStatus::Removed => Some(DownloadManagerError::MetadataResolutionFailed {
                status: "removed".to_string(),
                message: "download was removed before metadata resolved".to_string(),
            }),
            DownloadStatus::Waiting
            | DownloadStatus::Active
            | DownloadStatus::Paused
            | DownloadStatus::Complete => None,
        }
    }
}

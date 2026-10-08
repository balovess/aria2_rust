//! High-level download-management interface for Rust embedders.
//!
//! The engine and request-group modules remain available for the CLI and RPC
//! adapters. This module is the deeper seam for applications that only need
//! to submit work, observe immutable snapshots, control a task, and await a
//! terminal result without learning the engine's channel or lock layout.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::download_engine::DownloadEngine;
use super::download_event_hooks::{DownloadEventHooks, DownloadEventStream};
use super::engine_command::{EngineCommand, EngineCommandSendError, EngineCommandSender};
use crate::error::{Aria2Error, Result};
use crate::request::request_group::{
    DownloadOptions, DownloadResult, DownloadStatus, DownloadStatusSnapshot, GroupId, RequestGroup,
};
use crate::request::request_group_man::{PositionMode, RequestGroupMan};
use crate::util::rwlock_ext::RwLockRecover;

/// Errors returned by the high-level download-management interface.
#[derive(Debug, thiserror::Error)]
pub enum DownloadManagerError {
    #[error("engine command submission failed: {0}")]
    Command(#[from] EngineCommandSendError),
    #[error("download preparation failed: {0}")]
    Preparation(#[source] Aria2Error),
    #[error("download state operation failed: {0}")]
    State(#[source] Aria2Error),
    #[error("download event stream failed: {0}")]
    EventStream(#[source] tokio::sync::broadcast::error::RecvError),
    #[error("metadata resolution failed while download was {status}: {message}")]
    MetadataResolutionFailed { status: String, message: String },
    #[error("download engine lifecycle failed: {0}")]
    Engine(#[source] Aria2Error),
    #[error("waiting for the download was cancelled")]
    WaitCancelled,
    #[error("waiting for {operation} timed out")]
    WaitTimeout { operation: &'static str },
}

/// A small, cloneable interface for submitting and controlling downloads.
///
/// The manager owns no engine loop. It is normally obtained from a configured
/// [`DownloadEngine`] through [`DownloadEngine::download_manager`], or built
/// explicitly when an application owns the engine wiring itself.
#[derive(Clone)]
pub struct DownloadManager {
    group_man: Arc<RequestGroupMan>,
    command_sender: EngineCommandSender,
    event_hooks: Arc<DownloadEventHooks>,
}

impl DownloadManager {
    /// Build a manager with an explicitly owned event bus.
    ///
    /// Embedders that host more than one engine in a process should use this
    /// constructor so their event streams remain isolated.
    pub fn with_event_hooks(
        group_man: Arc<RequestGroupMan>,
        command_sender: EngineCommandSender,
        event_hooks: Arc<DownloadEventHooks>,
    ) -> Self {
        Self {
            group_man,
            command_sender,
            event_hooks,
        }
    }

    /// Submit one URI download and return its stable handle immediately.
    ///
    /// The group is registered before the wake-up command is queued. This
    /// makes a handle's first control operation deterministic even when the
    /// engine is busy or the command queue prioritizes control traffic.
    pub fn add_uri(
        &self,
        uris: Vec<String>,
        options: DownloadOptions,
    ) -> std::result::Result<DownloadHandle, DownloadManagerError> {
        if uris.is_empty() || uris.iter().any(|uri| uri.trim().is_empty()) {
            return Err(DownloadManagerError::Preparation(
                Aria2Error::InvalidArgument("at least one non-empty URI is required".to_string()),
            ));
        }

        let gid = self.group_man.next_available_gid();
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid, uris, options,
        )));
        self.group_man.add_group_arc(Arc::clone(&group));

        if let Err(error) = self
            .command_sender
            .send(EngineCommand::AddDownload { group })
        {
            // The command was not accepted, so do not leave an unowned task
            // in the manager's waiting queue.
            self.group_man.remove_group_by_id(gid);
            return Err(error.into());
        }

        Ok(self.handle(gid))
    }

    /// Submit an in-memory `.torrent` document and return its stable handle.
    ///
    /// The torrent is parsed and its file metadata is prepared before the
    /// group is registered. A parse or metadata error therefore leaves the
    /// manager unchanged. Web-seed URIs are attached to the same task and are
    /// used as fallback HTTP sources by the BitTorrent downloader.
    #[cfg(feature = "bittorrent")]
    pub fn add_torrent(
        &self,
        data: Vec<u8>,
        web_seed_uris: Vec<String>,
        options: DownloadOptions,
    ) -> std::result::Result<DownloadHandle, DownloadManagerError> {
        if data.is_empty() {
            return Err(DownloadManagerError::Preparation(
                Aria2Error::InvalidArgument("torrent data must not be empty".to_string()),
            ));
        }

        let gid = self.group_man.next_available_gid();
        let mut uris = Vec::with_capacity(1);
        uris.push(format!("bt://{}", gid.to_hex_string()));
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            uris,
            options.clone(),
        )));

        if options.pause {
            group
                .recover_mut()
                .pause()
                .map_err(DownloadManagerError::Preparation)?;
        }

        super::bittorrent::download::command::prepare_group_metadata(
            Arc::clone(&group),
            &data,
            &options,
            options.dir.as_deref(),
            &web_seed_uris,
        )
        .map_err(DownloadManagerError::Preparation)?;
        group.recover().set_bt_metadata_data(data);
        self.group_man.add_group_arc(Arc::clone(&group));

        if let Err(error) = self
            .command_sender
            .send(EngineCommand::AddDownload { group })
        {
            self.group_man.remove_group_by_id(gid);
            return Err(error.into());
        }

        Ok(self.handle(gid))
    }

    /// Submit an in-memory Metalink document and return handles for every
    /// task created from it.
    ///
    /// Direct-resource entries produce one handle each. When both the
    /// `metalink` and `bittorrent` features are enabled, torrent metaurls
    /// produce a metadata/payload pair, with the metadata handle queued first.
    /// The document is fully converted before any group is registered, and a
    /// later queue-send failure rolls back all groups created by this call.
    #[cfg(feature = "metalink")]
    pub fn add_metalink(
        &self,
        data: Vec<u8>,
        options: DownloadOptions,
    ) -> std::result::Result<Vec<DownloadHandle>, DownloadManagerError> {
        if data.is_empty() {
            return Err(DownloadManagerError::Preparation(
                Aria2Error::InvalidArgument("metalink data must not be empty".to_string()),
            ));
        }

        let converter = super::metalink::to_request_group::MetalinkToRequestGroup::new()
            .with_pause_requested(options.pause);
        let mut gids = std::iter::from_fn(|| Some(self.group_man.next_available_gid()));
        let expansion = converter
            .create_groups_from_bytes(&data, &options, &mut gids)
            .map_err(DownloadManagerError::Preparation)?;
        let resource_groups = expansion.resource_groups;
        #[cfg(feature = "bittorrent")]
        let graphs = expansion.torrent_graphs;

        let resource_tasks = resource_groups
            .into_iter()
            .map(|group| {
                let gid = group.recover().gid();
                (gid, group)
            })
            .collect::<Vec<_>>();
        #[cfg(feature = "bittorrent")]
        let mut tasks = resource_tasks;
        #[cfg(not(feature = "bittorrent"))]
        let tasks = resource_tasks;
        let mut registered_gids = Vec::with_capacity(tasks.len());

        for (gid, group) in &tasks {
            self.group_man.add_group_arc(Arc::clone(group));
            registered_gids.push(*gid);
        }

        #[cfg(feature = "bittorrent")]
        for graph in graphs {
            let metadata_gid = graph.metadata.recover().gid();
            let payload_gid = graph.payload.recover().gid();
            let metadata = Arc::clone(&graph.metadata);
            let payload = Arc::clone(&graph.payload);
            if let Err(error) = self.group_man.add_metalink_graph(graph) {
                for gid in registered_gids {
                    self.group_man.remove_group_by_id(gid);
                }
                return Err(DownloadManagerError::Preparation(error));
            }
            registered_gids.extend([metadata_gid, payload_gid]);
            tasks.extend([(metadata_gid, metadata), (payload_gid, payload)]);
        }

        for (_, group) in &tasks {
            if let Err(error) = self.command_sender.send(EngineCommand::AddDownload {
                group: Arc::clone(group),
            }) {
                for registered_gid in &registered_gids {
                    self.group_man.remove_group_by_id(*registered_gid);
                }
                return Err(error.into());
            }
        }

        Ok(tasks.into_iter().map(|(gid, _)| self.handle(gid)).collect())
    }

    /// Return a handle for an existing or not-yet-dispatched GID.
    pub fn handle(&self, gid: GroupId) -> DownloadHandle {
        DownloadHandle {
            gid,
            manager: self.clone(),
        }
    }

    /// Return handles for all currently live downloads.
    ///
    /// The snapshot contains reserved and active tasks, but not stopped
    /// results. Retain a returned handle if its terminal result must remain
    /// addressable after the engine removes the live group.
    pub fn handles(&self) -> Vec<DownloadHandle> {
        self.group_man
            .all_groups()
            .into_iter()
            .map(|(gid, _)| self.handle(gid))
            .collect()
    }

    /// Find a handle for a live download or a retained stopped result.
    ///
    /// A stopped result can disappear later when the result cache is purged.
    pub fn find(&self, gid: GroupId) -> Option<DownloadHandle> {
        if self.group_man.find_group(gid).is_some()
            || self
                .group_man
                .find_stopped_result(&gid.to_hex_string())
                .is_some()
        {
            Some(self.handle(gid))
        } else {
            None
        }
    }

    /// Subscribe to lifecycle and metadata events produced by this manager's
    /// event bus.
    pub fn subscribe(&self) -> DownloadEventStream {
        self.event_hooks.subscribe()
    }

    /// Return the engine-applied maximum concurrent-download limit.
    ///
    /// A value of `0` means unlimited. After [`Self::set_max_concurrent`],
    /// this value changes when the engine consumes the queued command.
    pub fn max_concurrent(&self) -> usize {
        self.group_man.max_concurrent()
    }

    /// Return the engine-applied process-wide download limit in bytes/sec.
    pub fn global_download_limit(&self) -> Option<u64> {
        self.group_man.global_download_limit()
    }

    /// Return the engine-applied process-wide upload limit in bytes/sec.
    pub fn global_upload_limit(&self) -> Option<u64> {
        self.group_man.global_upload_limit()
    }

    /// Return a snapshot of retained terminal results using aria2 pagination
    /// semantics. Negative offsets count backward from the newest result.
    pub fn stopped_results(&self, offset: i32, count: usize) -> Vec<DownloadResult> {
        self.group_man.get_stopped_results(offset, count)
    }

    /// Return the number of retained terminal results.
    pub fn stopped_results_len(&self) -> usize {
        self.group_man.stopped_results_len()
    }

    /// Remove completed, failed, and retained terminal results.
    ///
    /// Returns the number of entries removed from both live scheduling stores
    /// and stopped-result retention.
    pub fn clear_completed(&self) -> std::result::Result<usize, DownloadManagerError> {
        self.group_man
            .clear_completed()
            .map_err(DownloadManagerError::State)
    }

    /// Queue a graceful pause for every active and reserved download.
    ///
    /// The command is coalesced with other pending pause-all commands. Success
    /// means it was accepted by the engine queue; callers that need to
    /// observe individual state transitions can await each returned handle.
    pub fn pause_all(&self) -> std::result::Result<(), DownloadManagerError> {
        self.command_sender.send(EngineCommand::PauseAll)?;
        Ok(())
    }

    /// Queue a forced pause for every active and reserved download.
    pub fn force_pause_all(&self) -> std::result::Result<(), DownloadManagerError> {
        self.command_sender.send(EngineCommand::ForcePauseAll)?;
        Ok(())
    }

    /// Queue a resume for every paused download.
    pub fn resume_all(&self) -> std::result::Result<(), DownloadManagerError> {
        self.command_sender.send(EngineCommand::UnpauseAll)?;
        Ok(())
    }

    /// Update the process-wide concurrent-download limit.
    ///
    /// `0` means unlimited, matching aria2 and [`RequestGroupMan`]. The
    /// change is applied by the engine loop; if the limit is lowered, the
    /// existing scheduler pauses excess active downloads.
    pub fn set_max_concurrent(&self, max: u32) -> std::result::Result<(), DownloadManagerError> {
        self.command_sender
            .send(EngineCommand::SetMaxConcurrent { max })?;
        Ok(())
    }

    /// Update process-wide download and upload limits in bytes per second.
    ///
    /// Passing `None` for one direction removes that direction's limit. The
    /// shared limiter is updated by the engine loop, so already-running
    /// downloads observe the new rates without being recreated.
    pub fn set_global_rate_limit(
        &self,
        download_limit: Option<u64>,
        upload_limit: Option<u64>,
    ) -> std::result::Result<(), DownloadManagerError> {
        self.command_sender
            .send(EngineCommand::SetGlobalRateLimit {
                download_limit,
                upload_limit,
            })?;
        Ok(())
    }
}

/// A stable identity for one submitted download.
#[derive(Clone)]
pub struct DownloadHandle {
    gid: GroupId,
    manager: DownloadManager,
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

/// A running engine together with the handles needed to control and await it.
pub struct DownloadEngineHandle {
    manager: DownloadManager,
    task: JoinHandle<Result<()>>,
}

impl DownloadEngineHandle {
    pub fn downloads(&self) -> &DownloadManager {
        &self.manager
    }

    /// Request graceful engine shutdown.
    ///
    /// Success means the halt command was accepted by the engine queue. Use
    /// [`Self::wait`] to await actual engine termination.
    pub fn shutdown(&self) -> std::result::Result<(), DownloadManagerError> {
        self.manager.command_sender.send(EngineCommand::HaltAll {
            reason: crate::request::request_group::HaltReason::ShutdownSignal,
        })?;
        Ok(())
    }

    /// Request forced engine shutdown.
    ///
    /// Success means the force-halt command was accepted by the engine queue.
    pub fn force_shutdown(&self) -> std::result::Result<(), DownloadManagerError> {
        self.manager
            .command_sender
            .send(EngineCommand::ForceHaltAll {
                reason: crate::request::request_group::HaltReason::ShutdownSignal,
            })?;
        Ok(())
    }

    /// Request graceful shutdown and wait until the engine task exits.
    pub async fn shutdown_and_wait(self) -> std::result::Result<(), DownloadManagerError> {
        self.shutdown()?;
        self.wait().await.map_err(DownloadManagerError::Engine)
    }

    /// Request forced shutdown and wait until the engine task exits.
    pub async fn force_shutdown_and_wait(self) -> std::result::Result<(), DownloadManagerError> {
        self.force_shutdown()?;
        self.wait().await.map_err(DownloadManagerError::Engine)
    }

    /// Consume the handle and wait for the engine task to finish.
    pub async fn wait(self) -> Result<()> {
        self.task.await.map_err(|error| {
            Aria2Error::DownloadFailed(format!("download engine task panicked: {error}"))
        })?
    }
}

impl DownloadEngine {
    /// Return the high-level manager after a request-group manager has been
    /// configured with [`Self::set_request_group_man`].
    pub fn download_manager(&self) -> Option<DownloadManager> {
        self.request_group_man.as_ref().map(|group_man| {
            DownloadManager::with_event_hooks(
                Arc::clone(group_man),
                self.engine_cmd_tx.clone(),
                Arc::clone(&self.event_hooks),
            )
        })
    }

    /// Start the engine and return an owning handle for its lifecycle.
    pub fn start(self) -> Result<DownloadEngineHandle> {
        let manager = self.download_manager().ok_or_else(|| {
            Aria2Error::DownloadFailed("start requires request_group_man to be set".to_string())
        })?;
        let task = tokio::spawn(async move { self.run().await });
        Ok(DownloadEngineHandle { manager, task })
    }

    /// Attach a request-group manager, start the engine, and return its owning
    /// lifecycle handle in one step.
    ///
    /// This is the compact entry point for embedders that do not need to
    /// configure the engine between wiring the manager and starting it.
    pub fn start_with_request_group_man(
        mut self,
        group_man: Arc<RequestGroupMan>,
    ) -> Result<DownloadEngineHandle> {
        self.set_request_group_man(group_man);
        self.start()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::util::rwlock_ext::RwLockRecover;

    fn manager(
        group_man: Arc<RequestGroupMan>,
        command_sender: EngineCommandSender,
    ) -> DownloadManager {
        DownloadManager::with_event_hooks(
            group_man,
            command_sender,
            Arc::new(DownloadEventHooks::new()),
        )
    }

    #[tokio::test]
    async fn handle_waits_for_terminal_state_without_polling() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let gid = group_man
            .add_group(
                vec!["http://example.test/file".to_string()],
                DownloadOptions::default(),
            )
            .expect("group registration");
        let handle = manager.handle(gid);
        let waiter = tokio::spawn({
            let handle = handle.clone();
            async move { handle.wait().await }
        });

        tokio::task::yield_now().await;
        let group = group_man.find_group(gid).expect("group exists");
        group.recover().mark_complete();

        let result = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("wait must be event-driven")
            .expect("wait task must not panic")
            .expect("terminal result");
        assert_eq!(result.status, DownloadStatus::Complete);
    }

    #[tokio::test]
    async fn handle_waits_for_requested_status_without_polling() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let handle = manager
            .add_uri(
                vec!["http://example.test/file".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");
        let waiter = tokio::spawn({
            let handle = handle.clone();
            async move { handle.wait_for_status(DownloadStatus::Paused).await }
        });

        tokio::task::yield_now().await;
        let group = group_man.find_group(handle.gid()).expect("group exists");
        group
            .recover_mut()
            .pause()
            .expect("pause transition should succeed");

        let result = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("status wait must be event-driven")
            .expect("status wait task must not panic")
            .expect("status transition should be observed");
        assert_eq!(result.status, DownloadStatus::Paused);
    }

    #[tokio::test]
    async fn handle_waits_for_metadata_and_replays_a_late_subscription() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let handle = manager
            .add_uri(
                vec!["magnet:?xt=urn:btih:example".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");
        let event = crate::MetadataResolvedEvent::new(handle.gid(), vec![GroupId::new(0x42)]);
        let waiter = tokio::spawn({
            let handle = handle.clone();
            async move { handle.wait_for_metadata().await }
        });

        tokio::task::yield_now().await;
        manager.event_hooks.notify_metadata_resolved(event.clone());

        let resolved = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("metadata wait must be event-driven")
            .expect("metadata wait task must not panic")
            .expect("metadata event should be observed");
        assert_eq!(resolved, event);

        let replayed = handle
            .wait_for_metadata()
            .await
            .expect("recent metadata event should be replayed");
        assert_eq!(replayed, event);
    }

    #[tokio::test]
    async fn metadata_wait_can_be_cancelled_without_changing_download_state() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let handle = manager
            .add_uri(
                vec!["magnet:?xt=urn:btih:example".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let result = handle
            .wait_for_metadata_with_cancellation(&cancellation)
            .await;

        assert!(matches!(result, Err(DownloadManagerError::WaitCancelled)));
        assert!(handle.status_snapshot().is_some());
    }

    #[tokio::test]
    async fn metadata_wait_timeout_does_not_change_download_state() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let handle = manager
            .add_uri(
                vec!["magnet:?xt=urn:btih:example".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");

        let result = handle
            .wait_for_metadata_with_timeout(Duration::from_millis(1))
            .await;

        assert!(matches!(
            result,
            Err(DownloadManagerError::WaitTimeout {
                operation: "metadata resolution"
            })
        ));
        assert!(handle.status_snapshot().is_some());
    }

    #[tokio::test]
    async fn metadata_wait_returns_when_resolution_fails() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let handle = manager
            .add_uri(
                vec!["magnet:?xt=urn:btih:example".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");
        let waiter = tokio::spawn({
            let handle = handle.clone();
            async move { handle.wait_for_metadata().await }
        });

        tokio::task::yield_now().await;
        group_man
            .find_group(handle.gid())
            .expect("group exists")
            .recover()
            .mark_error("no metadata peers".to_string());

        let result = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("metadata failure must wake the waiter")
            .expect("metadata waiter must not panic");
        assert!(matches!(
            result,
            Err(DownloadManagerError::MetadataResolutionFailed { status, message })
                if status == "error" && message == "no metadata peers"
        ));
    }

    #[tokio::test]
    async fn handle_wait_can_be_cancelled_without_changing_download_state() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let handle = manager
            .add_uri(
                vec!["http://example.test/file".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");
        let cancellation = CancellationToken::new();
        let waiter = tokio::spawn({
            let handle = handle.clone();
            let cancellation = cancellation.clone();
            async move { handle.wait_with_cancellation(&cancellation).await }
        });

        tokio::task::yield_now().await;
        cancellation.cancel();

        let result = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("cancellation must wake the wait")
            .expect("wait task must not panic");
        assert!(matches!(result, Err(DownloadManagerError::WaitCancelled)));
        assert!(handle.status_snapshot().is_some());
    }

    #[tokio::test]
    async fn unknown_handle_waits_fail_without_waiting_for_an_event() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let handle = manager.handle(GroupId::new(0xdead_beef));

        let status_result = tokio::time::timeout(
            Duration::from_secs(1),
            handle.wait_for_status(DownloadStatus::Waiting),
        )
        .await
        .expect("unknown status wait must return promptly");
        assert!(matches!(
            status_result,
            Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
        ));

        let terminal_result = tokio::time::timeout(Duration::from_secs(1), handle.wait())
            .await
            .expect("unknown terminal wait must return promptly");
        assert!(matches!(
            terminal_result,
            Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
        ));
    }

    #[test]
    fn control_commands_reject_unknown_and_invalid_states_before_queueing() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, mut command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let unknown = manager.handle(GroupId::new(0xdead_beef));

        assert!(matches!(
            unknown.pause(),
            Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
        ));
        assert!(matches!(
            unknown.force_pause(),
            Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
        ));
        assert!(matches!(
            unknown.resume(),
            Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
        ));
        assert!(matches!(
            unknown.remove(),
            Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
        ));
        assert!(matches!(
            unknown.force_remove(),
            Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
        ));
        assert!(matches!(
            command_receiver.try_recv(),
            Err(super::super::engine_command::EngineCommandTryRecvError::Empty)
        ));

        let live = manager
            .add_uri(
                vec!["https://example.test/file".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");
        assert!(matches!(
            live.resume(),
            Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
        ));
    }

    #[tokio::test]
    async fn starts_with_request_group_manager_in_one_step() {
        let mut engine = DownloadEngine::new();
        engine.set_keep_alive(true);
        let handle = engine
            .start_with_request_group_man(Arc::new(RequestGroupMan::new()))
            .expect("engine should start with a request-group manager");

        assert!(handle.downloads().handles().is_empty());
        tokio::time::timeout(Duration::from_secs(1), handle.shutdown_and_wait())
            .await
            .expect("engine should stop promptly")
            .expect("engine should stop cleanly");
    }

    #[tokio::test]
    async fn force_shutdown_and_wait_stops_keep_alive_engine() {
        let mut engine = DownloadEngine::new();
        engine.set_keep_alive(true);
        let handle = engine
            .start_with_request_group_man(Arc::new(RequestGroupMan::new()))
            .expect("engine should start with a request-group manager");

        tokio::time::timeout(Duration::from_secs(1), handle.force_shutdown_and_wait())
            .await
            .expect("force shutdown should stop promptly")
            .expect("force shutdown should complete cleanly");
    }

    #[test]
    fn add_uri_registers_before_command_dispatch() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let handle = manager
            .add_uri(
                vec!["http://example.test/file".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");

        assert!(handle.status_snapshot().is_some());
        assert_eq!(group_man.count(), 1);
    }

    #[test]
    fn add_uri_rejects_empty_input_before_registration() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);

        for uris in [Vec::new(), vec![String::new()], vec!["   ".to_string()]] {
            let result = manager.add_uri(uris, DownloadOptions::default());
            assert!(matches!(
                result,
                Err(DownloadManagerError::Preparation(
                    Aria2Error::InvalidArgument(_)
                ))
            ));
        }

        assert_eq!(group_man.count(), 0);
    }

    #[test]
    fn manager_lists_and_finds_live_handles_without_exposing_groups() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let handle = manager
            .add_uri(
                vec!["http://example.test/file".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");

        let handles = manager.handles();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].gid(), handle.gid());
        assert!(manager.find(handle.gid()).is_some());
        assert!(manager.find(GroupId::new(0xdead_beef)).is_none());
    }

    #[test]
    fn manager_exposes_batch_lifecycle_commands() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, mut command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);

        manager
            .pause_all()
            .expect("pause-all command should be accepted");
        assert!(matches!(
            command_receiver.try_recv(),
            Ok(EngineCommand::PauseAll)
        ));

        manager
            .force_pause_all()
            .expect("force-pause-all command should be accepted");
        assert!(matches!(
            command_receiver.try_recv(),
            Ok(EngineCommand::ForcePauseAll)
        ));

        manager
            .resume_all()
            .expect("resume-all command should be accepted");
        assert!(matches!(
            command_receiver.try_recv(),
            Ok(EngineCommand::UnpauseAll)
        ));

        manager
            .set_max_concurrent(3)
            .expect("max-concurrent command should be accepted");
        assert!(matches!(
            command_receiver.try_recv(),
            Ok(EngineCommand::SetMaxConcurrent { max: 3 })
        ));

        manager
            .set_global_rate_limit(Some(1024), None)
            .expect("global rate-limit command should be accepted");
        assert!(matches!(
            command_receiver.try_recv(),
            Ok(EngineCommand::SetGlobalRateLimit {
                download_limit: Some(1024),
                upload_limit: None,
            })
        ));
    }

    #[test]
    fn manager_exposes_runtime_queries_and_stopped_results() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);

        assert_eq!(manager.max_concurrent(), 5);
        assert_eq!(manager.global_download_limit(), None);
        assert_eq!(manager.global_upload_limit(), None);
        assert_eq!(manager.stopped_results_len(), 0);

        let handle = manager
            .add_uri(
                vec!["https://example.test/file".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");
        group_man
            .remove_group(handle.gid())
            .expect("reserved download should be removable");

        assert_eq!(manager.stopped_results_len(), 1);
        assert_eq!(manager.stopped_results(0, 1)[0].gid, handle.gid());
        assert_eq!(manager.clear_completed().expect("clear should succeed"), 1);
        assert_eq!(manager.stopped_results_len(), 0);
    }

    #[test]
    fn handle_exposes_file_snapshot_without_rpc_polling() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let handle = manager
            .add_uri(
                vec!["https://example.test/file.zip".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");

        let files = handle.get_files().expect("live group snapshot");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "file.zip");
        let uris = handle.get_uris().expect("URI snapshot");
        assert_eq!(uris.len(), 1);
        assert_eq!(uris[0].uri, "https://example.test/file.zip");
    }

    #[tokio::test]
    async fn handle_changes_uris_and_wakes_snapshot_observers() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let handle = manager
            .add_uri(
                vec!["https://example.test/first".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");
        let signal = group_man.activity_signal();
        let mut observed = signal.generation();
        let add_uris = vec!["https://example.test/second".to_string()];

        assert_eq!(
            handle
                .change_uris(1, &[], &add_uris, Some(0))
                .expect("URI change should succeed"),
            (0, 1)
        );
        tokio::time::timeout(
            Duration::from_secs(1),
            signal.wait_for_change(&mut observed),
        )
        .await
        .expect("URI changes must wake snapshot observers");

        let files = handle.get_files().expect("live group snapshot");
        assert_eq!(files[0].uris[0].uri, add_uris[0]);
    }

    #[test]
    fn handle_applies_runtime_options_through_manager_policy() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let handle = manager
            .add_uri(
                vec!["https://example.test/file".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");
        let mut changes = HashMap::new();
        changes.insert("dir".to_string(), serde_json::json!("reserved-dir"));

        handle
            .change_options(changes)
            .expect("runtime option change should succeed");

        let group = group_man.find_group(handle.gid()).expect("group exists");
        let runtime_options = group.recover().runtime_options();
        assert_eq!(
            runtime_options.get("dir"),
            Some(&serde_json::json!("reserved-dir"))
        );
        assert_eq!(
            handle
                .runtime_options()
                .expect("live runtime options")
                .get("dir"),
            Some(&serde_json::json!("reserved-dir"))
        );
        assert!(
            handle
                .pending_options()
                .expect("live pending options")
                .is_empty()
        );
    }

    #[test]
    fn handle_changes_reserved_queue_position() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let first = manager
            .add_uri(
                vec!["https://example.test/first".to_string()],
                DownloadOptions::default(),
            )
            .expect("first download submission");
        let second = manager
            .add_uri(
                vec!["https://example.test/second".to_string()],
                DownloadOptions::default(),
            )
            .expect("second download submission");

        assert_eq!(
            second
                .change_position(0, PositionMode::SetFromStart)
                .expect("reserved position change"),
            0
        );
        assert!(matches!(
            first.change_position(-1, PositionMode::SetFromStart),
            Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
        ));
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn add_torrent_prepares_metadata_before_registration() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let mut torrent = b"d8:announce28:http://tracker.test/announce4:infod6:lengthi1e4:name8:file.bin12:piece lengthi1e6:pieces20:".to_vec();
        torrent.extend_from_slice(&[0; 20]);
        torrent.extend_from_slice(b"ee");

        let handle = manager
            .add_torrent(
                torrent,
                vec!["https://example.test/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .expect("valid torrent submission");

        assert!(handle.status_snapshot().is_some());
        let files = handle.get_files().expect("prepared file metadata");
        assert_eq!(files.len(), 1);
        assert_eq!(
            std::path::Path::new(&files[0].path)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("file.bin")
        );
        assert_eq!(group_man.count(), 1);
    }

    #[cfg(feature = "bittorrent")]
    #[test]
    fn add_torrent_rejects_invalid_data_without_registration() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);

        let result = manager.add_torrent(vec![1, 2, 3], Vec::new(), DownloadOptions::default());

        assert!(
            matches!(
                result,
                Err(DownloadManagerError::Preparation(
                    Aria2Error::BittorrentParse(_)
                ))
            ),
            "invalid torrent should retain the BitTorrent parse error: {:?}",
            result.as_ref().err()
        );
        assert_eq!(group_man.count(), 0);
    }

    #[cfg(feature = "metalink")]
    #[test]
    fn add_metalink_returns_handles_for_resource_groups() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="file.bin"><url>https://example.test/file.bin</url></file></metalink>"#;

        let handles = manager
            .add_metalink(data.to_vec(), DownloadOptions::default())
            .expect("valid Metalink submission");

        assert_eq!(handles.len(), 1);
        assert!(handles[0].status_snapshot().is_some());
        assert_eq!(group_man.count(), 1);
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn add_metalink_returns_metadata_and_payload_handles_for_torrent_metaurl() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = manager(Arc::clone(&group_man), command_sender);
        let data = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="file.bin"><metaurl mediatype="torrent">https://example.test/file.torrent</metaurl></file></metalink>"#;

        let handles = manager
            .add_metalink(data.to_vec(), DownloadOptions::default())
            .expect("valid torrent Metalink submission");

        assert_eq!(handles.len(), 2);
        assert_ne!(handles[0].gid(), handles[1].gid());
        assert!(
            handles
                .iter()
                .all(|handle| handle.status_snapshot().is_some())
        );
        assert_eq!(group_man.count(), 2);
    }
}

//! High-level download-management interface for Rust embedders.
//!
//! The engine and request-group modules remain available for the CLI and RPC
//! adapters. This module is the deeper seam for applications that only need
//! to submit work, observe immutable snapshots, control a task, and await a
//! terminal result without learning the engine's channel or lock layout.

use std::sync::Arc;

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
    #[error("waiting for the download was cancelled")]
    WaitCancelled,
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
        let mut uris = Vec::with_capacity(web_seed_uris.len() + 1);
        uris.push(format!("bt://{}", gid.to_hex_string()));
        uris.extend(web_seed_uris);
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

        super::bt_download_command::prepare_group_metadata(
            Arc::clone(&group),
            &data,
            &options,
            options.dir.as_deref(),
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

        let converter = super::metalink_to_request_group::MetalinkToRequestGroup::new()
            .with_pause_requested(options.pause);
        let mut resource_gids = std::iter::from_fn(|| Some(self.group_man.next_available_gid()));
        let resource_groups = converter
            .create_resource_groups_from_bytes(&data, &options, &mut resource_gids)
            .map_err(DownloadManagerError::Preparation)?;

        #[cfg(feature = "bittorrent")]
        let mut graph_gids = std::iter::from_fn(|| Some(self.group_man.next_available_gid()));
        #[cfg(feature = "bittorrent")]
        let graphs = converter
            .create_torrent_graphs_from_bytes(&data, &options, &mut graph_gids)
            .map_err(DownloadManagerError::Preparation)?;

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
    /// Success means the command was accepted by the engine queue. Use
    /// [`Self::wait_for_status`] with [`DownloadStatus::Paused`] when the
    /// caller must wait until the state transition is visible.
    pub fn pause(&self) -> std::result::Result<(), DownloadManagerError> {
        self.send_control(EngineCommand::Pause { gid: self.gid })
    }

    /// Queue a forced pause command and return once it is accepted.
    pub fn force_pause(&self) -> std::result::Result<(), DownloadManagerError> {
        self.send_control(EngineCommand::ForcePause { gid: self.gid })
    }

    /// Queue a resume command and return once it is accepted.
    ///
    /// A resumed task first becomes [`DownloadStatus::Waiting`] and may later
    /// be promoted to [`DownloadStatus::Active`].
    pub fn resume(&self) -> std::result::Result<(), DownloadManagerError> {
        self.send_control(EngineCommand::Unpause { gid: self.gid })
    }

    /// Queue a graceful removal command and return once it is accepted.
    pub fn remove(&self) -> std::result::Result<(), DownloadManagerError> {
        self.send_control(EngineCommand::RemoveDownload { gid: self.gid })
    }

    /// Queue a forced removal command and return once it is accepted.
    pub fn force_remove(&self) -> std::result::Result<(), DownloadManagerError> {
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
            if let Some(result) = self.download_result()
                && result.status.as_str() == expected.as_str()
            {
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

    /// Wait for metadata resolution while allowing the caller to cancel the
    /// wait without changing the download.
    pub async fn wait_for_metadata_with_cancellation(
        &self,
        cancellation: &CancellationToken,
    ) -> std::result::Result<crate::MetadataResolvedEvent, DownloadManagerError> {
        let mut events = self.manager.subscribe();
        if let Some(event) = self.manager.event_hooks.metadata_event_for(self.gid) {
            return Ok(event);
        }

        tokio::select! {
            result = events.recv_metadata_for(self.gid) => {
                result.map_err(DownloadManagerError::EventStream)
            }
            _ = cancellation.cancelled() => Err(DownloadManagerError::WaitCancelled),
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
            if let Some(result) = self.download_result()
                && result.status.is_terminal()
            {
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

    pub fn shutdown(&self) -> std::result::Result<(), DownloadManagerError> {
        self.manager.command_sender.send(EngineCommand::HaltAll {
            reason: crate::request::request_group::HaltReason::ShutdownSignal,
        })?;
        Ok(())
    }

    pub fn force_shutdown(&self) -> std::result::Result<(), DownloadManagerError> {
        self.manager
            .command_sender
            .send(EngineCommand::ForceHaltAll {
                reason: crate::request::request_group::HaltReason::ShutdownSignal,
            })?;
        Ok(())
    }

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
    async fn starts_with_request_group_manager_in_one_step() {
        let mut engine = DownloadEngine::new();
        engine.set_keep_alive(true);
        let handle = engine
            .start_with_request_group_man(Arc::new(RequestGroupMan::new()))
            .expect("engine should start with a request-group manager");

        assert!(handle.downloads().handles().is_empty());
        handle
            .shutdown()
            .expect("shutdown command should be accepted");
        tokio::time::timeout(Duration::from_secs(1), handle.wait())
            .await
            .expect("engine should stop promptly")
            .expect("engine should stop cleanly");
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

        assert!(matches!(
            result,
            Err(DownloadManagerError::Preparation(Aria2Error::Fatal(_)))
        ));
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

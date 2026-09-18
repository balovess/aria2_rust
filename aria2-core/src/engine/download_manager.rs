//! High-level download-management interface for Rust embedders.
//!
//! The engine and request-group modules remain available for the CLI and RPC
//! adapters. This module is the deeper seam for applications that only need
//! to submit work, observe immutable snapshots, control a task, and await a
//! terminal result without learning the engine's channel or lock layout.

use std::sync::Arc;

use tokio::task::JoinHandle;

use super::download_engine::DownloadEngine;
use super::download_event_hooks::{DownloadEventHooks, DownloadEventStream};
use super::engine_command::{EngineCommand, EngineCommandSendError, EngineCommandSender};
use crate::error::{Aria2Error, Result};
use crate::request::request_group::{
    DownloadOptions, DownloadResult, DownloadStatus, DownloadStatusSnapshot, GroupId, RequestGroup,
};
use crate::request::request_group_man::RequestGroupMan;
use crate::util::rwlock_ext::RwLockRecover;

/// Errors returned by the high-level download-management interface.
#[derive(Debug, thiserror::Error)]
pub enum DownloadManagerError {
    #[error("engine command submission failed: {0}")]
    Command(#[from] EngineCommandSendError),
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
    /// Build a manager using the process-wide compatibility event bus.
    pub fn new(group_man: Arc<RequestGroupMan>, command_sender: EngineCommandSender) -> Self {
        Self::with_event_hooks(
            group_man,
            command_sender,
            Arc::clone(DownloadEventHooks::shared()),
        )
    }

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

    /// Return a handle for an existing or not-yet-dispatched GID.
    pub fn handle(&self, gid: GroupId) -> DownloadHandle {
        DownloadHandle {
            gid,
            manager: self.clone(),
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

    pub fn pause(&self) -> std::result::Result<(), DownloadManagerError> {
        self.send_control(EngineCommand::Pause { gid: self.gid })
    }

    pub fn force_pause(&self) -> std::result::Result<(), DownloadManagerError> {
        self.send_control(EngineCommand::ForcePause { gid: self.gid })
    }

    pub fn resume(&self) -> std::result::Result<(), DownloadManagerError> {
        self.send_control(EngineCommand::Unpause { gid: self.gid })
    }

    pub fn remove(&self) -> std::result::Result<(), DownloadManagerError> {
        self.send_control(EngineCommand::RemoveDownload { gid: self.gid })
    }

    pub fn force_remove(&self) -> std::result::Result<(), DownloadManagerError> {
        self.send_control(EngineCommand::ForceRemoveDownload { gid: self.gid })
    }

    /// Wait for a terminal result without polling the RPC status or file list.
    ///
    /// The manager's generation signal is level-sensitive, so a notification
    /// published before this method starts waiting is still observed. Paused
    /// downloads remain live and therefore do not complete this future.
    pub async fn wait(&self) -> std::result::Result<DownloadResult, DownloadManagerError> {
        let signal = self.manager.group_man.activity_signal();
        let mut observed = signal.generation();

        loop {
            if let Some(result) = self.download_result()
                && matches!(
                    result.status,
                    DownloadStatus::Complete | DownloadStatus::Error(_) | DownloadStatus::Removed
                )
            {
                return Ok(result);
            }

            signal.wait_for_change(&mut observed).await;
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::request::request_group::DownloadStatus;
    use crate::util::rwlock_ext::RwLockRecover;

    #[tokio::test]
    async fn handle_waits_for_terminal_state_without_polling() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = DownloadManager::new(Arc::clone(&group_man), command_sender);
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

    #[test]
    fn add_uri_registers_before_command_dispatch() {
        let group_man = Arc::new(RequestGroupMan::new());
        let (command_sender, _command_receiver) = super::super::engine_command::channel();
        let manager = DownloadManager::new(Arc::clone(&group_man), command_sender);
        let handle = manager
            .add_uri(
                vec!["http://example.test/file".to_string()],
                DownloadOptions::default(),
            )
            .expect("download submission");

        assert!(handle.status_snapshot().is_some());
        assert_eq!(group_man.count(), 1);
    }
}

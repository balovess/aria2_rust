use std::sync::Arc;

use tokio::task::JoinHandle;

use super::super::download_engine::DownloadEngine;
use super::super::engine_command::EngineCommand;
use super::{DownloadManager, DownloadManagerError};
use crate::error::{Aria2Error, Result};
use crate::request::request_group_man::RequestGroupMan;
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

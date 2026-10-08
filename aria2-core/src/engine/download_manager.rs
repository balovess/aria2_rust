//! High-level download-management interface for Rust embedders.
//!
//! The engine and request-group modules remain available for the CLI and RPC
//! adapters. This module is the deeper seam for applications that only need
//! to submit work, observe immutable snapshots, control a task, and await a
//! terminal result without learning the engine's channel or lock layout.

use std::sync::Arc;

use super::download_event_hooks::{DownloadEventHooks, DownloadEventStream};
use super::engine_command::{EngineCommand, EngineCommandSendError, EngineCommandSender};
use crate::error::Aria2Error;
use crate::request::request_group::{DownloadOptions, DownloadResult, GroupId, RequestGroup};
use crate::request::request_group_man::RequestGroupMan;
#[cfg(any(feature = "bittorrent", feature = "metalink"))]
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
/// [`crate::engine::download_engine::DownloadEngine`] through
/// [`crate::engine::download_engine::DownloadEngine::download_manager`], or built
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

mod engine_handle;
mod handle;

pub use engine_handle::DownloadEngineHandle;
pub use handle::DownloadHandle;
#[cfg(test)]
mod tests;

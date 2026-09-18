use std::sync::Arc;

use aria2_core::config::OptionRegistry;
use aria2_core::engine::command::Command;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::session::save_session_command::SaveSessionCommand;
use aria2_rpc::{
    BackendError, BackendEvent, BackendMetadata, BackendReadSnapshot, BackendRequest,
    BackendResponse, BackendResult, RpcBackend,
};
use async_trait::async_trait;

use super::query::paginate;
use super::{CoreRpcBackend, RPC_SHUTDOWN_GRACE};

#[async_trait]
impl RpcBackend for CoreRpcBackend {
    fn metadata(&self) -> BackendMetadata {
        self.metadata.clone()
    }

    async fn task_count(&self) -> usize {
        self.group_man.count()
    }

    async fn execute(&self, request: BackendRequest) -> Result<BackendResult, BackendError> {
        match request {
            BackendRequest::UpdateBrowserContext { context } => {
                crate::app::browser_context_rpc::update(context).map_err(Self::invalid)?;
                Ok(BackendResult::response(BackendResponse::Ok))
            }
            BackendRequest::ClearBrowserContext => {
                crate::app::browser_context_rpc::clear();
                Ok(BackendResult::response(BackendResponse::Ok))
            }
            BackendRequest::AddUri {
                uris,
                options,
                position,
            } => self.add_uri(uris, options, position).await,
            BackendRequest::AddTorrent {
                data,
                additional_uris,
                options,
                position,
            } => {
                self.add_torrent(data, additional_uris, options, position)
                    .await
            }
            BackendRequest::AddMetalink {
                data,
                options,
                position,
            } => self.add_metalink(data, options, position).await,
            BackendRequest::Remove { gid } => self.remove(gid, false),
            BackendRequest::ForceRemove { gids } => {
                let mut last = String::new();
                let mut events = Vec::with_capacity(gids.len());
                for gid in gids {
                    last = gid.clone();
                    let result = self.remove(gid, true)?;
                    events.extend(result.events);
                }
                Ok(BackendResult::with_events(
                    BackendResponse::Gid(last),
                    events,
                ))
            }
            BackendRequest::Pause { gid } => self.pause(gid, false),
            BackendRequest::ForcePause { gid } => self.pause(gid, true),
            BackendRequest::Unpause { gid } => self.unpause(gid),
            BackendRequest::TellStatus { gid, .. } => self.tell_status(gid),
            BackendRequest::TellActive { .. } => self.tell_active(Vec::new()),
            BackendRequest::TellWaiting { offset, num, .. } => {
                self.tell_waiting(offset, num, Vec::new())
            }
            BackendRequest::TellStopped { offset, num, .. } => {
                self.tell_stopped(offset, num, Vec::new())
            }
            BackendRequest::GetGlobalStat => {
                let snapshot = self.capture_snapshot();
                Ok(BackendResult::response(BackendResponse::GlobalStat(
                    snapshot.global_stat,
                )))
            }
            BackendRequest::GetUris { gid } => self.get_uris(gid),
            BackendRequest::GetFiles { gid } => self.get_files(gid),
            BackendRequest::GetServers { gid } => self.get_servers(gid),
            BackendRequest::PurgeDownloadResult => {
                self.group_man.purge_stopped_results();
                Ok(BackendResult::response(BackendResponse::Text("OK".into())))
            }
            BackendRequest::RemoveDownloadResult { gid } => {
                if self.group_man.remove_stopped_result(&gid).is_none() {
                    return Err(Self::execution(format!(
                        "GID {gid} not found in download results"
                    )));
                }
                Ok(BackendResult::response(BackendResponse::Text("OK".into())))
            }
            BackendRequest::GetGlobalOption => {
                let options = self.global_options().await;
                let options =
                    OptionRegistry::new().project_defined_global_options_for_rpc(&options);
                Ok(BackendResult::response(BackendResponse::Options(options)))
            }
            BackendRequest::ChangeGlobalOption { options } => {
                self.change_global_option(options).await
            }
            BackendRequest::GetOption { gid } => self.get_option(gid).await,
            BackendRequest::ChangeOption { gid, options } => self.change_option(gid, options),
            BackendRequest::GetPeers { gid } => self.get_peers(gid),
            #[cfg(feature = "bittorrent")]
            BackendRequest::GetTrackers { gid } => self.get_trackers(gid),
            #[cfg(not(feature = "bittorrent"))]
            BackendRequest::GetTrackers { .. } => {
                Err(BackendError::Unsupported("BitTorrent is disabled".into()))
            }
            #[cfg(feature = "bittorrent")]
            BackendRequest::GetDhtStatus => self.get_dht_status().await,
            #[cfg(not(feature = "bittorrent"))]
            BackendRequest::GetDhtStatus => {
                Err(BackendError::Unsupported("BitTorrent is disabled".into()))
            }
            BackendRequest::PauseAll => {
                let gids = self.lifecycle_gids();
                self.group_man.pause_all();
                self.send(EngineCommand::PauseAll)?;
                Ok(BackendResult::with_events(
                    BackendResponse::Text("OK".into()),
                    gids.into_iter().map(BackendEvent::DownloadPause).collect(),
                ))
            }
            BackendRequest::ForcePauseAll => {
                let gids = self.lifecycle_gids();
                self.group_man.force_pause_all();
                self.send(EngineCommand::ForcePauseAll)?;
                Ok(BackendResult::with_events(
                    BackendResponse::Text("OK".into()),
                    gids.into_iter().map(BackendEvent::DownloadPause).collect(),
                ))
            }
            BackendRequest::UnpauseAll => {
                let gids = self.lifecycle_gids();
                self.group_man.unpause_all();
                self.send(EngineCommand::UnpauseAll)?;
                Ok(BackendResult::with_events(
                    BackendResponse::Text("OK".into()),
                    gids.into_iter().map(BackendEvent::DownloadStart).collect(),
                ))
            }
            BackendRequest::ChangeUri {
                gid,
                file_index,
                delete_uris,
                add_uris,
                position,
            } => {
                let group = self.group(&gid)?;
                let result = group
                    .write()
                    .map_err(|_| BackendError::Internal("Failed to lock request group".into()))?
                    .change_uris(file_index, &delete_uris, &add_uris, position)
                    .map_err(|error| Self::execution(error.to_string()))?;
                Ok(BackendResult::response(BackendResponse::Counts([
                    result.0, result.1,
                ])))
            }
            BackendRequest::SaveSession => {
                let path = self
                    .save_session_path
                    .clone()
                    .ok_or_else(|| Self::execution("Filename is not given. Set --save-session."))?;
                let mut command = SaveSessionCommand::new(path, Arc::clone(&self.group_man));
                command.execute().await.map_err(|error| {
                    BackendError::Internal(format!("Failed to save session: {error}"))
                })?;
                Ok(BackendResult::response(BackendResponse::Text("OK".into())))
            }
            BackendRequest::ChangePosition {
                gid,
                position,
                mode,
            } => self.change_position(&gid, position, mode),
            BackendRequest::Shutdown { force } => {
                let count = self.group_man.count();
                if force {
                    self.group_man.force_remove_reserved();
                }
                aria2_core::engine::halt_watchers::spawn_timed_halt(
                    self.engine_cmd_tx.clone(),
                    if force {
                        std::time::Duration::ZERO
                    } else {
                        RPC_SHUTDOWN_GRACE
                    },
                    force,
                );
                let text = if force {
                    format!("OK. {count} downloads forcibly terminated.")
                } else {
                    format!("OK. {count} active downloads paused.")
                };
                Ok(BackendResult::response(BackendResponse::Text(text)))
            }
        }
    }

    async fn capture_read_snapshot(
        &self,
    ) -> Result<Option<Arc<BackendReadSnapshot>>, BackendError> {
        Ok(Some(Arc::new(self.capture_snapshot())))
    }

    async fn execute_with_snapshot(
        &self,
        request: BackendRequest,
        snapshot: Option<Arc<BackendReadSnapshot>>,
    ) -> Result<BackendResult, BackendError> {
        match (&request, snapshot) {
            (BackendRequest::TellActive { .. }, Some(snapshot)) => Ok(BackendResult::response(
                BackendResponse::Statuses(snapshot.active.clone()),
            )),
            (BackendRequest::TellWaiting { offset, num, .. }, Some(snapshot)) => {
                Ok(BackendResult::response(BackendResponse::Statuses(
                    paginate(snapshot.waiting.clone(), *offset, *num),
                )))
            }
            (BackendRequest::TellStopped { offset, num, .. }, Some(snapshot)) => {
                Ok(BackendResult::response(BackendResponse::Statuses(
                    paginate(snapshot.stopped.clone(), *offset, *num),
                )))
            }
            (BackendRequest::GetGlobalStat, Some(snapshot)) => Ok(BackendResult::response(
                BackendResponse::GlobalStat(snapshot.global_stat.clone()),
            )),
            _ => self.execute(request).await,
        }
    }
}

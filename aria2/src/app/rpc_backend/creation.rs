use std::collections::HashMap;
#[cfg(any(feature = "bittorrent", feature = "metalink"))]
use std::sync::Arc;

use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
#[cfg(feature = "metalink")]
use aria2_core::util::rwlock_ext::RwLockRecover;
use aria2_rpc::{BackendError, BackendEvent, BackendResponse, BackendResult, PositionMode};

use super::CoreRpcBackend;

impl CoreRpcBackend {
    pub(super) fn add_group(
        &self,
        gid: GroupId,
        uris: Vec<String>,
        options: DownloadOptions,
        option_snapshot: HashMap<String, serde_json::Value>,
        torrent_data: Option<Vec<u8>>,
    ) -> Result<String, BackendError> {
        self.group_man
            .add_group_with_gid(gid, uris, options)
            .map_err(|error| Self::execution(format!("Failed to add group: {error}")))?;
        let group = self
            .group_man
            .group_by_id(gid)
            .ok_or_else(|| BackendError::Internal("Group not found after insert".into()))?;
        #[cfg(feature = "bittorrent")]
        if let Some(data) = torrent_data.as_deref() {
            let options = group
                .read()
                .map_err(|_| BackendError::Internal("Failed to lock request group".into()))?
                .options()
                .clone();
            if let Err(error) = aria2_core::engine::bt_download_command::prepare_group_metadata(
                Arc::clone(&group),
                data,
                &options,
                options.dir.as_deref(),
            ) {
                let _ = self.group_man.remove_group_by_id(gid);
                return Err(Self::invalid(error.to_string()));
            }
        }
        {
            let mut group = group
                .write()
                .map_err(|_| BackendError::Internal("Failed to lock request group".into()))?;
            group.set_option_snapshot(option_snapshot);
            #[cfg(feature = "bittorrent")]
            if let Some(data) = torrent_data {
                group.set_bt_metadata_data(data);
            }
        }
        self.send(EngineCommand::AddDownload { group })?;
        Ok(gid.to_hex_string())
    }

    pub(super) async fn add_uri(
        &self,
        uris: Vec<String>,
        options: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    ) -> Result<BackendResult, BackendError> {
        let (download_options, snapshot) = self.merged_task_options(options).await?;
        let gid = self.group_man.next_available_gid();
        let gid_hex = self.add_group(gid, uris, download_options, snapshot, None)?;
        if let Some(position) = position {
            self.change_position(&gid_hex, position as i32, PositionMode::SetFromStart)?;
        }
        Ok(BackendResult::with_events(
            BackendResponse::Gid(gid_hex.clone()),
            vec![BackendEvent::DownloadStart(gid_hex)],
        ))
    }

    pub(super) async fn add_torrent(
        &self,
        data: Vec<u8>,
        additional_uris: Vec<String>,
        options: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    ) -> Result<BackendResult, BackendError> {
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = (data, additional_uris, options, position);
            return Err(BackendError::Unsupported(
                "BitTorrent is not enabled".into(),
            ));
        }

        #[cfg(feature = "bittorrent")]
        {
            Self::validate_torrent_data(&data)?;
            let (download_options, snapshot) = self.merged_task_options(options).await?;
            let gid = self.group_man.next_available_gid();
            let mut uris = Vec::with_capacity(1 + additional_uris.len());
            uris.push(format!("bt://{}", gid.to_hex_string()));
            uris.extend(additional_uris);
            let gid_hex = self.add_group(gid, uris, download_options, snapshot, Some(data))?;
            if let Some(position) = position {
                self.change_position(&gid_hex, position as i32, PositionMode::SetFromStart)?;
            }
            Ok(BackendResult::with_events(
                BackendResponse::Gid(gid_hex.clone()),
                vec![BackendEvent::DownloadStart(gid_hex)],
            ))
        }
    }

    pub(super) async fn add_metalink(
        &self,
        data: Vec<u8>,
        options: HashMap<String, serde_json::Value>,
        position: Option<usize>,
    ) -> Result<BackendResult, BackendError> {
        #[cfg(not(feature = "metalink"))]
        {
            let _ = (data, options, position);
            Err(BackendError::Unsupported("Metalink is not enabled".into()))
        }

        #[cfg(feature = "metalink")]
        {
            let (download_options, snapshot) = self.merged_task_options(options).await?;
            let converter =
                aria2_core::engine::metalink_to_request_group::MetalinkToRequestGroup::new();
            let mut gids = std::iter::from_fn(|| Some(self.group_man.next_available_gid()));
            let resource_groups = converter
                .create_resource_groups_from_bytes(&data, &download_options, &mut gids)
                .map_err(|error| Self::invalid(error.to_string()))?;
            let mut response_gids = Vec::new();
            let mut start_gids = Vec::new();
            for group in resource_groups {
                let gid = group.recover().gid();
                group.recover_mut().set_option_snapshot(snapshot.clone());
                let wake_group = Arc::clone(&group);
                self.group_man.add_group_arc(group);
                self.send(EngineCommand::AddDownload { group: wake_group })?;
                let gid = gid.to_hex_string();
                response_gids.push(gid.clone());
                start_gids.push(gid);
            }

            #[cfg(all(feature = "metalink", feature = "bittorrent"))]
            {
                let mut graph_gids =
                    std::iter::from_fn(|| Some(self.group_man.next_available_gid()));
                let graphs = converter
                    .create_torrent_graphs_from_bytes(&data, &download_options, &mut graph_gids)
                    .map_err(|error| Self::invalid(error.to_string()))?;
                for graph in graphs {
                    let metadata_gid = graph.metadata.recover().gid();
                    let payload_gid = graph.payload.recover().gid();
                    graph
                        .metadata
                        .recover_mut()
                        .set_option_snapshot(snapshot.clone());
                    graph
                        .payload
                        .recover_mut()
                        .set_option_snapshot(snapshot.clone());
                    let metadata_group = Arc::clone(&graph.metadata);
                    let payload_group = Arc::clone(&graph.payload);
                    self.group_man
                        .add_metalink_graph(graph)
                        .map_err(|error| Self::execution(error.to_string()))?;
                    // The manager insertion above makes the groups visible to
                    // RPC reads immediately. These idempotent commands wake a
                    // running engine so it promotes the newly inserted queue.
                    self.send(EngineCommand::AddDownload {
                        group: metadata_group,
                    })?;
                    self.send(EngineCommand::AddDownload {
                        group: payload_group,
                    })?;
                    let metadata_gid = metadata_gid.to_hex_string();
                    let payload_gid = payload_gid.to_hex_string();
                    response_gids.extend([metadata_gid.clone(), payload_gid.clone()]);
                    start_gids.extend([metadata_gid, payload_gid]);
                }
            }

            if let Some(position) = position
                && let Some(gid) = response_gids.first()
            {
                self.change_position(gid, position as i32, PositionMode::SetFromStart)?;
            }
            Ok(BackendResult::with_events(
                BackendResponse::Gids(response_gids),
                start_gids
                    .into_iter()
                    .map(BackendEvent::DownloadStart)
                    .collect(),
            ))
        }
    }
}

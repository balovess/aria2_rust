//! Grouped multi-file Metalink and torrent-metaurl execution.

use tracing::{info, warn};

use super::MetalinkDownloadCommand;
use crate::engine::command::Command;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
#[cfg(feature = "bittorrent")]
use crate::request::request_group::MetadataInfo;
use crate::util::rwlock_ext::RwLockRecover;

impl MetalinkDownloadCommand {
    pub(super) async fn execute_grouped(&mut self) -> Result<()> {
        let grouped_files = std::mem::take(&mut self.grouped_file_infos);
        let mut completed_bytes = 0u64;
        let mut direct_failed = false;

        for (path, info) in &grouped_files {
            let mut command = Self {
                group: std::sync::Arc::clone(&self.group),
                client: self.client.clone(),
                output_path: path.clone(),
                started: true,
                completed: false,
                completed_bytes: 0,
                metalink_data: Vec::new(),
                file_info: Some(info.clone()),
                grouped_file_infos: Vec::new(),
                checkpoint: None,
                global_limiter: self.global_limiter.clone(),
                #[cfg(feature = "bittorrent")]
                public_tracker_catalog: self.public_tracker_catalog.clone(),
                #[cfg(feature = "bittorrent")]
                bt_registry: self.bt_registry.clone(),
                #[cfg(feature = "bittorrent")]
                bt_listener: self.bt_listener.clone(),
                #[cfg(feature = "bittorrent")]
                lpd_manager: self.lpd_manager.clone(),
            };
            match command.execute_file(false, false).await {
                Ok(()) => {
                    completed_bytes = completed_bytes.saturating_add(command.completed_bytes);
                }
                Err(error) => {
                    direct_failed = true;
                    warn!(
                        path = %command.output_path.display(),
                        error = %error,
                        "Shared Metalink direct mirror failed"
                    );
                }
            }
        }

        if direct_failed && self.lifecycle_error().is_none() {
            #[cfg(feature = "bittorrent")]
            {
                warn!("At least one shared Metalink mirror failed, using one torrent fallback");
                return self.try_torrent_metaurl_group(&grouped_files).await;
            }
            #[cfg(not(feature = "bittorrent"))]
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Shared Metalink mirrors failed and BitTorrent support is disabled".into(),
            )));
        }

        self.completed_bytes = completed_bytes;
        self.completed = true;
        let mut group = self.group.recover_mut();
        group.update_progress(completed_bytes);
        group.set_completed_length(completed_bytes);
        group.complete()?;
        Ok(())
    }

    /// Download a `.torrent` from the given metaurls (by priority) and run a
    /// BitTorrent download for it. Mirrors C++ `BtDependency` which resolves
    /// `metaurl mediatype="application/x-bittorrent"` entries.
    #[cfg(feature = "bittorrent")]
    pub(crate) async fn try_torrent_metaurl(
        &mut self,
        meta_urls: &[aria2_protocol::metalink::parser::MetaUrlEntry],
    ) -> Result<()> {
        use aria2_protocol::metalink::parser::MediaType;

        let mut last_err: Option<Aria2Error> = None;
        for mu in meta_urls
            .iter()
            .filter(|m| m.mediatype == MediaType::Torrent)
        {
            info!(url = %mu.url, "Downloading torrent from Metalink metaurl");
            match self.download_metadata_url_with_retry(&mu.url).await {
                Ok(torrent_bytes) => {
                    // Persist metadata beside the payload so the dependency
                    // can be reconstructed by the manager and after restart.
                    let metadata_path = self.output_path.with_extension("torrent");
                    tokio::fs::write(&metadata_path, &torrent_bytes)
                        .await
                        .map_err(|error| {
                            Aria2Error::FileIo(format!(
                                "Failed to persist torrent metadata '{}': {error}",
                                metadata_path.display()
                            ))
                        })?;

                    let options = self.group.recover().options().clone();
                    let gid = self.group.recover().gid();
                    let dir = self.output_path.parent().and_then(|p| p.to_str());
                    {
                        let group = self.group.recover_mut();
                        group.set_metadata_info(
                            MetadataInfo::new(gid, &mu.url)
                                .with_metadata_path(metadata_path.to_string_lossy()),
                        );
                    }
                    let mut bt_cmd =
                        crate::engine::bt_download_command::BtDownloadCommand::new_with_group(
                            std::sync::Arc::clone(&self.group),
                            &torrent_bytes,
                            &options,
                            dir,
                        )?;
                    if let Some(gl) = self.global_limiter.clone() {
                        bt_cmd.set_global_limiter(gl);
                    }
                    #[cfg(feature = "bittorrent")]
                    if let Some(catalog) = self.public_tracker_catalog.clone() {
                        bt_cmd.set_public_tracker_catalog(catalog);
                    }
                    #[cfg(feature = "bittorrent")]
                    if let Some(registry) = self.bt_registry.clone() {
                        bt_cmd.set_bt_registry(registry);
                    }
                    #[cfg(feature = "bittorrent")]
                    if let Some(listener) = self.bt_listener.clone() {
                        bt_cmd.set_bt_listener(listener);
                    }
                    #[cfg(feature = "bittorrent")]
                    if let Some(manager) = self.lpd_manager.clone() {
                        bt_cmd.set_lpd_manager(manager);
                    }
                    bt_cmd.execute().await?;
                    self.completed_bytes = self.group.recover().total_length();
                    {
                        let mut group = self.group.recover_mut();
                        group.update_progress(self.completed_bytes);
                        group.complete()?;
                    }
                    self.completed = true;
                    info!(
                        "Metalink torrent metaurl download done: {}",
                        self.output_path.display()
                    );
                    return Ok(());
                }
                Err(e) => {
                    warn!(url = %mu.url, error = %e, "Torrent metaurl failed");
                    if matches!(
                        e,
                        Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
                    ) || self.should_stop_after_not_found(&e)
                    {
                        return Err(e);
                    }
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            Aria2Error::Fatal(FatalError::Config("All torrent metaurls failed".into()))
        }))
    }

    /// Resolve one torrent metaurl for every file in a shared Metalink group.
    ///
    /// The torrent is parsed once and the selected Metalink paths/mirrors are
    /// applied to one BitTorrent context. This preserves multi-file offsets
    /// and avoids the per-file metadata loss caused by independent fallback.
    #[cfg(feature = "bittorrent")]
    async fn try_torrent_metaurl_group(
        &mut self,
        grouped_files: &[(std::path::PathBuf, super::super::types::FileDownloadInfo)],
    ) -> Result<()> {
        use crate::request::request_group::BtFileMapping;
        use aria2_protocol::metalink::parser::MediaType;

        let mut meta_urls = Vec::new();
        for (_, info) in grouped_files {
            for metaurl in &info.torrent_metaurls {
                if metaurl.mediatype == MediaType::Torrent
                    && !meta_urls
                        .iter()
                        .any(|candidate: &String| candidate == &metaurl.url)
                {
                    meta_urls.push(metaurl.url.clone());
                }
            }
        }
        if meta_urls.is_empty() {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Shared Metalink group has no torrent metaurl".into(),
            )));
        }

        let mappings = grouped_files
            .iter()
            .map(|(path, info)| BtFileMapping {
                original_name: info
                    .torrent_metaurls
                    .iter()
                    .find(|metaurl| metaurl.mediatype == MediaType::Torrent)
                    .and_then(|metaurl| metaurl.name.clone())
                    .unwrap_or_default(),
                path: path.to_string_lossy().into_owned(),
                uris: info
                    .sorted_urls
                    .iter()
                    .filter(|url| url.is_non_p2p())
                    .map(|url| url.url.clone())
                    .collect(),
                max_connection_per_server: self
                    .group
                    .recover()
                    .options()
                    .max_connection_per_server
                    .unwrap_or(crate::constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
                    .clamp(1, 16) as usize,
                unique_protocol: self
                    .group
                    .recover()
                    .options()
                    .metalink_enable_unique_protocol,
            })
            .collect::<Vec<_>>();

        let mut last_err = None;
        for metadata_uri in meta_urls {
            info!(url = %metadata_uri, "Downloading shared torrent from Metalink metaurl");
            match self.download_metadata_url_with_retry(&metadata_uri).await {
                Ok(torrent_bytes) => {
                    let metadata_path = self.output_path.with_extension("torrent");
                    tokio::fs::write(&metadata_path, &torrent_bytes)
                        .await
                        .map_err(|error| {
                            Aria2Error::FileIo(format!(
                                "Failed to persist torrent metadata '{}': {error}",
                                metadata_path.display()
                            ))
                        })?;

                    let options = self.group.recover().options().clone();
                    let gid = self.group.recover().gid();
                    let dir = self.output_path.parent().and_then(|path| path.to_str());
                    self.group.recover_mut().set_metadata_info(
                        MetadataInfo::new(gid, &metadata_uri)
                            .with_metadata_path(metadata_path.to_string_lossy()),
                    );

                    let mut bt_cmd = crate::engine::bt_download_command::BtDownloadCommand::new_with_group_and_mappings(
                        std::sync::Arc::clone(&self.group),
                        &torrent_bytes,
                        &options,
                        dir,
                        &mappings,
                    )?;
                    if let Some(global_limiter) = self.global_limiter.clone() {
                        bt_cmd.set_global_limiter(global_limiter);
                    }
                    #[cfg(feature = "bittorrent")]
                    if let Some(catalog) = self.public_tracker_catalog.clone() {
                        bt_cmd.set_public_tracker_catalog(catalog);
                    }
                    #[cfg(feature = "bittorrent")]
                    if let Some(registry) = self.bt_registry.clone() {
                        bt_cmd.set_bt_registry(registry);
                    }
                    #[cfg(feature = "bittorrent")]
                    if let Some(listener) = self.bt_listener.clone() {
                        bt_cmd.set_bt_listener(listener);
                    }
                    #[cfg(feature = "bittorrent")]
                    if let Some(manager) = self.lpd_manager.clone() {
                        bt_cmd.set_lpd_manager(manager);
                    }
                    bt_cmd.execute().await?;
                    self.completed_bytes = self.group.recover().completed_length();
                    self.completed = true;
                    info!(
                        path = %self.output_path.display(),
                        bytes = self.completed_bytes,
                        "Shared Metalink torrent fallback completed"
                    );
                    return Ok(());
                }
                Err(error) => {
                    warn!(url = %metadata_uri, error = %error, "Shared torrent metaurl failed");
                    if matches!(
                        error,
                        Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
                    ) || self.should_stop_after_not_found(&error)
                    {
                        return Err(error);
                    }
                    last_err = Some(error);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            Aria2Error::Fatal(FatalError::Config(
                "All shared torrent metaurls failed".into(),
            ))
        }))
    }
}

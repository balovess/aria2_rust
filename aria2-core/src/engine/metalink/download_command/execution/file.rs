//! Single-file Metalink download execution.

use tracing::{debug, info, warn};

use super::MetalinkDownloadCommand;
use crate::engine::active_output_registry::global_registry;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::util::rwlock_ext::RwLockRecover;

impl MetalinkDownloadCommand {
    pub(super) async fn execute_file(
        &mut self,
        complete_group: bool,
        allow_torrent_fallback: bool,
    ) -> Result<()> {
        #[cfg(not(feature = "bittorrent"))]
        let _ = allow_torrent_fallback;

        // Resolve file info: either from pre-parsed file_info (multi-file mode)
        // or by re-parsing the raw metalink_data (single-file mode).
        // We extract owned data to avoid lifetime/borrow issues.
        let sorted_urls_owned: Vec<aria2_protocol::metalink::parser::UrlEntry>;
        let expected_size: Option<u64>;
        let hash_entry_owned: Option<aria2_protocol::metalink::parser::HashEntry>;
        let pieces_owned: Option<aria2_protocol::metalink::parser::PieceInfo>;
        let torrent_metaurls_owned: Vec<aria2_protocol::metalink::parser::MetaUrlEntry>;

        match &self.file_info {
            Some(info) => {
                sorted_urls_owned = info.sorted_urls.clone();
                expected_size = info.expected_size;
                hash_entry_owned = info.hash_entry.clone();
                pieces_owned = info.pieces.clone();
                torrent_metaurls_owned = info.torrent_metaurls.clone();
            }
            None => {
                let doc = aria2_protocol::metalink::parser::MetalinkDocument::parse(
                    &self.metalink_data,
                    None,
                )
                .map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!("Metalink parse error: {}", e)))
                })?;

                let file = if doc.files.len() == 1 {
                    &doc.files[0]
                } else {
                    // Multi-file Metalink in single-file mode: use first file
                    &doc.files[0]
                };

                sorted_urls_owned = file
                    .get_sorted_urls()
                    .iter()
                    .map(|u| (*u).clone())
                    .collect();
                expected_size = file.size;
                hash_entry_owned = file.strongest_hash().cloned();
                pieces_owned = file.pieces.clone();
                torrent_metaurls_owned = file
                    .meta_urls
                    .iter()
                    .filter(|m| m.mediatype == aria2_protocol::metalink::parser::MediaType::Torrent)
                    .cloned()
                    .collect();

                if sorted_urls_owned.is_empty() && torrent_metaurls_owned.is_empty() {
                    return Err(Aria2Error::Fatal(FatalError::Config(
                        "No download mirrors available".into(),
                    )));
                }
            }
        }

        if sorted_urls_owned.is_empty() {
            // No HTTP/FTP mirrors, but a torrent metaurl is present: fall
            // straight through to the BitTorrent dependency path.
            if torrent_metaurls_owned.is_empty() {
                return Err(Aria2Error::Fatal(FatalError::Config(
                    "No download mirrors available".into(),
                )));
            }
            #[cfg(feature = "bittorrent")]
            if allow_torrent_fallback {
                return self.try_torrent_metaurl(&torrent_metaurls_owned).await;
            }
            #[cfg(not(feature = "bittorrent"))]
            return Err(Aria2Error::Fatal(FatalError::Config(
                "No download mirrors available".into(),
            )));
        }

        if let Some(parent) = self.output_path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
            })?;
        }

        // Resolve filename collision against other active downloads.
        // If another task is already writing to self.output_path, a unique
        // name such as "file (1).ext" will be generated automatically.
        let resolved_output_path = global_registry().resolve(&self.output_path).await;

        let mut last_error = None;

        for url_entry in &sorted_urls_owned {
            debug!(
                "Trying mirror [priority={}] : {}",
                url_entry.priority, url_entry.url
            );

            match self
                .download_payload_with_retry(&resolved_output_path, &url_entry.url, expected_size)
                .await
            {
                Ok(payload) => {
                    let hash_valid = match hash_entry_owned.as_ref() {
                        Some(hash) => match self
                            .verify_file_hash(&payload.path, payload.total_length, hash)
                            .await
                        {
                            Ok(valid) => valid,
                            Err(error) => {
                                self.discard_checkpoint(&payload.path).await;
                                global_registry().release(&resolved_output_path).await;
                                return Err(error);
                            }
                        },
                        None => true,
                    };
                    if !hash_valid {
                        let hash = hash_entry_owned
                            .as_ref()
                            .expect("hash validation requires a hash entry");
                        warn!(
                            "Hash verification failed [{}]: trying next mirror",
                            hash.algo.as_standard_name()
                        );
                        self.discard_checkpoint(&payload.path).await;
                        last_error = Some(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: format!(
                                    "Hash verification failed: {}",
                                    hash.algo.as_standard_name()
                                ),
                            },
                        ));
                        continue;
                    }

                    // Chunk-level verification (<pieces>) — mirrors C++
                    // `MetalinkEntry::chunkChecksum` checking after download.
                    let pieces_valid = match pieces_owned.as_ref() {
                        Some(pieces) => {
                            match self.verify_pieces_file(&payload.path, pieces).await {
                                Ok(valid) => valid,
                                Err(error) => {
                                    self.discard_checkpoint(&payload.path).await;
                                    global_registry().release(&resolved_output_path).await;
                                    return Err(error);
                                }
                            }
                        }
                        None => true,
                    };
                    if !pieces_valid {
                        warn!("Chunk hash verification failed: trying next mirror");
                        self.discard_checkpoint(&payload.path).await;
                        last_error = Some(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: "Chunk hash verification failed".to_string(),
                            },
                        ));
                        continue;
                    }

                    self.complete_checkpoint().await;
                    self.completed_bytes = payload.completed_length;

                    {
                        let g = self.group.recover();
                        if payload.total_length > 0 {
                            g.set_total_length(payload.total_length);
                        }
                        g.update_progress(self.completed_bytes);
                        g.update_speed(self.completed_bytes, 0);
                        drop(g);
                        if complete_group {
                            let mut g = self.group.recover_mut();
                            g.complete()?;
                        }
                    }

                    info!(
                        "Metalink download done: {} ({} bytes from {})",
                        resolved_output_path.display(),
                        self.completed_bytes,
                        url_entry.url
                    );
                    self.completed = true;
                    global_registry().release(&resolved_output_path).await;
                    return Ok(());
                }
                Err(e) => {
                    warn!("Mirror download failed {}: {}", url_entry.url, e);
                    if self.lifecycle_error().is_some() {
                        global_registry().release(&resolved_output_path).await;
                        return Err(e);
                    }
                    if matches!(
                        e,
                        Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
                    ) || self.should_stop_after_not_found(&e)
                    {
                        global_registry().release(&resolved_output_path).await;
                        return Err(e);
                    }
                    last_error = Some(e);
                }
            }
        }

        global_registry().release(&resolved_output_path).await;

        // All HTTP/FTP mirrors failed: fall back to the BitTorrent metaurl
        // dependency (mirrors C++ BtDependency resolving a torrent metaurl
        // when no direct resource can be downloaded).
        #[cfg(feature = "bittorrent")]
        if allow_torrent_fallback && !torrent_metaurls_owned.is_empty() {
            warn!("All HTTP mirrors failed, falling back to torrent metaurl");
            self.discard_checkpoint(&resolved_output_path).await;
            return self.try_torrent_metaurl(&torrent_metaurls_owned).await;
        }

        Err(last_error
            .unwrap_or_else(|| Aria2Error::Fatal(FatalError::Config("All mirrors failed".into()))))
    }
}

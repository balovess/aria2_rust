//! Session management for download persistence
//!
//! This module handles saving and restoring download sessions:
//! - Restoring incomplete downloads from session files
//! - Saving session state on shutdown
//! - Mapping session entries to download options

use super::App;
use aria2_core::config::project_initial_options;
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use aria2_core::request::request_group::{BtDependency, BtFileMapping, MetadataInfo, RequestGroup};
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::session::active_session::ActiveSessionManager;
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use aria2_core::session::session_entry::SessionEntry;
use aria2_core::util::rwlock_ext::RwLockRecover;
use std::path::PathBuf;
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use std::sync::{Arc, RwLock};
use tracing::{debug, info, warn};

impl App {
    /// Restore incomplete download tasks from a session file.
    ///
    /// This method is called at startup to resume downloads from the
    /// --input-file session file.
    ///
    /// # Restore Logic
    /// 1. Skip entries with status "complete"
    /// 2. Skip entries with both completed_length and total_length as 0
    /// 3. Recreate download tasks for entries with progress
    /// 4. BT download bitfield info is preserved for later use
    ///
    /// # Returns
    /// - `Ok(usize)`: Number of successfully restored tasks
    /// - `Err(String)`: Error during restoration
    pub async fn restore_session(&self) -> std::result::Result<usize, String> {
        let input_file = match self.get_opt_str("input-file").await {
            Some(path) => path,
            None => return Ok(0), // No input-file specified
        };

        let session_path = PathBuf::from(&input_file);
        if !session_path.exists() {
            info!(
                "Session file does not exist, skipping restore: {}",
                input_file
            );
            return Ok(0);
        }

        if !Self::looks_like_session_file(&input_file) {
            return Ok(0);
        }

        info!("Restoring download tasks from session file: {}", input_file);

        let mgr = ActiveSessionManager::new(session_path.clone());

        let entries = match mgr.load_session().await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("Failed to load session file: {}", e);
                return Err(e);
            }
        };

        if entries.is_empty() {
            info!("Session file is empty or has no recoverable entries");
            return Ok(0);
        }

        let mut restored_count = 0;

        for entry in &entries {
            // Skip completed entries
            if entry.status == "complete" {
                debug!("Skipping completed entry: GID={:x}", entry.gid);
                continue;
            }

            // C++ restores ALL non-finished entries, even those with 0/0 progress
            // (newly added but never started). Only skip entries that have
            // explicitly been marked as "removed" — they should not be restored.
            if entry.status == "removed" {
                debug!("Skipping removed entry: GID={:x}", entry.gid);
                continue;
            }

            // Keep the persisted request options separate from Rust-only
            // session metadata before reconstructing execution settings.
            let option_snapshot = Self::session_option_snapshot(&entry.options);
            let opts = Self::map_entry_to_download_options(&entry.options);

            #[cfg(all(feature = "metalink", feature = "bittorrent"))]
            if let Some(payload_gid) = standard_graph_payload_gid(entry) {
                match self
                    .restore_standard_metalink_graph(
                        entry,
                        payload_gid,
                        opts.clone(),
                        option_snapshot.clone(),
                    )
                    .await
                {
                    Ok(count) => {
                        restored_count += count;
                        continue;
                    }
                    Err(error) => {
                        warn!(
                            gid = %entry.gid,
                            error = %error,
                            "Failed to restore Metalink graph entry; falling back to plain session task"
                        );
                    }
                }
            }

            info!(
                "Restoring download task: GID={:x}, URIs={:?}, progress={}/{}",
                entry.gid, entry.uris, entry.completed_length, entry.total_length
            );

            // Add group through RequestGroupMan
            {
                let man = &self.request_man;
                let gid = GroupId::new(entry.gid);
                match man.add_group_with_gid(gid, entry.uris.clone(), opts) {
                    Ok(()) => {
                        restored_count += 1;
                        info!("Successfully restored task #{}", gid.value());

                        // Store BT bitfield if present
                        if let Some(group_lock) = man.get_group(gid) {
                            let mut group = group_lock.recover_mut();
                            group.set_option_snapshot(option_snapshot.clone());
                            if entry.bitfield.is_some() {
                                *group.bt_bitfield.recover_mut() = entry.bitfield.clone();
                                debug!(
                                    "Set BT bitfield for GID={}, bits={}",
                                    gid.value(),
                                    entry.bitfield.as_ref().map(|b| b.len()).unwrap_or(0)
                                );
                            }
                            group.update_progress(entry.completed_length);
                            group.set_total_length(entry.total_length);
                        }
                    }
                    Err(e) => {
                        warn!("Failed to restore task (GID={:x}): {}", entry.gid, e);
                    }
                }
            }
        }

        info!(
            "Session restore complete: {} entries total, {} tasks restored",
            entries.len(),
            restored_count
        );
        Ok(restored_count)
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    async fn restore_standard_metalink_graph(
        &self,
        entry: &SessionEntry,
        payload_gid: GroupId,
        payload_options: DownloadOptions,
        option_snapshot: std::collections::HashMap<String, serde_json::Value>,
    ) -> std::result::Result<usize, String> {
        let metadata_gid = GroupId::new(entry.gid);
        if metadata_gid == payload_gid {
            return Err("metadata and payload GIDs must differ".to_string());
        }
        let metadata_uri = entry
            .options
            .get("aria2-rust-metadata-uri")
            .cloned()
            .or_else(|| entry.uris.first().cloned())
            .filter(|uri| !uri.is_empty())
            .ok_or_else(|| "Metalink graph entry has no metadata URI".to_string())?;

        let metadata_path = entry
            .options
            .get("aria2-rust-metadata-path")
            .cloned()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let name = metadata_uri
                    .rsplit('/')
                    .next()
                    .filter(|name| !name.is_empty())
                    .unwrap_or("metadata.torrent");
                PathBuf::from(payload_options.dir.as_deref().unwrap_or(".")).join(name)
            });
        let output_name = entry
            .options
            .get("aria2-rust-output-name")
            .cloned()
            .or_else(|| payload_options.out.clone())
            .ok_or_else(|| "Metalink graph entry has no payload output name".to_string())?;
        let payload_path =
            PathBuf::from(payload_options.dir.as_deref().unwrap_or(".")).join(&output_name);
        let metadata_info = MetadataInfo::new(metadata_gid, &metadata_uri)
            .with_metadata_path(metadata_path.to_string_lossy());

        let fallback_uris =
            decode_session_uris(entry.options.get("aria2-rust-fallback-uris")).unwrap_or_default();
        let file_mappings = decode_session_mappings(entry.options.get("aria2-rust-file-mappings"))
            .unwrap_or_default();
        let memory_source = entry
            .options
            .get("aria2-rust-metadata-memory")
            .is_some_and(|value| value == "true");

        let mut metadata_options = payload_options.clone();
        metadata_options.follow_torrent =
            Some(aria2_core::request::request_group::FollowMode::Disabled);
        metadata_options.follow_metalink =
            Some(aria2_core::request::request_group::FollowMode::Disabled);
        metadata_options.dir = metadata_path
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned());
        metadata_options.out = metadata_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let metadata = Arc::new(RwLock::new(RequestGroup::new(
            metadata_gid,
            vec![metadata_uri.clone()],
            metadata_options,
        )));
        metadata
            .recover_mut()
            .set_option_snapshot(option_snapshot.clone());
        if memory_source {
            metadata.recover().mark_in_memory_download();
        }
        metadata.recover().set_belongs_to_gid(payload_gid);

        let payload = Arc::new(RwLock::new(RequestGroup::new(
            payload_gid,
            vec![format!("bt://{}", metadata_gid.to_hex_string())],
            payload_options,
        )));
        payload.recover_mut().set_option_snapshot(option_snapshot);
        payload.recover().set_output_name(output_name);
        payload.recover().set_metadata_info(metadata_info.clone());
        if let Some(bitfield) = entry.bitfield.clone() {
            payload.recover().set_bt_bitfield(Some(bitfield));
        }
        if let (Some(num_pieces), Some(piece_length), Some(info_hash_hex)) = (
            entry.num_pieces,
            entry.piece_length,
            entry.info_hash_hex.clone(),
        ) {
            payload
                .recover()
                .set_bt_metadata(num_pieces, piece_length, info_hash_hex);
        }
        let dependency = if memory_source {
            BtDependency::new_memory_with_fallback(
                metadata_gid,
                Arc::clone(&payload),
                Arc::clone(&metadata),
                payload_path,
                metadata_info,
                fallback_uris,
            )
        } else {
            BtDependency::new_file_with_fallback(
                metadata_gid,
                Arc::clone(&payload),
                metadata_path,
                payload_path,
                metadata_info,
                fallback_uris,
            )
        }
        .with_file_mappings(file_mappings);
        payload.recover().set_dependency(Box::new(dependency));
        payload.recover().set_total_length(entry.total_length);
        payload
            .recover()
            .set_completed_length(entry.completed_length);
        if entry.paused || entry.status == "paused" {
            metadata
                .recover_mut()
                .pause()
                .map_err(|error| error.to_string())?;
            payload
                .recover_mut()
                .pause()
                .map_err(|error| error.to_string())?;
        }

        let manager = &self.request_man;
        if manager.find_group(metadata_gid).is_some() || manager.find_group(payload_gid).is_some() {
            return Err("Metalink graph GID already exists".to_string());
        }
        manager.add_group_arc(metadata);
        manager.add_group_arc(payload);
        Ok(2)
    }

    /// Save active session on application shutdown.
    ///
    /// Called after engine finishes to save all incomplete downloads
    /// to the session file.
    ///
    /// # Returns
    /// - `Ok(Option<usize>)`: Number of saved entries (if save-session is configured)
    /// - `Err(String)`: Error during save
    pub async fn save_session_on_shutdown(&self) -> std::result::Result<Option<usize>, String> {
        let save_path = match self.get_opt_str("save-session").await {
            Some(path) => path,
            None => {
                debug!("save-session not configured, skipping shutdown save");
                return Ok(None);
            }
        };

        info!("Saving session to: {}", save_path);

        let session_path = PathBuf::from(&save_path);
        let mgr = ActiveSessionManager::new(session_path);

        // Snapshot group handles before the asynchronous file write. The
        // manager lock must not be held while session serialization performs
        // filesystem I/O, otherwise RPC mutations can be blocked at shutdown.
        let groups = self.request_man.list_groups();
        let stopped_results = self.request_man.get_stopped_results(0, usize::MAX);

        match mgr
            .save_session_with_results(&groups, &stopped_results)
            .await
        {
            Ok(n) => {
                info!("Successfully saved {} entries to {}", n, save_path);
                Ok(Some(n))
            }
            Err(e) => {
                warn!("Failed to save session: {}", e);
                Err(e)
            }
        }
    }

    /// Map SessionEntry options HashMap to DownloadOptions
    pub(super) fn map_entry_to_download_options(
        options: &std::collections::HashMap<String, String>,
    ) -> DownloadOptions {
        DownloadOptions::from_option_strings(options)
    }

    fn session_option_snapshot(
        options: &std::collections::HashMap<String, String>,
    ) -> std::collections::HashMap<String, serde_json::Value> {
        project_initial_options(
            options
                .iter()
                .map(|(name, value)| (name.clone(), serde_json::Value::String(value.clone()))),
        )
    }
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
fn standard_graph_payload_gid(entry: &SessionEntry) -> Option<GroupId> {
    entry
        .options
        .get("aria2-rust-payload-gid")
        .and_then(|value| GroupId::from_hex_string(value))
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
fn decode_session_bytes(value: Option<&String>) -> Option<Vec<u8>> {
    use base64::Engine;

    let value = value?;
    base64::engine::general_purpose::STANDARD.decode(value).ok()
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
fn decode_session_uris(value: Option<&String>) -> Option<Vec<String>> {
    let bytes = decode_session_bytes(value)?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
fn decode_session_mappings(value: Option<&String>) -> Option<Vec<BtFileMapping>> {
    let bytes = decode_session_bytes(value)?;
    serde_json::from_slice(&bytes).ok()
}

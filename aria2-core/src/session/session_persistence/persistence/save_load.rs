//! Core save/load logic for SessionPersistence.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::engine::resume_data::{ResumeData, ResumeDataExt};
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use crate::request::request_group::{BtFileMapping, FollowMode};
use crate::request::request_group::{DownloadOptions, GroupId, MetadataInfo, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use std::collections::HashMap;

use super::types::SessionPersistence;

impl SessionPersistence {
    pub(super) fn should_persist_group(group: &RequestGroup) -> bool {
        group.belongs_to_gid().is_none()
    }

    /// Save all active/paused/stopped command states to the session directory
    ///
    /// Iterates through all RequestGroups, converts each to ResumeData,
    /// and writes individual .aria2 files. Also saves global options.
    ///
    /// # Arguments
    ///
    /// * `groups` - Slice of active download groups to persist
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of successfully saved commands
    /// * `Err(String)` - Error message if critical failure occurs
    ///
    /// # File Format
    ///
    /// Each command is saved as `{gid}.aria2` in JSON format.
    /// Global options are saved as `session_options.json`.
    pub async fn save_state(
        &self,
        groups: &[Arc<std::sync::RwLock<RequestGroup>>],
    ) -> Result<usize, String> {
        // Ensure session directory exists
        tokio::fs::create_dir_all(&self.session_dir)
            .await
            .map_err(|e| {
                format!(
                    "Failed to create session dir {}: {}",
                    self.session_dir.display(),
                    e
                )
            })?;

        let mut saved = 0usize;
        let mut can_prune_stale_files = true;
        let mut saved_files = HashSet::new();

        for group_lock in groups.iter() {
            let group = group_lock.recover();
            if !Self::should_persist_group(&group) {
                debug!(gid = %group.gid().value(), "Skipping generated child group");
                continue;
            }

            // Convert RequestGroup to ResumeData
            match ResumeData::from_request_group(&group) {
                Ok(resume_data) => {
                    let file_name = format!("{}.aria2", resume_data.gid);
                    let path = self.session_dir.join(&file_name);

                    if let Err(e) = resume_data.save_to_file(&path) {
                        can_prune_stale_files = false;
                        warn!(
                            gid = %resume_data.gid,
                            error = %e,
                            "Failed to save resume data for GID"
                        );
                        continue;
                    }
                    saved += 1;
                    saved_files.insert(file_name);
                    debug!(
                        gid = %resume_data.gid,
                        path = %path.display(),
                        "Saved resume data"
                    );
                }
                Err(e) => {
                    can_prune_stale_files = false;
                    debug!(
                        gid = %group.gid().value(),
                        error = %e,
                        "Skipping command that cannot be serialized"
                    );
                }
            }
        }

        // Save global options summary
        self.save_global_options(groups).await?;

        // A successful session snapshot is authoritative. Remove resume
        // files from older snapshots, but keep them when a current entry
        // could not be serialized so a transient failure cannot erase the
        // last recoverable state.
        if can_prune_stale_files {
            remove_stale_resume_files(&self.session_dir, &saved_files).await?;
        }

        // Persist canonical cookies using aria2's Netscape-compatible format.
        let cookie_path = self.session_dir.join("cookies.txt");
        if let Err(e) = Self::save_cookie_storage_to_file(&self.cookie_storage, &cookie_path).await
        {
            warn!("Failed to persist cookies: {}", e);
        } else {
            debug!(path = %cookie_path.display(), "Cookies persisted to session");
        }

        // Keep the legacy JSON adapter available for existing callers.
        if let Some(ref jar) = self.cookie_jar {
            let legacy_path = self.session_dir.join("cookies.json");
            if let Err(e) = Self::save_cookie_jar_to_file(jar, &legacy_path).await {
                warn!("Failed to persist legacy cookies: {}", e);
            }
        }

        info!(
            saved,
            dir = %self.session_dir.display(),
            "Session state saved"
        );

        Ok(saved)
    }

    /// Load saved states from session directory and restore paused commands
    ///
    /// Reads all .aria2 files from the session directory, deserializes them,
    /// and creates paused download commands for each valid entry.
    ///
    /// # Arguments
    ///
    /// * `groups` - Mutable reference to the groups vector to restore into
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of successfully restored commands
    /// * `Err(String)` - Error message if critical failure occurs
    ///
    /// # Graceful Error Handling
    ///
    /// - Missing session directory returns Ok(0) (not an error)
    /// - Corrupt/malformed .aria2 files are skipped with a warning
    /// - Partial restoration is allowed (some files may fail)
    pub async fn load_state_into_manager(
        &mut self,
        manager: &crate::request::request_group_man::RequestGroupMan,
    ) -> Result<usize, String> {
        let mut restored = Vec::new();
        self.load_state(&mut restored).await?;

        let mut loaded = 0;
        for group in restored {
            match manager.add_restored_group(group) {
                Ok(()) => loaded += 1,
                Err(error) => warn!(error = %error, "Skipping conflicting restored group"),
            }
        }
        Ok(loaded)
    }

    pub async fn load_state(
        &mut self,
        groups: &mut Vec<Arc<std::sync::RwLock<RequestGroup>>>,
    ) -> Result<usize, String> {
        if !self.session_dir.exists() {
            debug!(
                dir = %self.session_dir.display(),
                "Session directory does not exist, nothing to load"
            );
            return Ok(0);
        }

        let mut loaded = 0usize;
        #[cfg(all(feature = "metalink", feature = "bittorrent"))]
        let mut graph_options: HashMap<GroupId, HashMap<String, String>> = HashMap::new();
        let mut entries = tokio::fs::read_dir(&self.session_dir).await.map_err(|e| {
            format!(
                "Failed to read session dir {}: {}",
                self.session_dir.display(),
                e
            )
        })?;

        loop {
            let Some(entry) = entries.next_entry().await.map_err(|error| {
                format!(
                    "Failed to enumerate session dir {} while loading state: {}",
                    self.session_dir.display(),
                    error
                )
            })?
            else {
                break;
            };
            let path = entry.path();

            // Only process .aria2 files
            let is_aria2 = path.extension().map(|e| e == "aria2").unwrap_or(false);

            if !is_aria2 {
                continue;
            }

            match ResumeData::load_from_file(&path) {
                Ok(Some(resume_data)) => {
                    // Restore command from resume data
                    match Self::restore_command(&resume_data) {
                        Ok(group) => {
                            let gid = group.gid();
                            if groups
                                .iter()
                                .any(|existing| existing.recover().gid() == gid)
                            {
                                warn!(
                                    gid = %resume_data.gid,
                                    "Duplicate persisted GID, skipping restore"
                                );
                                continue;
                            }

                            #[cfg(all(feature = "metalink", feature = "bittorrent"))]
                            graph_options.insert(gid, resume_data.options.clone());
                            groups.push(Arc::new(std::sync::RwLock::new(group)));
                            loaded += 1;
                            info!(
                                gid = %resume_data.gid,
                                status = %resume_data.status,
                                "Restored download from session"
                            );
                        }
                        Err(e) => {
                            warn!(
                                gid = %resume_data.gid,
                                error = %e,
                                "Failed to restore command from resume data"
                            );
                        }
                    }
                }
                Ok(None) => {
                    debug!(path = %path.display(), "Resume file was empty (skipped)");
                }
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "Corrupted or invalid .aria2 file, skipping gracefully"
                    );
                    // Continue loading other files - don't abort entire load
                }
            }
        }

        #[cfg(all(feature = "metalink", feature = "bittorrent"))]
        restore_metalink_graphs(groups, &graph_options);

        // Load global options if available
        let _ = self.load_global_options().await;

        // Load canonical cookies first, matching aria2's save-cookies format.
        let cookie_path = self.session_dir.join("cookies.txt");
        if cookie_path.exists() {
            if let Err(e) =
                Self::load_cookie_storage_from_file(&self.cookie_storage, &cookie_path).await
            {
                warn!("Failed to load cookies from session: {}", e);
            } else {
                info!("Loaded canonical cookies from session");
            }
        }

        // Read the legacy JSON adapter when present so existing session/API
        // callers continue to observe the persisted jar alongside canonical storage.
        let legacy_path = self.session_dir.join("cookies.json");
        if legacy_path.exists() {
            match Self::load_cookie_jar_from_file(&legacy_path).await {
                Ok(jar) => {
                    self.cookie_jar = Some(jar);
                    info!("Loaded legacy cookies from session");
                }
                Err(e) => {
                    warn!("Failed to load legacy cookies from session: {}", e);
                }
            }
        }

        info!(
            loaded,
            dir = %self.session_dir.display(),
            "Session state loaded"
        );

        Ok(loaded)
    }

    /// Restore a single download command from ResumeData
    ///
    /// Creates a new paused RequestGroup with the URIs and options
    /// extracted from the persisted state.
    fn restore_command(resume_data: &ResumeData) -> Result<RequestGroup, String> {
        if resume_data.uris.is_empty() {
            return Err("ResumeData has no URIs, cannot restore".to_string());
        }

        // Extract URIs from UriState list
        let uris: Vec<String> = resume_data.uris.iter().map(|u| u.uri.clone()).collect();

        // Build DownloadOptions from stored state
        let mut options = DownloadOptions::from_option_strings(&resume_data.options);
        if let Some(value) = resume_data.options.get("ssh-host-key-md") {
            options.ssh_host_key_md = Some(value.clone());
        }

        // Set output path if available
        if let Some(ref output_path) = resume_data.output_path {
            if let Some(parent) = Path::new(output_path).parent() {
                options.dir = Some(parent.to_string_lossy().to_string());
            }
            if let Some(file_name) = Path::new(output_path).file_name() {
                options.out = Some(file_name.to_string_lossy().to_string());
            }
        }

        let gid = GroupId::from_hex_string(&resume_data.gid).ok_or_else(|| {
            format!(
                "Invalid persisted GID '{}': not a hexadecimal u64",
                resume_data.gid
            )
        })?;

        let memory_download = options.uses_memory_download()
            || resume_data
                .options
                .get("aria2-rust-memory-download")
                .is_some_and(|value| value == "true");
        let mut group = RequestGroup::new(gid, uris, options);
        if memory_download {
            group.mark_in_memory_download();
        }
        group.set_total_length(resume_data.total_length);
        group.set_completed_length(resume_data.completed_length);
        group.set_uploaded_length(resume_data.uploaded_length);
        if !resume_data.bitfield.is_empty() {
            group.set_bt_bitfield(Some(resume_data.bitfield.clone()));
        }
        if resume_data.num_pieces.is_some()
            || resume_data.piece_length.is_some()
            || resume_data.bt_info_hash.is_some()
        {
            group.set_bt_metadata(
                resume_data.num_pieces.unwrap_or_default(),
                resume_data.piece_length.unwrap_or_default(),
                resume_data.bt_info_hash.clone().unwrap_or_default(),
            );
        }
        if let Some(output_name) = resume_data.options.get("aria2-rust-output-name") {
            group.set_output_name(output_name.clone());
        }
        if let Some(content_type) = resume_data.options.get("aria2-rust-content-type") {
            group.set_content_type(content_type.clone());
        }

        #[cfg(feature = "bittorrent")]
        if let Some(data) =
            decode_base64_bytes(resume_data.options.get("aria2-rust-bt-metadata-data"))
        {
            group.set_bt_metadata_data(data);
        }

        if let Some(metadata_path) = resume_data.bt_saved_metadata_path.as_deref() {
            let metadata_gid = resume_data
                .options
                .get("metadata-gid")
                .and_then(|value| GroupId::from_hex_string(value))
                .unwrap_or(gid);
            let metadata_uri = resume_data
                .options
                .get("metadata-uri")
                .map(String::as_str)
                .or_else(|| resume_data.uris.first().map(|uri| uri.uri.as_str()))
                .unwrap_or_default();
            group.set_metadata_info(
                MetadataInfo::new(metadata_gid, metadata_uri)
                    .with_metadata_path(metadata_path.to_owned()),
            );
        }
        #[cfg(feature = "metalink")]
        if let (Some(encoded), Some(file_index)) = (
            resume_data.metalink_data.as_deref(),
            resume_data.metalink_file_index,
        ) {
            use base64::Engine;
            let data = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| format!("Invalid persisted Metalink data: {error}"))?;
            group.set_metalink_source(data, file_index);
        }

        // Restore progress information if available
        if resume_data.completed_length > 0 {
            group.set_resume_offset(resume_data.completed_length);
        }

        if resume_data.status == "paused" {
            group.pause().map_err(|error| error.to_string())?;
        }

        Ok(group)
    }

    /// Start background auto-save task
    ///
    /// Spawns a Tokio task that periodically calls save_state().
    /// The task runs until the returned handle is dropped or cancelled.
    ///
    /// # Arguments
    ///
    /// * `groups` - Arc-wrapped shared reference to the groups vector
    ///
    /// # Returns
    ///
    /// A JoinHandle that can be used to cancel the auto-save task
    pub fn start_auto_save(
        &self,
        groups: Arc<tokio::sync::RwLock<Vec<Arc<std::sync::RwLock<RequestGroup>>>>>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !self.auto_save_enabled {
            debug!("Auto-save is disabled");
            return None;
        }

        let session_dir = self.session_dir.clone();
        let interval = self.auto_save_interval;

        info!(
            interval_secs = interval.as_secs(),
            dir = %session_dir.display(),
            "Starting auto-save task"
        );

        Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);

            loop {
                ticker.tick().await;

                let groups_read = groups.read().await;
                let persistence = SessionPersistence::new(&session_dir).without_auto_save();

                match persistence.save_state(&groups_read).await {
                    Ok(count) => {
                        if count > 0 {
                            debug!(count, "Auto-save completed successfully");
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Auto-save failed, will retry next interval");
                    }
                }
            }
        }))
    }
}

async fn remove_stale_resume_files(
    session_dir: &Path,
    retained_files: &HashSet<String>,
) -> Result<(), String> {
    let mut entries = tokio::fs::read_dir(session_dir).await.map_err(|error| {
        format!(
            "Failed to read session dir {} while pruning stale files: {}",
            session_dir.display(),
            error
        )
    })?;

    while let Some(entry) = entries.next_entry().await.map_err(|error| {
        format!(
            "Failed to enumerate session dir {} while pruning stale files: {}",
            session_dir.display(),
            error
        )
    })? {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if path
            .extension()
            .is_some_and(|extension| extension == "aria2")
            && !retained_files.contains(file_name)
        {
            tokio::fs::remove_file(&path).await.map_err(|error| {
                format!(
                    "Failed to remove stale session file {}: {}",
                    path.display(),
                    error
                )
            })?;
        }
    }

    Ok(())
}

/// Recreate the metadata prerequisite for a persisted Metalink torrent graph.
///
/// Generated payload groups are intentionally not serialized independently.
/// The payload's `MetadataInfo` and persisted metadata path are the durable
/// identity, so loading must rebuild the metadata group and its dependency
/// before the manager starts promoting downloads.
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
fn restore_metalink_graphs(
    groups: &mut Vec<Arc<std::sync::RwLock<RequestGroup>>>,
    graph_options: &HashMap<GroupId, HashMap<String, String>>,
) {
    use std::path::PathBuf;

    let existing_gids: Vec<GroupId> = groups.iter().map(|group| group.recover().gid()).collect();
    let mut new_metadata = Vec::new();

    for payload in groups.iter() {
        let (payload_gid, info, options, output_name) = {
            let group = payload.recover();
            if group.belongs_to_gid().is_some()
                || !group.uris().iter().any(|uri| uri.starts_with("bt://"))
            {
                continue;
            }
            let Some(info) = group.metadata_info() else {
                continue;
            };
            if info.gid().is_none() || info.uri().is_empty() || info.metadata_path().is_none() {
                continue;
            }
            (
                group.gid(),
                info,
                group.options().clone(),
                group.output_name().or_else(|| group.options().out.clone()),
            )
        };

        let metadata_gid = info.gid().expect("metadata provenance was checked above");
        let metadata_path = PathBuf::from(
            info.metadata_path()
                .expect("metadata path was checked above"),
        );
        let persisted_options = graph_options.get(&payload_gid);
        let fallback_uris = persisted_options
            .and_then(|options| decode_json_string_list(options.get("aria2-rust-fallback-uris")))
            .unwrap_or_default();
        let file_mappings = persisted_options
            .and_then(|options| decode_json_file_mappings(options.get("aria2-rust-file-mappings")))
            .unwrap_or_default();
        let metadata_data = persisted_options
            .and_then(|options| decode_base64_bytes(options.get("aria2-rust-bt-metadata-data")));
        let memory_source = persisted_options
            .and_then(|options| options.get("aria2-rust-metadata-memory"))
            .is_some_and(|value| value == "true")
            || metadata_data.is_some();

        let metadata_group = if let Some(existing) = groups.iter().find(|group| {
            let group = group.recover();
            group.gid() == metadata_gid && group.uris().iter().any(|uri| uri == info.uri())
        }) {
            Arc::clone(existing)
        } else if existing_gids.contains(&metadata_gid)
            || new_metadata
                .iter()
                .any(|group: &Arc<std::sync::RwLock<RequestGroup>>| {
                    group.recover().gid() == metadata_gid
                })
        {
            tracing::warn!(
                payload_gid = payload_gid.value(),
                metadata_gid = metadata_gid.value(),
                "Cannot restore Metalink graph because metadata GID is already occupied"
            );
            continue;
        } else {
            let mut metadata_options = options.clone();
            metadata_options.follow_torrent = Some(FollowMode::Disabled);
            metadata_options.follow_metalink = Some(FollowMode::Disabled);
            metadata_options.dir = metadata_path
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned());
            metadata_options.out = metadata_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
                metadata_gid,
                vec![info.uri().to_string()],
                metadata_options,
            )));
            if memory_source {
                group.recover().mark_in_memory_download();
                if let Some(data) = metadata_data.clone() {
                    group.recover().set_in_memory_data(data);
                }
            }
            group.recover().set_belongs_to_gid(payload_gid);
            new_metadata.push(Arc::clone(&group));
            group
        };

        let payload_path = output_name
            .map(|name| PathBuf::from(options.dir.as_deref().unwrap_or(".")).join(name))
            .unwrap_or_else(|| {
                PathBuf::from(options.dir.as_deref().unwrap_or("."))
                    .join(format!("{}.bin", payload_gid.to_hex_string()))
            });

        let dependency = if memory_source {
            crate::request::request_group::BtDependency::new_memory_with_fallback(
                metadata_gid,
                Arc::clone(payload),
                Arc::clone(&metadata_group),
                payload_path,
                info,
                fallback_uris,
            )
        } else {
            crate::request::request_group::BtDependency::new_file_with_fallback(
                metadata_gid,
                Arc::clone(payload),
                metadata_path,
                payload_path,
                info,
                fallback_uris,
            )
        }
        .with_file_mappings(file_mappings);
        payload.recover().set_dependency(Box::new(dependency));
        let _ = metadata_group;
    }

    for group in new_metadata.into_iter().rev() {
        groups.insert(0, group);
    }
}

#[cfg(feature = "bittorrent")]
fn decode_base64_bytes(value: Option<&String>) -> Option<Vec<u8>> {
    use base64::Engine;

    base64::engine::general_purpose::STANDARD
        .decode(value?)
        .ok()
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
fn decode_json_string_list(value: Option<&String>) -> Option<Vec<String>> {
    let bytes = decode_base64_bytes(value)?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(all(feature = "metalink", feature = "bittorrent"))]
fn decode_json_file_mappings(value: Option<&String>) -> Option<Vec<BtFileMapping>> {
    let bytes = decode_base64_bytes(value)?;
    serde_json::from_slice(&bytes).ok()
}

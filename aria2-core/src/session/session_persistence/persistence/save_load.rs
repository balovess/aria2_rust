//! Core save/load logic for SessionPersistence.

use std::path::Path;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::engine::resume_data::{ResumeData, ResumeDataExt};
use crate::request::request_group::{DownloadOptions, GroupId, MetadataInfo, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

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
                        warn!(
                            gid = %resume_data.gid,
                            error = %e,
                            "Failed to save resume data for GID"
                        );
                        continue;
                    }
                    saved += 1;
                    debug!(
                        gid = %resume_data.gid,
                        path = %path.display(),
                        "Saved resume data"
                    );
                }
                Err(e) => {
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
        let mut entries = tokio::fs::read_dir(&self.session_dir).await.map_err(|e| {
            format!(
                "Failed to read session dir {}: {}",
                self.session_dir.display(),
                e
            )
        })?;

        while let Ok(Some(entry)) = entries.next_entry().await {
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
        let mut options = DownloadOptions::default();
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

        let group = RequestGroup::new(gid, uris, options);

        if let Some(metadata_path) = resume_data.bt_saved_metadata_path.as_deref() {
            let metadata_uri = resume_data
                .uris
                .first()
                .map(|uri| uri.uri.as_str())
                .unwrap_or_default();
            group.set_metadata_info(
                MetadataInfo::new(gid, metadata_uri).with_metadata_path(metadata_path.to_owned()),
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

        // Mark as paused if status indicates so
        if resume_data.status == "paused" || resume_data.status == "waiting" {
            // The group will be created in a paused/waiting state
            // Actual pause handling depends on the engine's lifecycle management
        }

        // Restore progress information if available
        if resume_data.completed_length > 0 {
            group.set_resume_offset(resume_data.completed_length);
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

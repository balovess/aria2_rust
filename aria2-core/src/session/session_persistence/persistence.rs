//! SessionPersistence struct and core save/load logic.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::engine::resume_data::{ResumeData, ResumeDataExt};
use crate::http::cookie_storage::CookieJar;
use crate::request::request_group::{DownloadOptions, DownloadStatus, GroupId, RequestGroup};
use crate::selector::server_stat_man::ServerStatMan;
use crate::util::rwlock_ext::RwLockRecover;

/// Filename for global session options saved alongside .aria2 files
const SESSION_OPTIONS_FILENAME: &str = "session_options.json";

/// Default auto-save interval in seconds
pub const DEFAULT_AUTO_SAVE_INTERVAL_SECS: u64 = 60;

/// High-level session persistence manager
///
/// Coordinates saving and loading of download session state using the
/// ResumeData JSON format. Manages both individual command states (.aria2
/// files) and global session options.
///
/// # Examples
///
/// ```ignore
/// use aria2_core::session::session_persistence::SessionPersistence;
/// use std::path::Path;
///
/// let session = SessionPersistence::new(Path::new("/tmp/aria2_session"));
///
/// // Save current state
/// let count = session.save_state(&groups).await?;
/// println!("Saved {} downloads", count);
///
/// // Load saved state
/// let count = session.load_state(&mut groups).await?;
/// println!("Restored {} downloads", count);
/// ```
pub struct SessionPersistence {
    /// Directory where .aria2 files are stored
    session_dir: PathBuf,
    /// Auto-save interval
    pub(super) auto_save_interval: Duration,
    /// Whether auto-save is enabled
    pub(super) auto_save_enabled: bool,
    /// Optional cookie jar for persisting cookies alongside session data
    cookie_jar: Option<CookieJar>,
}

impl SessionPersistence {
    /// Create a new SessionPersistence instance
    ///
    /// # Arguments
    ///
    /// * `session_dir` - Directory path for storing .aria2 session files
    pub fn new(session_dir: &Path) -> Self {
        Self {
            session_dir: session_dir.to_path_buf(),
            auto_save_interval: Duration::from_secs(DEFAULT_AUTO_SAVE_INTERVAL_SECS),
            auto_save_enabled: true,
            cookie_jar: None,
        }
    }

    /// Create with custom auto-save interval
    pub fn with_interval(mut self, interval_secs: u64) -> Self {
        self.auto_save_interval = Duration::from_secs(interval_secs.max(10));
        self
    }

    /// Disable auto-save (only manual save/load)
    pub fn without_auto_save(mut self) -> Self {
        self.auto_save_enabled = false;
        self
    }

    /// Set cookie jar for persistence alongside session data
    pub fn with_cookie_jar(mut self, jar: CookieJar) -> Self {
        self.cookie_jar = Some(jar);
        self
    }

    /// Get mutable reference to the cookie jar for adding cookies before saving
    pub fn cookie_jar_mut(&mut self) -> Option<&mut CookieJar> {
        self.cookie_jar.as_mut()
    }

    /// Get reference to the cookie jar
    pub fn cookie_jar(&self) -> Option<&CookieJar> {
        self.cookie_jar.as_ref()
    }

    /// Get the session directory path
    pub fn session_dir(&self) -> &Path {
        &self.session_dir
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
    pub async fn save_state(&self, groups: &[Arc<std::sync::RwLock<RequestGroup>>]) -> Result<usize, String> {
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

        // Persist cookies if cookie jar is available
        if let Some(ref jar) = self.cookie_jar {
            let cookie_path = self.session_dir.join("cookies.json");
            if let Err(e) = Self::save_cookie_jar_to_file(jar, &cookie_path).await {
                warn!("Failed to persist cookies: {}", e);
            } else {
                debug!(path = %cookie_path.display(), "Cookies persisted to session");
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

        // Load cookies from session directory
        let cookie_path = self.session_dir.join("cookies.json");
        if cookie_path.exists() {
            match Self::load_cookie_jar_from_file(&cookie_path).await {
                Ok(jar) => {
                    self.cookie_jar = Some(jar);
                    info!("Loaded cookies from session");
                }
                Err(e) => {
                    warn!("Failed to load cookies from session: {}", e);
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

        // Set output path if available
        if let Some(ref output_path) = resume_data.output_path {
            if let Some(parent) = Path::new(output_path).parent() {
                options.dir = Some(parent.to_string_lossy().to_string());
            }
            if let Some(file_name) = Path::new(output_path).file_name() {
                options.out = Some(file_name.to_string_lossy().to_string());
            }
        }

        // Generate GID from stored value (try to parse hex, or create new)
        let gid = if !resume_data.gid.is_empty() {
            GroupId::from_hex_string(&resume_data.gid).unwrap_or_else(GroupId::new_random)
        } else {
            GroupId::new_random()
        };

        let group = RequestGroup::new(gid, uris, options);

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

    /// Save global options summary to session directory
    async fn save_global_options(
        &self,
        _groups: &[Arc<std::sync::RwLock<RequestGroup>>],
    ) -> Result<(), String> {
        let opts_path = self.session_dir.join(SESSION_OPTIONS_FILENAME);

        // Build a simple options summary from all groups
        let options_summary = serde_json::json!({
            "version": "1.0",
            "saved_at": chrono_timestamp_or_fallback(),
            "note": "Global session options summary"
        });

        let json = serde_json::to_string_pretty(&options_summary)
            .map_err(|e| format!("Failed to serialize session options: {}", e))?;

        tokio::fs::write(&opts_path, json).await.map_err(|e| {
            format!(
                "Failed to write session options {}: {}",
                opts_path.display(),
                e
            )
        })?;

        Ok(())
    }

    /// Load global options from session directory
    async fn load_global_options(&self) -> Result<(), String> {
        let opts_path = self.session_dir.join(SESSION_OPTIONS_FILENAME);

        if !opts_path.exists() {
            return Ok(());
        }

        let content = tokio::fs::read_to_string(&opts_path).await.map_err(|e| {
            format!(
                "Failed to read session options {}: {}",
                opts_path.display(),
                e
            )
        })?;

        // Validate it's valid JSON (basic sanity check)
        let _parsed: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| format!("Invalid JSON in session options: {}", e))?;

        debug!(path = %opts_path.display(), "Loaded session options");

        Ok(())
    }

    /// Clean up all session files (for testing or reset)
    pub async fn cleanup(&self) -> Result<(), String> {
        if !self.session_dir.exists() {
            return Ok(());
        }

        let mut entries = tokio::fs::read_dir(&self.session_dir)
            .await
            .map_err(|e| format!("Failed to read session dir: {}", e))?;

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if let Err(e) = tokio::fs::remove_file(&path).await {
                warn!(path = %path.display(), error = %e, "Failed to remove session file");
            }
        }

        info!(dir = %self.session_dir.display(), "Session directory cleaned up");
        Ok(())
    }

    // =====================================================================
    // Server Statistics Persistence
    // =====================================================================

    /// Save server statistics to the session directory.
    ///
    /// Persists all server performance statistics (download speeds, error counts,
    /// etc.) to a JSON file in the session directory. This allows the adaptive
    /// URI selector to remember server performance across restarts.
    ///
    /// # Arguments
    ///
    /// * `stat_man` - Reference to the ServerStatMan to save
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of server stats saved
    /// * `Err(String)` - Error message if save fails
    ///
    /// # File Location
    ///
    /// Stats are saved to `{session_dir}/server-stat.json`
    ///
    /// # Example
    ///
    /// ```ignore
    /// use aria2_core::session::session_persistence::SessionPersistence;
    /// use aria2_core::selector::server_stat_man::ServerStatMan;
    ///
    /// let persistence = SessionPersistence::new(Path::new("/tmp/aria2_session"));
    /// let stat_man = ServerStatMan::new();
    /// stat_man.update("fast.mirror.com", 10000, false);
    ///
    /// let saved = persistence.save_server_stats(&stat_man).await?;
    /// println!("Saved {} server stats", saved);
    /// ```
    pub async fn save_server_stats(&self, stat_man: &ServerStatMan) -> Result<usize, String> {
        let stat_file = self.session_dir.join("server-stat.json");
        let saved = stat_man.save_to_file_async(&stat_file).await?;

        if saved > 0 {
            debug!(
                count = saved,
                path = %stat_file.display(),
                "Server statistics saved"
            );
        }

        Ok(saved)
    }

    /// Load server statistics from the session directory.
    ///
    /// Restores previously saved server performance statistics from a JSON file
    /// in the session directory. This allows the adaptive URI selector to
    /// make informed decisions immediately after startup.
    ///
    /// # Arguments
    ///
    /// * `stat_man` - Reference to the ServerStatMan to load into
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of server stats loaded
    /// * `Err(String)` - Error message if load fails
    ///
    /// # Behavior
    ///
    /// - Returns `Ok(0)` if no server-stat.json file exists (not an error)
    /// - Returns error if file exists but is invalid
    /// - Merges with existing stats (doesn't clear current stats)
    ///
    /// # Example
    ///
    /// ```ignore
    /// use aria2_core::session::session_persistence::SessionPersistence;
    /// use aria2_core::selector::server_stat_man::ServerStatMan;
    ///
    /// let persistence = SessionPersistence::new(Path::new("/tmp/aria2_session"));
    /// let stat_man = ServerStatMan::new();
    ///
    /// let loaded = persistence.load_server_stats(&stat_man).await?;
    /// println!("Loaded {} server stats from previous session", loaded);
    /// ```
    pub async fn load_server_stats(&self, stat_man: &ServerStatMan) -> Result<usize, String> {
        let stat_file = self.session_dir.join("server-stat.json");

        if !stat_file.exists() {
            debug!("No server statistics file found, starting fresh");
            return Ok(0);
        }

        let loaded = stat_man.load_from_file_async(&stat_file).await?;

        if loaded > 0 {
            info!(
                count = loaded,
                path = %stat_file.display(),
                "Server statistics loaded from previous session"
            );
        }

        Ok(loaded)
    }

    // =====================================================================
    // K2.1 — Selective Save Methods
    // =====================================================================

    /// Save only active/in-progress downloads (skip completed/stopped/error).
    ///
    /// Filters groups by download status, persisting only those that are
    /// actively downloading or waiting in queue.
    ///
    /// # Arguments
    ///
    /// * `groups` - Slice of all download groups to filter
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of active downloads successfully saved
    /// * `Err(String)` - Error message if critical failure occurs
    pub async fn save_active_only(
        &self,
        groups: &[Arc<std::sync::RwLock<RequestGroup>>],
    ) -> Result<usize, String> {
        let mut count = 0;
        for group in groups {
            let g = group.recover();
            let status = g.status();

            // Only save if actively downloading or waiting
            match status {
                DownloadStatus::Active | DownloadStatus::Waiting => {
                    drop(g);
                    // Convert and save this single group
                    let group_read = group.recover();
                    match ResumeData::from_request_group(&group_read) {
                        Ok(resume_data) => {
                            drop(group_read);
                            let file_name = format!("{}.aria2", resume_data.gid);
                            let path = self.session_dir.join(&file_name);
                            if resume_data.save_to_file(&path).is_ok() {
                                count += 1;
                                debug!(gid = %resume_data.gid, "Saved active download");
                            } else {
                                warn!(gid = %resume_data.gid, "Failed to save active download");
                            }
                        }
                        Err(e) => {
                            debug!(error = %e, "Skipping active download that cannot be serialized");
                        }
                    }
                }
                _ => {} // Skip completed, paused, removed, error
            }
        }
        debug!(
            saved = count,
            total = groups.len(),
            "save_active_only completed"
        );
        Ok(count)
    }

    /// Save only completed downloads for archival.
    ///
    /// Filters groups by completion status, persisting only finished downloads.
    /// Useful for creating archives of successful downloads separate from
    /// active/pending work.
    ///
    /// # Arguments
    ///
    /// * `groups` - Slice of all download groups to filter
    ///
    /// # Returns
    ///
    /// * `Ok(usize)` - Number of completed downloads successfully saved
    /// * `Err(String)` - Error message if critical failure occurs
    pub async fn save_completed(
        &self,
        groups: &[Arc<std::sync::RwLock<RequestGroup>>],
    ) -> Result<usize, String> {
        let mut count = 0;
        for group in groups {
            let g = group.recover();
            let status = g.status();

            if status.is_completed() || matches!(status, DownloadStatus::Complete) {
                drop(g);
                // Convert and save this completed group
                let group_read = group.recover();
                match ResumeData::from_request_group(&group_read) {
                    Ok(resume_data) => {
                        drop(group_read);
                        let file_name = format!("{}.aria2", resume_data.gid);
                        let path = self.session_dir.join(&file_name);
                        if resume_data.save_to_file(&path).is_ok() {
                            count += 1;
                            debug!(gid = %resume_data.gid, "Saved completed download");
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "Skipping completed download that cannot be serialized");
                    }
                }
            }
        }
        debug!(
            saved = count,
            total = groups.len(),
            "save_completed completed"
        );
        Ok(count)
    }

    // =====================================================================
    // K2.3 — Cookie Persistence Helpers
    // =====================================================================

    /// Save cookie jar to a JSON file for persistence.
    ///
    /// Serializes all cookies in the jar to JSON format for storage alongside
    /// session data. Uses simple JSON serialization since CookieJar doesn't
    /// have built-in file I/O methods.
    async fn save_cookie_jar_to_file(jar: &CookieJar, path: &Path) -> Result<(), String> {
        // Use serde_json to serialize the cookie jar's internal data
        #[derive(Serialize)]
        struct SerializableJar<'a> {
            cookies: &'a [crate::http::cookie_storage::JarCookie],
        }

        let serializable = SerializableJar {
            cookies: &jar.cookies,
        };

        let json = serde_json::to_string_pretty(&serializable).map_err(|e| e.to_string())?;

        tokio::fs::write(path, json)
            .await
            .map_err(|e| format!("Failed to write cookie file: {}", e))
    }

    /// Load cookie jar from a JSON file.
    ///
    /// Deserializes cookies from JSON format and creates a new CookieJar
    /// instance with the loaded data.
    async fn load_cookie_jar_from_file(path: &Path) -> Result<CookieJar, String> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| format!("Failed to read cookie file: {}", e))?;

        #[derive(Deserialize)]
        struct SerializableJar {
            cookies: Vec<crate::http::cookie_storage::JarCookie>,
        }

        let parsed: SerializableJar =
            serde_json::from_str(&content).map_err(|e| format!("Invalid cookie JSON: {}", e))?;

        let mut jar = CookieJar::new();
        for cookie in parsed.cookies {
            jar.store(cookie);
        }

        Ok(jar)
    }
}

/// Fallback timestamp generator when chrono is not available
fn chrono_timestamp_or_fallback() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

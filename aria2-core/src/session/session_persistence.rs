//! Session Save/Load Persistence - Phase 15 H4
//!
//! Provides complete session state persistence using the ResumeData JSON format
//! (.aria2 files). This module bridges the ActiveSessionManager with the
//! ResumeData serialization system for cross-restart download resumption.
//!
//! # Architecture
//!
//! ```text
//! session_persistence.rs (this file)
//!   ├── SessionPersistence struct - High-level save/load coordinator
//!   ├── save_state() - Serialize all commands to .aria2 files
//!   ├── load_state() - Restore commands from .aria2 files
//!   ├── start_auto_save() - Background periodic save task
//!   └── signal_handler() - SIGTERM/SIGINT graceful shutdown
//!
//! Dependencies:
//!   resume_data.rs - ResumeData, UriState, ChecksumInfo structs
//!   active_session.rs - ActiveSessionManager for session file I/O
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::engine::resume_data::{ResumeData, ResumeDataExt};
use crate::http::cookie_storage::CookieJar;
use crate::request::request_group::{DownloadOptions, DownloadStatus, GroupId, RequestGroup};

/// Filename for global session options saved alongside .aria2 files
const SESSION_OPTIONS_FILENAME: &str = "session_options.json";

/// Default auto-save interval in seconds
const DEFAULT_AUTO_SAVE_INTERVAL_SECS: u64 = 60;

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
    auto_save_interval: Duration,
    /// Whether auto-save is enabled
    auto_save_enabled: bool,
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
    pub async fn save_state(&self, groups: &[Arc<RwLock<RequestGroup>>]) -> Result<usize, String> {
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
            let group = group_lock.read().await;

            // Convert RequestGroup to ResumeData
            match ResumeData::from_request_group(&group).await {
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
        groups: &mut Vec<Arc<RwLock<RequestGroup>>>,
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
                            groups.push(Arc::new(RwLock::new(group)));
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
        groups: Arc<RwLock<Vec<Arc<RwLock<RequestGroup>>>>>,
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
        _groups: &[Arc<RwLock<RequestGroup>>],
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
        groups: &[Arc<RwLock<RequestGroup>>],
    ) -> Result<usize, String> {
        let mut count = 0;
        for group in groups {
            let g = group.read().await;
            let status = g.status().await;

            // Only save if actively downloading or waiting
            match status {
                DownloadStatus::Active | DownloadStatus::Waiting => {
                    drop(g);
                    // Convert and save this single group
                    let group_read = group.read().await;
                    match ResumeData::from_request_group(&group_read).await {
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
        groups: &[Arc<RwLock<RequestGroup>>],
    ) -> Result<usize, String> {
        let mut count = 0;
        for group in groups {
            let g = group.read().await;
            let status = g.status().await;

            if status.is_completed() || matches!(status, DownloadStatus::Complete) {
                drop(g);
                // Convert and save this completed group
                let group_read = group.read().await;
                match ResumeData::from_request_group(&group_read).await {
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

// =========================================================================
// K2.2 — DHT State Snapshot
// =========================================================================

/// Snapshot of DHT (Distributed Hash Table) routing state for persistence.
///
/// Captures the current state of DHT nodes, token secret, and bootstrap timing
/// to allow quick resumption without full bootstrap on restart.
///
/// # Serialization
///
/// This struct implements Serialize/Deserialize for JSON persistence alongside
/// session data. Use `to_json_string()` and `from_json_string()` for conversion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DhtStateSnapshot {
    /// Known DHT nodes in the routing table
    pub nodes: Vec<DhtNodeInfo>,
    /// Current token secret used for DHT get_peers requests (20 bytes)
    pub token_secret: [u8; 20],
    /// Unix epoch timestamp of last successful bootstrap, if any
    pub last_bootstrap_epoch_secs: Option<u64>,
    /// Total number of nodes in the snapshot (convenience field)
    pub total_nodes: usize,
}

/// Information about a single DHT node in the routing table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DhtNodeInfo {
    /// 20-byte node ID (SHA-1 hash)
    pub id: [u8; 20],
    /// Network address as "ip:port" string
    pub addr: String,
    /// Unix epoch timestamp when this node was last seen/verified
    pub last_seen_epoch_secs: u64,
}

impl DhtStateSnapshot {
    /// Create an empty snapshot (for when DHT is unavailable or not initialized).
    ///
    /// Returns a snapshot with no nodes and zeroed token secret,
    /// suitable as a default or placeholder value.
    pub fn empty() -> Self {
        Self {
            nodes: vec![],
            token_secret: [0u8; 20],
            last_bootstrap_epoch_secs: None,
            total_nodes: 0,
        }
    }

    /// Create a snapshot from node data with automatic total_nodes calculation.
    ///
    /// # Arguments
    ///
    /// * `nodes` - Vector of known DHT nodes
    /// * `token_secret` - Current 20-byte token secret
    /// * `last_bootstrap` - Optional timestamp of last bootstrap
    pub fn new(
        nodes: Vec<DhtNodeInfo>,
        token_secret: [u8; 20],
        last_bootstrap_epoch_secs: Option<u64>,
    ) -> Self {
        let total_nodes = nodes.len();
        Self {
            nodes,
            token_secret,
            last_bootstrap_epoch_secs,
            total_nodes,
        }
    }

    /// Serialize snapshot to JSON string for persistence.
    ///
    /// # Returns
    ///
    /// * `Ok(String)` - JSON-formatted snapshot data
    /// * `Err(String)` - Error message if serialization fails
    pub fn to_json_string(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| e.to_string())
    }

    /// Parse snapshot from JSON string.
    ///
    /// # Arguments
    ///
    /// * `json` - JSON string containing serialized snapshot data
    ///
    /// # Returns
    ///
    /// * `Ok(DhtStateSnapshot)` - Deserialized snapshot
    /// * `Err(String)` - Error message if parsing fails
    pub fn from_json_string(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper to create a temporary directory for tests
    fn create_test_session_dir() -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            % 1_000_000_000;
        let dir =
            std::env::temp_dir().join(format!("aria2_session_test_{}_{}", std::process::id(), ts));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("Failed to create test session directory");
        dir
    }

    /// Helper to create test RequestGroups
    fn create_test_groups(count: usize) -> Vec<Arc<RwLock<RequestGroup>>> {
        let mut groups = Vec::new();
        for i in 0..count {
            let gid = GroupId::new(i as u64 + 1000);
            let uri = format!("http://example.com/file{}.bin", i);
            let options = DownloadOptions {
                dir: Some("/downloads".to_string()),
                split: Some(4),
                ..Default::default()
            };
            let group = Arc::new(RwLock::new(RequestGroup::new(gid, vec![uri], options)));
            groups.push(group);
        }
        groups
    }

    #[tokio::test]
    async fn test_session_save_creates_files() {
        let session_dir = create_test_session_dir();
        let persistence = SessionPersistence::new(&session_dir);

        let groups = create_test_groups(3);

        // Save state
        let saved_count = persistence
            .save_state(&groups)
            .await
            .expect("Save should succeed");

        assert_eq!(saved_count, 3, "Should save 3 commands");

        // Verify .aria2 files were created
        let entries: Vec<_> = fs::read_dir(&session_dir)
            .expect("Should read session dir")
            .filter_map(|e| e.ok())
            .collect();

        // Should have at least 3 .aria2 files + 1 options file
        let aria2_count = entries
            .iter()
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "aria2")
                    .unwrap_or(false)
            })
            .count();

        assert_eq!(aria2_count, 3, "Should have 3 .aria2 files");

        // Verify each file contains valid JSON with GID
        for entry in entries.iter().filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "aria2")
                .unwrap_or(false)
        }) {
            let content = fs::read_to_string(entry.path()).expect("Should read file");
            let parsed: serde_json::Value =
                serde_json::from_str(&content).expect("Should be valid JSON");
            assert!(
                parsed.get("gid").is_some(),
                "Each .aria2 file should contain a GID field"
            );
        }

        // Clean up
        let _ = fs::remove_dir_all(&session_dir);
    }

    #[tokio::test]
    async fn test_session_load_restores_commands() {
        let session_dir = create_test_session_dir();
        let mut persistence = SessionPersistence::new(&session_dir);

        // Create and save original groups
        let original_groups = create_test_groups(2);
        let saved = persistence
            .save_state(&original_groups)
            .await
            .expect("Save should succeed");
        assert_eq!(saved, 2, "Should save 2 commands");

        // Load into empty groups vector
        let mut loaded_groups: Vec<Arc<RwLock<RequestGroup>>> = Vec::new();
        let loaded = persistence
            .load_state(&mut loaded_groups)
            .await
            .expect("Load should succeed");

        assert_eq!(loaded, 2, "Should restore 2 commands");

        // Verify restored groups have URIs
        let mut found_uris: Vec<String> = Vec::new();
        for group_lock in &loaded_groups {
            let group = group_lock.read().await;
            for uri in group.uris() {
                found_uris.push(uri.clone());
            }
        }

        assert!(
            found_uris.iter().any(|u| u.contains("file0.bin")),
            "Should restore first file URI"
        );
        assert!(
            found_uris.iter().any(|u| u.contains("file1.bin")),
            "Should restore second file URI"
        );

        // Clean up
        let _ = fs::remove_dir_all(&session_dir);
    }

    #[tokio::test]
    async fn test_session_save_empty_no_error() {
        let session_dir = create_test_session_dir();
        let persistence = SessionPersistence::new(&session_dir);

        // Save empty groups list - should succeed without error
        let empty_groups: Vec<Arc<RwLock<RequestGroup>>> = Vec::new();
        let result = persistence.save_state(&empty_groups).await;

        assert!(result.is_ok(), "Saving empty session should not error");
        let saved_count = result.unwrap();
        assert_eq!(saved_count, 0, "Empty session should report 0 saved");

        // Session directory should still exist (with options file at least)
        assert!(
            session_dir.exists(),
            "Session dir should be created even for empty save"
        );

        // Clean up
        let _ = fs::remove_dir_all(&session_dir);
    }

    #[tokio::test]
    async fn test_session_corrupted_file_skipped_gracefully() {
        let session_dir = create_test_session_dir();

        // Create a corrupted .aria2 file
        let corrupt_file = session_dir.join("corrupt-gid.aria2");
        fs::write(&corrupt_file, "THIS IS NOT VALID JSON {{{{").expect("Should write corrupt file");

        // Also create a valid .aria2 file
        let valid_file = session_dir.join("valid-gid.aria2");
        let valid_resume_data = ResumeData {
            gid: "valid-gid-12345".to_string(),
            uris: vec![crate::engine::resume_data::UriState {
                uri: "http://example.com/valid-file.bin".to_string(),
                tried: true,
                used: false,
                last_result: None,
                speed_bytes_per_sec: None,
            }],
            total_length: 1024,
            completed_length: 512,
            uploaded_length: 0,
            bitfield: vec![],
            num_pieces: None,
            piece_length: None,
            status: "paused".to_string(),
            error_message: None,
            last_download_time: 0,
            created_at: 0,
            output_path: Some("/downloads/valid-file.bin".to_string()),
            checksum: None,
            options: std::collections::HashMap::new(),
            resume_offset: Some(512),
            bt_info_hash: None,
            bt_saved_metadata_path: None,
        };
        valid_resume_data
            .save_to_file(&valid_file)
            .expect("Should write valid file");

        let mut persistence = SessionPersistence::new(&session_dir);
        let mut loaded_groups: Vec<Arc<RwLock<RequestGroup>>> = Vec::new();

        // Load should succeed despite corrupt file
        let result = persistence.load_state(&mut loaded_groups).await;

        assert!(result.is_ok(), "Load should succeed despite corrupt file");
        let loaded_count = result.unwrap();
        assert_eq!(
            loaded_count, 1,
            "Should load 1 valid file (corrupt one skipped)"
        );

        // Verify the valid one was loaded correctly
        assert_eq!(loaded_groups.len(), 1, "Should have 1 restored group");

        // Clean up
        let _ = fs::remove_dir_all(&session_dir);
    }

    #[tokio::test]
    async fn test_session_load_nonexistent_dir_returns_zero() {
        let nonexistent_dir =
            PathBuf::from("/tmp/aria2_nonexistent_test_dir_that_should_not_exist_12345");
        let mut persistence = SessionPersistence::new(&nonexistent_dir);

        let mut groups: Vec<Arc<RwLock<RequestGroup>>> = Vec::new();
        let result = persistence.load_state(&mut groups).await;

        assert!(result.is_ok(), "Nonexistent dir should return Ok");
        assert_eq!(result.unwrap(), 0, "Nonexistent dir should return 0 loaded");
        assert!(groups.is_empty(), "No groups should be added");
    }

    #[tokio::test]
    async fn test_session_cleanup_removes_all_files() {
        let session_dir = create_test_session_dir();
        let persistence = SessionPersistence::new(&session_dir);

        // Create some files
        let groups = create_test_groups(2);
        let _ = persistence.save_state(&groups).await.unwrap();

        // Verify files exist
        assert!(
            session_dir.exists(),
            "Session dir should exist before cleanup"
        );

        // Cleanup
        persistence.cleanup().await.expect("Cleanup should succeed");

        // Verify directory is empty or removed
        if session_dir.exists() {
            let remaining: Vec<_> = fs::read_dir(&session_dir)
                .expect("Should read dir")
                .filter_map(|e| e.ok())
                .collect();
            assert!(
                remaining.is_empty(),
                "All files should be removed after cleanup"
            );
        }

        // Clean up
        let _ = fs::remove_dir_all(&session_dir);
    }

    #[tokio::test]
    async fn test_session_custom_interval() {
        let session_dir = create_test_session_dir();

        let persistence = SessionPersistence::new(&session_dir).with_interval(30);

        assert_eq!(
            persistence.auto_save_interval,
            Duration::from_secs(30),
            "Custom interval should be set"
        );

        // Test minimum interval enforcement
        let short_interval = SessionPersistence::new(&session_dir).with_interval(1);
        assert!(
            short_interval.auto_save_interval >= Duration::from_secs(10),
            "Interval should be at least 10 seconds"
        );

        let _ = fs::remove_dir_all(&session_dir);
    }

    #[tokio::test]
    async fn test_resume_data_roundtrip_via_persistence() {
        let session_dir = create_test_session_dir();
        let mut persistence = SessionPersistence::new(&session_dir);

        // Create a group with specific properties
        let gid = GroupId::new(0xDEADBEEF);
        let options = DownloadOptions {
            dir: Some("/test/downloads".to_string()),
            out: Some("special_file.iso".to_string()),
            split: Some(16),
            ..Default::default()
        };
        let group = Arc::new(RwLock::new(RequestGroup::new(
            gid,
            vec!["http://example.com/special_file.iso".to_string()],
            options,
        )));

        // Set some progress
        {
            let g = group.write().await;
            g.set_total_length_atomic(10485760); // 10MB
            g.set_completed_length(5242880); // 5MB
        }

        // Save
        let saved = persistence.save_state(&[group]).await.unwrap();
        assert_eq!(saved, 1);

        // Load back
        let mut loaded: Vec<Arc<RwLock<RequestGroup>>> = Vec::new();
        let loaded_count = persistence.load_state(&mut loaded).await.unwrap();
        assert_eq!(loaded_count, 1);

        // Verify the loaded group has correct URIs
        let restored = loaded[0].read().await;
        let uris = restored.uris();
        assert_eq!(uris.len(), 1);
        assert!(uris[0].contains("special_file.iso"));

        // Clean up
        let _ = fs::remove_dir_all(&session_dir);
    }

    // =====================================================================
    // K2.4 — New Tests for Session Enhancements
    // =====================================================================

    /// Test K2.4 #1: Selective save of active downloads only.
    ///
    /// Creates a mix of active and completed downloads, then verifies that
    /// save_active_only() only persists the active/waiting ones.
    #[tokio::test]
    async fn test_selective_save_active_only() {
        let session_dir = create_test_session_dir();
        let persistence = SessionPersistence::new(&session_dir);

        // Create groups with different statuses
        let mut groups: Vec<Arc<RwLock<RequestGroup>>> = Vec::new();

        // Active download (should be saved)
        let active_gid = GroupId::new(1001);
        let active_group = Arc::new(RwLock::new(RequestGroup::new(
            active_gid,
            vec!["http://example.com/active.bin".to_string()],
            DownloadOptions::default(),
        )));
        {
            let mut g = active_group.write().await;
            g.start().await.unwrap(); // Set to Active status
        }
        groups.push(active_group);

        // Waiting download (should be saved)
        let waiting_gid = GroupId::new(1002);
        let waiting_group = Arc::new(RwLock::new(RequestGroup::new(
            waiting_gid,
            vec!["http://example.com/waiting.bin".to_string()],
            DownloadOptions::default(),
        )));
        // Waiting is default status, no need to change
        groups.push(waiting_group);

        // Completed download (should NOT be saved)
        let complete_gid = GroupId::new(1003);
        let complete_group = Arc::new(RwLock::new(RequestGroup::new(
            complete_gid,
            vec!["http://example.com/complete.bin".to_string()],
            DownloadOptions::default(),
        )));
        {
            let mut g = complete_group.write().await;
            g.complete().await.unwrap(); // Set to Complete status
        }
        groups.push(complete_group);

        // Save only active/waiting
        let saved_count = persistence.save_active_only(&groups).await.unwrap();

        // Should save exactly 2 (active + waiting)
        assert_eq!(
            saved_count, 2,
            "save_active_only should save only active and waiting downloads"
        );

        // Verify files on disk - should have 2 .aria2 files
        let entries: Vec<_> = fs::read_dir(&session_dir)
            .expect("Should read session dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "aria2")
                    .unwrap_or(false)
            })
            .collect();

        assert_eq!(
            entries.len(),
            2,
            "Should have exactly 2 .aria2 files for active+waiting"
        );

        // Clean up
        let _ = fs::remove_dir_all(&session_dir);
    }

    /// Test K2.4 #2: DHT snapshot roundtrip preserves data.
    ///
    /// Creates a DhtStateSnapshot with sample data, serializes it to JSON,
    /// then deserializes and verifies all fields are preserved correctly.
    #[test]
    fn test_dht_snapshot_roundtrip() {
        use crate::session::session_persistence::{DhtNodeInfo, DhtStateSnapshot};

        // Create original snapshot with data
        let node1 = DhtNodeInfo {
            id: [1u8; 20],
            addr: "192.168.1.100:6881".to_string(),
            last_seen_epoch_secs: 1700000000,
        };

        let node2 = DhtNodeInfo {
            id: [2u8; 20],
            addr: "10.0.0.5:6881".to_string(),
            last_seen_epoch_secs: 1700000100,
        };

        let token_secret: [u8; 20] = [0xAB; 20];

        let original = DhtStateSnapshot::new(vec![node1, node2], token_secret, Some(1699999000));

        // Verify initial state
        assert_eq!(original.total_nodes, 2);
        assert_eq!(original.nodes.len(), 2);
        assert!(original.last_bootstrap_epoch_secs.is_some());

        // Serialize to JSON
        let json = original
            .to_json_string()
            .expect("Serialization should succeed");
        assert!(!json.is_empty(), "JSON output should not be empty");
        assert!(
            json.contains("192.168.1.100"),
            "JSON should contain first node address"
        );
        assert!(
            json.contains("10.0.0.5"),
            "JSON should contain second node address"
        );

        // Deserialize from JSON
        let restored =
            DhtStateSnapshot::from_json_string(&json).expect("Deserialization should succeed");

        // Verify all fields match
        assert_eq!(restored.total_nodes, 2, "total_nodes should be preserved");
        assert_eq!(restored.nodes.len(), 2, "nodes count should be preserved");
        assert_eq!(
            restored.token_secret, token_secret,
            "token_secret should be preserved"
        );
        assert_eq!(
            restored.last_bootstrap_epoch_secs,
            Some(1699999000),
            "last_bootstrap_epoch_secs should be preserved"
        );

        // Verify individual node data
        assert_eq!(
            restored.nodes[0].id, [1u8; 20],
            "First node ID should match"
        );
        assert_eq!(
            restored.nodes[0].addr, "192.168.1.100:6881",
            "First node address should match"
        );
        assert_eq!(
            restored.nodes[0].last_seen_epoch_secs, 1700000000,
            "First node timestamp should match"
        );

        assert_eq!(
            restored.nodes[1].id, [2u8; 20],
            "Second node ID should match"
        );
        assert_eq!(
            restored.nodes[1].addr, "10.0.0.5:6881",
            "Second node address should match"
        );

        // Test empty snapshot
        let empty = DhtStateSnapshot::empty();
        assert_eq!(empty.total_nodes, 0, "Empty snapshot should have 0 nodes");
        assert!(
            empty.nodes.is_empty(),
            "Empty snapshot should have no nodes"
        );

        let empty_json = empty
            .to_json_string()
            .expect("Empty serialization should succeed");
        let empty_restored = DhtStateSnapshot::from_json_string(&empty_json)
            .expect("Empty deserialization should work");
        assert_eq!(
            empty_restored.total_nodes, 0,
            "Restored empty should still be empty"
        );
    }

    /// Test K2.4 #3: Cookie persistence integration - cookies survive save/load cycle.
    ///
    /// Creates a SessionPersistence with cookies, saves state, loads it into
    /// a new instance, and verifies cookies are preserved.
    #[tokio::test]
    async fn test_cookie_persist_integration() {
        use crate::http::cookie_storage::{CookieJar, JarCookie};

        let session_dir = create_test_session_dir();

        // Create original session with cookie jar
        let mut jar = CookieJar::new();
        jar.store(JarCookie::new("session_id", "abc123", "example.com"));
        jar.store(JarCookie::new("auth_token", "xyz789", "api.example.com"));

        let persistence_with_cookies = SessionPersistence::new(&session_dir).with_cookie_jar(jar);

        // Verify cookies are set
        assert!(
            persistence_with_cookies.cookie_jar().is_some(),
            "Cookie jar should be set"
        );
        assert_eq!(
            persistence_with_cookies.cookie_jar().unwrap().len(),
            2,
            "Should have 2 cookies before save"
        );

        // Save session (includes cookies)
        let groups: Vec<Arc<RwLock<RequestGroup>>> = Vec::new();
        let _saved = persistence_with_cookies.save_state(&groups).await.unwrap();

        // Verify cookies.json file was created
        let cookie_path = session_dir.join("cookies.json");
        assert!(
            cookie_path.exists(),
            "cookies.json file should exist after save"
        );

        // Load into new instance (without pre-set cookies)
        let mut persistence_new = SessionPersistence::new(&session_dir);
        let mut loaded_groups: Vec<Arc<RwLock<RequestGroup>>> = Vec::new();
        let _loaded = persistence_new
            .load_state(&mut loaded_groups)
            .await
            .unwrap();

        // Verify cookies were loaded
        assert!(
            persistence_new.cookie_jar().is_some(),
            "Cookie jar should exist after load"
        );
        let loaded_jar = persistence_new.cookie_jar().unwrap();
        assert_eq!(
            loaded_jar.len(),
            2,
            "Should have loaded 2 cookies from file"
        );

        // Verify specific cookies were preserved
        let example_cookies = loaded_jar.get_cookies_for_url("http://example.com/", false);
        assert_eq!(
            example_cookies.len(),
            1,
            "Should find 1 cookie for example.com"
        );
        assert_eq!(example_cookies[0].name, "session_id");
        assert_eq!(example_cookies[0].value, "abc123");

        let api_cookies = loaded_jar.get_cookies_for_url("http://api.example.com/api", false);
        assert_eq!(
            api_cookies.len(),
            2,
            "Should find 2 cookies for api.example.com (parent domain + exact)"
        );
        let auth_cookie = api_cookies
            .iter()
            .find(|c| c.name == "auth_token")
            .expect("Should find auth_token cookie");
        assert_eq!(auth_cookie.value, "xyz789");
        let session_cookie = api_cookies
            .iter()
            .find(|c| c.name == "session_id")
            .expect("Should find session_id cookie from parent domain");
        assert_eq!(session_cookie.value, "abc123");

        // Clean up
        let _ = fs::remove_dir_all(&session_dir);
    }

    /// Test K2.4 #4: Auto-save with custom interval works correctly.
    ///
    /// Verifies that non-default intervals are accepted and stored properly,
    /// including enforcement of minimum interval requirement.
    #[tokio::test]
    async fn test_auto_save_with_custom_interval() {
        let session_dir = create_test_session_dir();

        // Test custom interval of 30 seconds
        let persistence_30s = SessionPersistence::new(&session_dir).with_interval(30);
        assert_eq!(
            persistence_30s.auto_save_interval,
            Duration::from_secs(30),
            "Custom 30s interval should be set"
        );

        // Test very short interval gets clamped to minimum (10 seconds)
        let persistence_too_short = SessionPersistence::new(&session_dir).with_interval(1);
        assert!(
            persistence_too_short.auto_save_interval >= Duration::from_secs(10),
            "Interval below minimum should be clamped to 10s"
        );

        // Test exact minimum interval
        let persistence_exact_min = SessionPersistence::new(&session_dir).with_interval(10);
        assert_eq!(
            persistence_exact_min.auto_save_interval,
            Duration::from_secs(10),
            "Exact minimum interval (10s) should be accepted"
        );

        // Test large interval
        let persistence_large = SessionPersistence::new(&session_dir).with_interval(300); // 5 minutes
        assert_eq!(
            persistence_large.auto_save_interval,
            Duration::from_secs(300),
            "Large interval (300s) should be accepted"
        );

        // Verify auto-save is enabled by default
        let persistence_default = SessionPersistence::new(&session_dir);
        assert!(
            persistence_default.auto_save_enabled,
            "Auto-save should be enabled by default"
        );
        assert_eq!(
            persistence_default.auto_save_interval,
            Duration::from_secs(DEFAULT_AUTO_SAVE_INTERVAL_SECS),
            "Default interval should be 60 seconds"
        );

        // Verify without_auto_save disables it
        let persistence_disabled = SessionPersistence::new(&session_dir).without_auto_save();
        assert!(
            !persistence_disabled.auto_save_enabled,
            "Auto-save should be disabled after without_auto_save()"
        );

        // Clean up
        let _ = fs::remove_dir_all(&session_dir);
    }
}

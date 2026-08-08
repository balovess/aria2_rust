//! Session management for download persistence
//!
//! This module handles saving and restoring download sessions:
//! - Restoring incomplete downloads from session files
//! - Saving session state on shutdown
//! - Mapping session entries to download options

use super::App;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::session::active_session::ActiveSessionManager;
use aria2_core::util::rwlock_ext::RwLockRecover;
use std::path::PathBuf;
use std::time::Duration;
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

        info!("Restoring download tasks from session file: {}", input_file);

        let mgr = ActiveSessionManager::new(
            session_path.clone(),
            Duration::from_secs(60), // Default interval, not used during restore
        );

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

            // Map SessionEntry options to DownloadOptions
            let opts = Self::map_entry_to_download_options(&entry.options);

            info!(
                "Restoring download task: GID={:x}, URIs={:?}, progress={}/{}",
                entry.gid, entry.uris, entry.completed_length, entry.total_length
            );

            // Add group through RequestGroupMan
            {
                let man = self.request_man.read().await;
                let gid = GroupId::new(entry.gid);
                match man.add_group_with_gid(gid, entry.uris.clone(), opts) {
                    Ok(()) => {
                        restored_count += 1;
                        info!("Successfully restored task #{}", gid.value());

                        // Store BT bitfield if present
                        if let Some(group_lock) = man.get_group(gid) {
                            let group = group_lock.recover_mut();
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
        let interval = self
            .get_opt_i64("save-session-interval")
            .await
            .unwrap_or(crate::constants::DEFAULT_SAVE_SESSION_INTERVAL_SECS as i64)
            .max(crate::constants::MIN_SESSION_INTERVAL_SECS as i64); // At least 1 second

        let mgr = ActiveSessionManager::new(session_path, Duration::from_secs(interval as u64));

        // Snapshot group handles before the asynchronous file write. The
        // manager lock must not be held while session serialization performs
        // filesystem I/O, otherwise RPC mutations can be blocked at shutdown.
        let groups = {
            let man = self.request_man.read().await;
            man.list_groups()
        };

        if groups.is_empty() {
            info!("No active download tasks, skipping session save");
            return Ok(Some(0));
        }

        match mgr.save_session(&groups).await {
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
}

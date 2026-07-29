//! Selective save methods for SessionPersistence (active-only, completed-only).

use std::sync::Arc;

use tracing::{debug, warn};

use crate::engine::resume_data::{ResumeData, ResumeDataExt};
use crate::request::request_group::{DownloadStatus, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

use super::types::SessionPersistence;

impl SessionPersistence {
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
}

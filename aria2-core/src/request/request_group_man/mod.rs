//! Request group manager with active/reserved/stopped queue split.
//!
//! Mirrors C++ `RequestGroupMan` which uses two `IndexedList`s:
//! `requestGroups_` (active) and `reservedGroups_` (waiting), plus
//! `downloadResults_` (completed). The Rust version uses:
//! - `DashMap` for active groups (concurrent RPC reads)
//! - `VecDeque` for reserved groups (FIFO promotion order)
//! - `Vec` for stopped results (RPC `tellStopped` queries)

mod demotion;
mod promotion;
mod reserved;
mod stopped;

use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use tracing::{debug, info};

use reserved::ReservedQueue;
use stopped::StoppedResults;

use super::request_group::{DownloadOptions, DownloadStatus, GroupId, HaltReason, RequestGroup};
use crate::error::Result;
use crate::util::rwlock_ext::RwLockRecover;

/// Request group manager with active/reserved/stopped queue split.
///
/// In C++ aria2, `RequestGroupMan` uses `IndexedList` for both active
/// and reserved groups. In Rust, we use `DashMap` for active groups
/// (enabling lock-free concurrent RPC reads) and `VecDeque` for reserved
/// groups (FIFO order with O(1) front removal during promotion).
pub struct RequestGroupMan {
    /// Active downloads — currently running with at least one in-flight command.
    /// Uses DashMap for concurrent RPC reads without blocking the engine loop.
    active: DashMap<GroupId, Arc<std::sync::RwLock<RequestGroup>>>,

    /// Reserved (waiting) downloads — queued but not yet started.
    pub(super) reserved: ReservedQueue,

    /// Completed/failed downloads — stored for RPC `tellStopped`.
    pub(super) stopped: StoppedResults,

    /// Maximum number of concurrent active downloads.
    /// 0 means unlimited. Mirrors C++ `maxConcurrentDownloads_`.
    max_concurrent: AtomicU32,

    /// Next GID for auto-generated group IDs.
    next_gid: AtomicU64,

    /// Global download speed limit (bytes/sec).
    global_download_limit: std::sync::RwLock<Option<u64>>,

    /// Global upload speed limit (bytes/sec).
    global_upload_limit: std::sync::RwLock<Option<u64>>,
}

impl RequestGroupMan {
    pub fn new() -> Self {
        info!("Initializing request group manager");

        RequestGroupMan {
            active: DashMap::new(),
            reserved: ReservedQueue::new(),
            stopped: StoppedResults::new(),
            max_concurrent: AtomicU32::new(5), // Default matching aria2
            next_gid: AtomicU64::new(1),
            global_download_limit: std::sync::RwLock::new(None),
            global_upload_limit: std::sync::RwLock::new(None),
        }
    }

    // ── Group Addition ──────────────────────────────────────────────────

    /// Add a new download group to the reserved queue.
    /// The engine will promote it to active when a slot is available.
    /// Returns the generated GID.
    pub fn add_group(&self, uris: Vec<String>, options: DownloadOptions) -> Result<GroupId> {
        let gid = self.generate_gid();
        let group = RequestGroup::new(gid, uris, options);
        self.reserved
            .push_back(Arc::new(std::sync::RwLock::new(group)));

        info!("Adding download task #{} (reserved)", gid.value());
        debug!(
            "Current reserved: {}, active: {}",
            self.reserved.len(),
            self.active.len()
        );

        Ok(gid)
    }

    /// Add an already-constructed `RequestGroup` (wrapped in `Arc<RwLock>`)
    /// to the reserved queue. Used by `EngineCommand::AddDownload` which
    /// creates the group externally (e.g. from an RPC `addUri` call).
    pub fn add_group_arc(&self, group: Arc<std::sync::RwLock<RequestGroup>>) {
        let gid = group.recover().gid();
        self.reserved.push_back(group);
        info!(
            "Adding download task #{} (reserved, pre-constructed)",
            gid.value()
        );
    }

    /// Insert a download group under a caller-chosen GID (used by RPC).
    /// Returns `Err` if the GID already exists.
    pub fn add_group_with_gid(
        &self,
        gid: GroupId,
        uris: Vec<String>,
        options: DownloadOptions,
    ) -> Result<()> {
        if self.find_group(gid).is_some() {
            return Err(crate::error::Aria2Error::DownloadFailed(format!(
                "GID {} already exists",
                gid.to_hex_string()
            )));
        }
        let group = RequestGroup::new(gid, uris, options);
        self.reserved
            .push_back(Arc::new(std::sync::RwLock::new(group)));
        info!(
            "Adding download task (RPC) #{} (reserved)",
            gid.to_hex_string()
        );
        Ok(())
    }

    // ── Group Lookup ────────────────────────────────────────────────────

    /// Find a group by numeric GID, searching both active and reserved.
    pub fn find_group(&self, gid: GroupId) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        // Check active first (most common case), then reserved.
        self.active
            .get(&gid)
            .map(|v| v.clone())
            .or_else(|| self.reserved.find_by_gid(gid))
    }

    /// Look up a group by its hex GID string (RPC convention).
    pub fn group_by_hex(&self, hex: &str) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        let gid = GroupId::from_hex_string(hex)?;
        self.find_group(gid)
    }

    /// Look up a group by numeric GID.
    pub fn group_by_id(&self, gid: GroupId) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        self.find_group(gid)
    }

    // ── Group Removal ───────────────────────────────────────────────────

    /// Remove a group by numeric GID from either active or reserved.
    pub fn remove_group_by_id(&self, gid: GroupId) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        // Try active first, then reserved.
        self.active
            .remove(&gid)
            .map(|(_, v)| v)
            .or_else(|| self.reserved.remove_by_gid(gid))
    }

    pub fn remove_group(&self, gid: GroupId) -> Result<()> {
        if let Some(group_lock) = self.remove_group_by_id(gid) {
            let mut group = group_lock.recover_mut();
            group.remove()?;
            info!("Removing download task #{}", gid.value());
            debug!(
                "Remaining: active={}, reserved={}",
                self.active.len(),
                self.reserved.len()
            );
        }
        Ok(())
    }

    // ── Pause/Unpause ───────────────────────────────────────────────────

    pub fn pause_group(&self, gid: GroupId) -> Result<()> {
        if let Some(group_lock) = self.find_group(gid) {
            let mut group = group_lock.recover_mut();
            group.pause()?;
            info!("Pausing download task #{}", gid.value());
        }
        Ok(())
    }

    pub fn unpause_group(&self, gid: GroupId) -> Result<()> {
        if let Some(group_lock) = self.find_group(gid) {
            let mut group = group_lock.recover_mut();
            if group.status().is_paused() {
                group.resume()?;
                info!("Resuming download task #{}", gid.value());
            }
        }
        Ok(())
    }

    pub fn force_pause_group(&self, gid: GroupId) -> Result<()> {
        if let Some(group_lock) = self.find_group(gid) {
            let mut group = group_lock.recover_mut();
            group.force_pause()?;
        }
        Ok(())
    }

    pub fn pause_all(&self) {
        for entry in self.active.iter() {
            let mut group = entry.recover_mut();
            let _ = group.pause();
        }
        // Also pause reserved groups that haven't started yet.
        for group in self.reserved.iter_snapshot() {
            let mut group = group.recover_mut();
            let _ = group.pause();
        }
    }

    pub fn force_pause_all(&self) {
        for entry in self.active.iter() {
            let mut group = entry.recover_mut();
            let _ = group.force_pause();
        }
        for group in self.reserved.iter_snapshot() {
            let mut group = group.recover_mut();
            let _ = group.force_pause();
        }
    }

    pub fn unpause_all(&self) {
        for entry in self.active.iter() {
            let mut group = entry.recover_mut();
            let _ = group.resume();
        }
        for group in self.reserved.iter_snapshot() {
            let mut group = group.recover_mut();
            let _ = group.resume();
        }
    }

    // ── Halt ────────────────────────────────────────────────────────────

    pub fn halt_all(&self, reason: HaltReason) {
        for entry in self.active.iter() {
            let group = entry.recover();
            group.request_halt(reason);
        }
    }

    pub fn force_halt_all(&self, reason: HaltReason) {
        for entry in self.active.iter() {
            let group = entry.recover();
            group.request_force_halt(reason);
        }
    }

    // ── Option Updates ──────────────────────────────────────────────────

    pub fn update_group_options(
        &self,
        gid_hex: &str,
        changes: HashMap<String, serde_json::Value>,
    ) -> std::result::Result<(), String> {
        let group = self
            .group_by_hex(gid_hex)
            .ok_or_else(|| format!("GID {} not found", gid_hex))?;

        let mut g = group.recover_mut();
        for (key, value) in changes {
            let applied = g.update_option(&key, value);
            if !applied {
                return Err(format!("Option '{}' cannot be changed at runtime", key));
            }
        }
        Ok(())
    }

    // ── Query Methods ───────────────────────────────────────────────────

    pub fn get_group(&self, gid: GroupId) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        self.find_group(gid)
    }

    pub fn is_group_active(&self, gid_hex: &str) -> std::result::Result<bool, String> {
        let group = self
            .group_by_hex(gid_hex)
            .ok_or_else(|| format!("GID {} not found", gid_hex))?;
        let g = group.recover();
        Ok(g.status().is_active())
    }

    /// Snapshot of all groups (active + reserved) as Arc clones.
    pub fn all_groups(&self) -> Vec<(GroupId, Arc<std::sync::RwLock<RequestGroup>>)> {
        let mut result: Vec<_> = self
            .active
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();

        for group in self.reserved.iter_snapshot() {
            let gid = group.recover().gid();
            result.push((gid, group));
        }

        result
    }

    /// Snapshot of all groups as Arc clones (without GID key).
    pub fn list_groups(&self) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
        let mut result: Vec<_> = self
            .active
            .iter()
            .map(|entry| entry.value().clone())
            .collect();

        result.extend(self.reserved.iter_snapshot());
        result
    }

    pub fn get_active_groups(&self) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
        self.active
            .iter()
            .filter(|entry| {
                let g = entry.recover();
                matches!(g.status(), DownloadStatus::Active)
            })
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub fn get_waiting_groups(&self) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
        self.reserved
            .iter_snapshot()
            .into_iter()
            .filter(|g| {
                matches!(
                    g.recover().status(),
                    DownloadStatus::Waiting | DownloadStatus::Paused
                )
            })
            .collect()
    }

    /// Total number of groups (active + reserved).
    pub fn count(&self) -> usize {
        self.active.len() + self.reserved.len()
    }

    /// Number of groups in the stopped results storage.
    pub fn stopped_count(&self) -> usize {
        self.stopped.len()
    }

    // ── Max Concurrent ──────────────────────────────────────────────────

    /// Get the maximum concurrent download limit.
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent.load(Ordering::Relaxed) as usize
    }

    /// Set the maximum concurrent download limit.
    /// 0 means unlimited.
    pub fn set_max_concurrent(&self, max: u32) {
        self.max_concurrent.store(max, Ordering::Relaxed);
        info!(
            "Max concurrent downloads set to {}",
            if max == 0 {
                "unlimited".to_string()
            } else {
                max.to_string()
            }
        );
    }

    // ── Global Speed Limits ─────────────────────────────────────────────

    pub fn set_global_speed_limit(&self, download_limit: Option<u64>, upload_limit: Option<u64>) {
        *self.global_download_limit.recover_mut() = download_limit;
        *self.global_upload_limit.recover_mut() = upload_limit;

        debug!(
            "Setting global speed limit - download: {:?}, upload: {:?}",
            download_limit, upload_limit
        );
    }

    pub fn global_download_limit(&self) -> Option<u64> {
        *self.global_download_limit.recover()
    }

    pub fn global_upload_limit(&self) -> Option<u64> {
        *self.global_upload_limit.recover()
    }

    // ── Clear Completed ─────────────────────────────────────────────────

    pub fn clear_completed(&self) -> Result<usize> {
        // Remove completed/errored groups from active.
        let to_remove: Vec<GroupId> = self
            .active
            .iter()
            .filter_map(|entry| {
                let group = entry.recover();
                if matches!(
                    group.status(),
                    DownloadStatus::Complete | DownloadStatus::Error(_)
                ) {
                    Some(*entry.key())
                } else {
                    None
                }
            })
            .collect();

        let mut count = to_remove.len();
        for gid in &to_remove {
            self.active.remove(gid);
        }

        // Also purge stopped results.
        count += self.stopped.purge_all();

        info!("Cleared {} completed tasks", count);
        Ok(count)
    }

    // ── Stopped Results Access ──────────────────────────────────────────

    /// Access stopped results for RPC `tellStopped`.
    pub fn get_stopped_results(
        &self,
        offset: i32,
        count: usize,
    ) -> Vec<crate::request::request_group::DownloadResult> {
        self.stopped.get_range(offset, count)
    }

    /// Find a stopped result by GID hex string.
    pub fn find_stopped_result(
        &self,
        hex: &str,
    ) -> Option<crate::request::request_group::DownloadResult> {
        self.stopped.find_by_hex(hex)
    }

    /// Remove a stopped result by GID hex string.
    pub fn remove_stopped_result(
        &self,
        hex: &str,
    ) -> Option<crate::request::request_group::DownloadResult> {
        self.stopped.remove_by_hex(hex)
    }

    /// Prune excess stopped results, keeping at most `max` entries.
    /// Mirrors C++ `purgeDownloadResult()` triggered by a timer.
    /// Returns the number of pruned results.
    pub fn prune_stopped_results(&self, max: usize) -> usize {
        let count = self.stopped.len();
        if count > max {
            let excess = count - max;
            self.stopped.remove_oldest(excess)
        } else {
            0
        }
    }

    // ── Internal ────────────────────────────────────────────────────────

    fn generate_gid(&self) -> GroupId {
        let gid = self.next_gid.fetch_add(1, Ordering::SeqCst);
        GroupId(gid)
    }

    /// Check whether all downloads are finished (no active, no reserved).
    /// Mirrors C++ `RequestGroupMan::downloadFinished()`.
    pub fn download_finished(&self) -> bool {
        self.active.is_empty() && self.reserved.is_empty()
    }
}

impl Default for RequestGroupMan {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    /// Test concurrent add_group operations
    #[test]
    fn test_concurrent_add_groups() {
        let man = Arc::new(RequestGroupMan::new());
        let num_tasks = 100;
        let mut handles = vec![];

        for i in 0..num_tasks {
            let man_clone = man.clone();
            let handle = thread::spawn(move || {
                let uri = format!("http://example.com/file{}.bin", i);
                let options = DownloadOptions::default();
                man_clone.add_group(vec![uri], options)
            });
            handles.push(handle);
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join()).collect();

        for result in results {
            assert!(result.is_ok());
            let gid = result.unwrap().unwrap();
            assert!(gid.value() > 0);
        }

        assert_eq!(man.count(), num_tasks);
    }

    /// Test that groups go to reserved queue by default.
    #[test]
    fn test_add_group_goes_to_reserved() {
        let man = RequestGroupMan::new();
        let _gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        assert_eq!(man.active.len(), 0, "No groups should be active yet");
        assert_eq!(man.reserved.len(), 1, "Group should be in reserved queue");
        assert_eq!(man.count(), 1);
    }

    /// Test max_concurrent default and setting.
    #[test]
    fn test_max_concurrent() {
        let man = RequestGroupMan::new();
        assert_eq!(man.max_concurrent(), 5); // default
        man.set_max_concurrent(10);
        assert_eq!(man.max_concurrent(), 10);
        man.set_max_concurrent(0); // unlimited
        assert_eq!(man.max_concurrent(), 0);
    }

    /// Test find_group searches both active and reserved.
    #[test]
    fn test_find_group_searches_both() {
        let man = RequestGroupMan::new();
        let gid1 = man
            .add_group(vec!["http://a.com".to_string()], DownloadOptions::default())
            .unwrap();

        // Manually add a group to active (normally done by promotion).
        let gid2 = GroupId(999);
        let group2 = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid2,
            vec!["http://b.com".to_string()],
            DownloadOptions::default(),
        )));
        // Set it as Active
        group2.recover_mut().start().unwrap();
        man.active.insert(gid2, group2);

        // Should find gid1 in reserved.
        assert!(man.find_group(gid1).is_some());
        // Should find gid2 in active.
        assert!(man.find_group(gid2).is_some());
        // Should not find nonexistent.
        assert!(man.find_group(GroupId(12345)).is_none());
    }

    /// Test download_finished check.
    #[test]
    fn test_download_finished() {
        let man = RequestGroupMan::new();
        assert!(man.download_finished());
        man.add_group(
            vec!["http://example.com".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
        assert!(!man.download_finished());
    }
}

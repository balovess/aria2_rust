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
use tracing::{debug, info, warn};

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

    /// Insert a batch of groups at the front of the reserved queue.
    ///
    /// Mirrors C++ `RequestGroupMan::insertReservedGroup(0, nextGroups)`:
    /// child groups from `postDownloadProcessing()` are inserted at
    /// position 0 so they are promoted before other waiting downloads.
    pub fn insert_reserved_at_front(
        &self,
        groups: Vec<Arc<std::sync::RwLock<RequestGroup>>>,
    ) {
        let count = groups.len();
        self.reserved.insert_front_batch(groups);
        debug!(
            "Inserted {} groups at front of reserved queue",
            count
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

            // Record a REMOVED DownloadResult in the stopped storage so
            // `tellStopped` / `getDownloadResult` can surface it.
            // Mirrors C++ `ProcessStoppedGroup` -> `addDownloadResult(REMOVED)`.
            let result = group.create_download_result();
            self.stopped.add(result);

            debug!(
                "Remaining: active={}, reserved={}",
                self.active.len(),
                self.reserved.len()
            );
        }
        Ok(())
    }

    /// Handle a promoted group whose download task failed to spawn.
    ///
    /// `fill_from_reserver()` inserts the group into the active DashMap, but
    /// if no command can be created for it (e.g. an empty URI list or an
    /// unsupported scheme) there is no running task to ever demote it. This
    /// removes the group from active, records an error, and stores a stopped
    /// result so the group does not stay in the active list forever.
    ///
    /// Mirrors C++ `createInitialCommand()` failure handling which stops the
    /// group with an error.
    pub fn fail_spawned_group(&self, gid: GroupId, message: &str) -> bool {
        if let Some((_, group)) = self.active.remove(&gid) {
            group.recover_mut().mark_error(message.to_string());
            let result = group.recover().create_download_result();
            self.stopped.add(result);
            debug!(
                gid = gid.value(),
                "Removed failed-spawn group from active and recorded error"
            );
            true
        } else {
            warn!(gid = gid.value(), "Failed-spawn group not found in active");
            false
        }
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

    // ── Pause → reserved → unpause → re-promotion loop ─────────────────

    /// A paused active group (no more in-flight commands) must be re-queued
    /// to the reserved queue — not demoted to stopped results — and must be
    /// able to resume via unpause → promotion. This is the core "pause then
    /// unpause then resume" closed loop.
    #[test]
    fn test_paused_group_requeues_to_reserved_and_can_resume() {
        let man = RequestGroupMan::new();
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();

        // Promote to active.
        let promoted = man.fill_from_reserver();
        assert_eq!(promoted.len(), 1);
        assert_eq!(man.active.len(), 1);
        assert_eq!(man.reserved.len(), 0);

        // aria2.pause: status → Paused (num_commands is 0 because no real
        // task was spawned in this unit test).
        man.pause_group(gid).unwrap();
        let group = man.find_group(gid).unwrap();
        assert!(group.recover().status().is_paused());

        // The paused group returns to the reserved queue.
        let requeued = man.requeue_non_terminal_groups(None);
        assert_eq!(requeued, 1, "paused group should be re-queued");
        assert_eq!(man.active.len(), 0);
        assert_eq!(man.reserved.len(), 1);
        assert!(man.find_group(gid).is_some(), "paused group must still exist");

        // Unpause then promote → the download restarts.
        man.unpause_group(gid).unwrap();
        let promoted = man.fill_from_reserver();
        assert_eq!(promoted.len(), 1, "unpaused group must be re-promoted");
        assert_eq!(man.active_count(), 1);
        let status = man.find_group(gid).unwrap().recover().status();
        assert_eq!(status, DownloadStatus::Active);
    }

    /// A group paused by `reduce_to_limit()` carries the restart flag; when
    /// it is re-queued the flag must be consumed so the group auto-resumes
    /// (C++ `releaseRuntimeResource()` clears the pause request).
    #[test]
    fn test_restart_requested_group_auto_resumes_on_requeue() {
        let man = RequestGroupMan::new();
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        man.fill_from_reserver();

        // Simulate reduce_to_limit(): pause + restart request.
        {
            let group = man.find_group(gid).unwrap();
            let mut g = group.recover_mut();
            g.pause().unwrap();
            g.request_restart();
        }

        let requeued = man.requeue_non_terminal_groups(None);
        assert_eq!(requeued, 1);

        // The restart flag was consumed and the group is Waiting again.
        {
            let group = man.find_group(gid).unwrap();
            let g = group.recover();
            assert_eq!(g.status(), DownloadStatus::Waiting);
            assert!(!g.is_restart_requested(), "restart flag must be consumed");
            assert!(
                !g.is_pause_requested(),
                "restart consumption must clear the pause request"
            );
        }

        // Promotion picks it up immediately (slot permitting).
        let promoted = man.fill_from_reserver();
        assert_eq!(promoted.len(), 1);
        assert_eq!(man.active_count(), 1);
    }

    // ── Remove writes a REMOVED stopped result ──────────────────────────

    /// `aria2.remove` must record a REMOVED DownloadResult in the stopped
    /// storage so `tellStopped` / `getDownloadResult` can surface it.
    #[test]
    fn test_remove_group_writes_stopped_removed_result() {
        use crate::request::request_group::DownloadResultCode;

        let man = RequestGroupMan::new();
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();

        man.remove_group(gid).unwrap();

        assert!(man.find_group(gid).is_none(), "group must be removed");
        assert_eq!(man.stopped_count(), 1, "REMOVED result must be stored");
        let result = man
            .find_stopped_result(&gid.to_hex_string())
            .expect("stopped result must be findable by GID");
        assert_eq!(result.status, DownloadStatus::Removed);
        assert_eq!(result.code, DownloadResultCode::Removed);
    }

    // ── Spawn failure must not leave a zombie in active ─────────────────

    /// A group whose download task failed to spawn must be removed from the
    /// active list and recorded as an error instead of staying there forever.
    #[test]
    fn test_fail_spawned_group_removes_from_active_and_records_error() {
        let man = RequestGroupMan::new();
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        man.fill_from_reserver();
        assert_eq!(man.active.len(), 1);

        let ok = man.fail_spawned_group(gid, "Failed to spawn download task");
        assert!(ok, "failed-spawn group should be handled");
        assert!(man.find_group(gid).is_none(), "group must leave the manager");
        assert_eq!(man.active.len(), 0, "group must not stay in active");

        let result = man
            .find_stopped_result(&gid.to_hex_string())
            .expect("failed-spawn group must have a stopped result");
        assert!(
            matches!(result.status, DownloadStatus::Error(_)),
            "failed-spawn group must be recorded as an error, got {:?}",
            result.status
        );
    }
}

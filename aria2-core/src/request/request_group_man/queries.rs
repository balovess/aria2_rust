//! Query, result-retention, and manager bookkeeping operations.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{debug, info};

use dashmap::mapref::entry::Entry;
use tokio::sync::Notify;

use super::{ActivitySignal, GroupId, RequestGroup, RequestGroupMan};
use crate::error::Result;
use crate::request::request_group::DownloadStatus;
use crate::util::rwlock_ext::RwLockRecover;

impl RequestGroupMan {
    // ── Query Methods ───────────────────────────────────────────────────

    pub fn get_group(&self, gid: GroupId) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        self.find_group(gid)
    }

    /// Snapshot groups in the order exposed by the scheduling stores.
    ///
    /// `groups` is the canonical identity index, but its `DashMap` iteration
    /// order is intentionally unspecified. Keep the observable order from
    /// aria2's active list and reserved FIFO queue, then append a group that
    /// is only visible in the canonical index while it is moving between
    /// those stores. The set also prevents duplicates if a reader observes
    /// the two stores during the same transfer window.
    fn groups_snapshot(&self) -> Vec<(GroupId, Arc<std::sync::RwLock<RequestGroup>>)> {
        let reserved = self.reserved.iter_snapshot();
        let canonical_len = self.groups.len();
        let active_len = self.active.len();

        // Lifecycle transitions remove a group from one scheduling store
        // before inserting it into the next. In the steady state the two
        // stores are complete, so avoid rescanning the canonical index and
        // allocating a deduplication set for every status query.
        if active_len + reserved.len() == canonical_len {
            let mut snapshot = Vec::with_capacity(canonical_len);
            snapshot.extend(
                self.active
                    .iter()
                    .map(|entry| (*entry.key(), entry.value().clone())),
            );
            snapshot.extend(reserved.into_iter().map(|group| {
                let gid = group.recover().gid();
                (gid, group)
            }));
            return snapshot;
        }

        let mut snapshot = Vec::with_capacity(canonical_len);
        let mut seen = HashSet::with_capacity(self.groups.len());

        for entry in self.active.iter() {
            let gid = *entry.key();
            if seen.insert(gid) {
                snapshot.push((gid, entry.value().clone()));
            }
        }

        for group in self.reserved.iter_snapshot() {
            let gid = group.recover().gid();
            if seen.insert(gid) {
                snapshot.push((gid, group));
            }
        }

        for entry in self.groups.iter() {
            let gid = *entry.key();
            if seen.insert(gid) {
                snapshot.push((gid, entry.value().clone()));
            }
        }

        snapshot
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
        self.groups_snapshot()
    }

    /// Snapshot of all groups as Arc clones (without GID key).
    pub fn list_groups(&self) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
        self.groups_snapshot()
            .into_iter()
            .map(|(_, group)| group)
            .collect()
    }

    /// Request a durable control-file flush for every non-terminal group.
    ///
    /// The active protocol command remains the owner of its in-memory
    /// checkpoint. It consumes this request at its next durable write boundary.
    pub fn request_control_file_saves(&self) {
        for group in self.list_groups() {
            let group = group.recover();
            if matches!(
                group.status(),
                DownloadStatus::Waiting | DownloadStatus::Active | DownloadStatus::Paused
            ) {
                group.save_control_file();
            }
        }
    }

    pub fn get_active_groups(&self) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
        let groups = self
            .active
            .iter()
            .filter_map(|entry| {
                let group = entry.value().clone();
                let is_active = matches!(group.recover().status(), DownloadStatus::Active);
                is_active.then_some(group)
            })
            .collect::<Vec<_>>();
        if self.active.len() + self.reserved.len() == self.groups.len() {
            return groups;
        }

        self.groups_snapshot()
            .into_iter()
            .filter_map(|(_, group)| {
                let is_active = matches!(group.recover().status(), DownloadStatus::Active);
                is_active.then_some(group)
            })
            .collect()
    }

    pub fn get_waiting_groups(&self) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
        let groups = self
            .reserved
            .iter_snapshot()
            .into_iter()
            .filter(|group| {
                matches!(
                    group.recover().status(),
                    DownloadStatus::Waiting | DownloadStatus::Paused
                )
            })
            .collect::<Vec<_>>();
        if self.active.len() + self.reserved.len() == self.groups.len() {
            return groups;
        }

        self.groups_snapshot()
            .into_iter()
            .filter_map(|(_, group)| {
                let is_waiting = matches!(
                    group.recover().status(),
                    DownloadStatus::Waiting | DownloadStatus::Paused
                );
                is_waiting.then_some(group)
            })
            .collect()
    }

    /// Total number of groups (active + reserved).
    pub fn count(&self) -> usize {
        self.groups.len()
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
        let _lifecycle = self.lifecycle_guard();
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
            if self.active.remove(gid).is_some() {
                self.unregister_group(*gid);
            }
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

    /// Purge all stopped results and return the number removed.
    pub fn purge_stopped_results(&self) -> usize {
        self.stopped.purge_all()
    }

    /// Number of stopped results currently retained.
    pub fn stopped_results_len(&self) -> usize {
        self.stopped.len()
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

    /// Reserve the next automatically allocated GID.
    pub fn next_available_gid(&self) -> GroupId {
        self.generate_gid()
    }

    // ── Internal ────────────────────────────────────────────────────────

    pub(super) fn generate_gid(&self) -> GroupId {
        let gid = self.next_gid.fetch_add(1, Ordering::SeqCst);
        GroupId(gid)
    }

    /// Register a group in the canonical non-terminal index.
    ///
    /// Registration is an atomic insert so explicitly supplied GIDs cannot
    /// replace an existing task when multiple callers add work concurrently.
    pub(super) fn register_group(&self, group: Arc<std::sync::RwLock<RequestGroup>>) -> bool {
        let gid = group.recover().gid();
        match self.groups.entry(gid) {
            Entry::Vacant(entry) => {
                {
                    let mut group = group.recover_mut();
                    group.set_global_net_stat(Arc::clone(&self.global_net_stat));
                    group.attach_activity_signal(Arc::clone(&self.activity_signal));
                }
                entry.insert(group);
                self.download_finished_notify.notify_waiters();
                self.activity_signal.notify();
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    /// Remove a group from the canonical index after it has left the manager.
    pub(super) fn unregister_group(&self, gid: GroupId) {
        self.groups.remove(&gid);
        self.download_finished_notify.notify_waiters();
        self.activity_signal.notify();
    }

    /// Return the event signal for live group and progress snapshots.
    pub fn activity_signal(&self) -> Arc<ActivitySignal> {
        Arc::clone(&self.activity_signal)
    }

    /// Return the notification source for changes to the manager's group set.
    ///
    /// Callers must still read [`download_finished`](Self::download_finished)
    /// after every wake; the notification is only a wake-up mechanism and is
    /// not the source of truth.
    pub fn download_finished_notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.download_finished_notify)
    }

    /// Check whether all downloads are finished (no active, no reserved).
    /// Mirrors C++ `RequestGroupMan::downloadFinished()`.
    pub fn download_finished(&self) -> bool {
        self.groups.is_empty()
    }
}

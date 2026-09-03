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
use dashmap::mapref::entry::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use reserved::{PositionMode, ReservedQueue};
use stopped::StoppedResults;

pub use reserved::PositionMode as ChangePositionMode;

use super::global_net_stat::GlobalNetStat;
use super::request_group::{
    ActivitySignal, DownloadOptions, DownloadStatus, GroupId, HaltReason, RequestGroup,
};
#[cfg(all(feature = "metalink", feature = "bittorrent"))]
use crate::engine::metalink_request_graph;
use crate::error::Result;
use crate::util::rwlock_ext::RwLockRecover;

/// Request group manager with active/reserved/stopped queue split.
///
/// In C++ aria2, `RequestGroupMan` uses `IndexedList` for both active
/// and reserved groups. In Rust, we use `DashMap` for active groups
/// (enabling lock-free concurrent RPC reads) and `VecDeque` for reserved
/// groups (FIFO order with O(1) front removal during promotion).
pub struct RequestGroupMan {
    /// Canonical index of every non-terminal request group.
    ///
    /// `active` and `reserved` are scheduling stores, so moving a group
    /// between them must not make the group temporarily undiscoverable to
    /// RPC/C API callers. This index owns the lookup invariant and is removed
    /// only when a group leaves the manager for good.
    groups: DashMap<GroupId, Arc<std::sync::RwLock<RequestGroup>>>,
    /// Active downloads — currently running with at least one in-flight command.
    /// Uses DashMap for concurrent RPC reads without blocking the engine loop.
    active: DashMap<GroupId, Arc<std::sync::RwLock<RequestGroup>>>,

    /// Reserved (waiting) downloads — queued but not yet started.
    pub(super) reserved: ReservedQueue,

    /// Serializes transitions between the canonical index and the active or
    /// reserved scheduling stores. RPC lifecycle calls may run concurrently
    /// with the engine's promotion and requeue passes.
    lifecycle_lock: std::sync::Mutex<()>,

    /// Completed/failed downloads — stored for RPC `tellStopped`.
    pub(super) stopped: StoppedResults,

    /// Maximum number of concurrent active downloads.
    /// 0 means unlimited. Mirrors C++ `maxConcurrentDownloads_`.
    max_concurrent: AtomicU32,

    /// Next GID for auto-generated group IDs.
    next_gid: AtomicU64,

    /// Serializes multi-group graph insertion across RPC callers.
    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    graph_insert_lock: std::sync::Mutex<()>,

    /// Global download speed limit (bytes/sec).
    global_download_limit: std::sync::RwLock<Option<u64>>,

    /// Global upload speed limit (bytes/sec).
    global_upload_limit: std::sync::RwLock<Option<u64>>,

    /// Session transfer counters shared by all registered groups.
    global_net_stat: Arc<GlobalNetStat>,

    /// Wakes consumers waiting for the manager to become empty or non-empty.
    download_finished_notify: Arc<Notify>,

    /// Wakes snapshot observers when a group or its progress changes.
    activity_signal: Arc<ActivitySignal>,

    /// Records an explicit process-level force shutdown so the application
    /// can distinguish intentional termination from an ordinary failed run.
    force_shutdown_requested: std::sync::atomic::AtomicBool,
}

impl RequestGroupMan {
    pub fn new() -> Self {
        info!("Initializing request group manager");

        RequestGroupMan {
            groups: DashMap::new(),
            active: DashMap::new(),
            reserved: ReservedQueue::new(),
            lifecycle_lock: std::sync::Mutex::new(()),
            stopped: StoppedResults::new(),
            max_concurrent: AtomicU32::new(5), // Default matching aria2
            next_gid: AtomicU64::new(1),
            #[cfg(all(feature = "metalink", feature = "bittorrent"))]
            graph_insert_lock: std::sync::Mutex::new(()),
            global_download_limit: std::sync::RwLock::new(None),
            global_upload_limit: std::sync::RwLock::new(None),
            global_net_stat: Arc::new(GlobalNetStat::default()),
            download_finished_notify: Arc::new(Notify::new()),
            activity_signal: Arc::new(ActivitySignal::new()),
            force_shutdown_requested: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn lifecycle_guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lifecycle_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // ── Group Addition ──────────────────────────────────────────────────

    /// Add a new download group to the reserved queue.
    /// The engine will promote it to active when a slot is available.
    /// Returns the generated GID.
    pub fn add_group(&self, uris: Vec<String>, options: DownloadOptions) -> Result<GroupId> {
        let _lifecycle = self.lifecycle_guard();
        let gid = self.generate_gid();
        let memory_download = options.uses_memory_download();
        let group = RequestGroup::new(gid, uris, options);
        if memory_download {
            group.mark_in_memory_download();
        }
        let group = Arc::new(std::sync::RwLock::new(group));
        if !self.register_group(Arc::clone(&group)) {
            return Err(crate::error::Aria2Error::DownloadFailed(format!(
                "GID {} already exists",
                gid.to_hex_string()
            )));
        }
        self.reserved.push_back(group);

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
        let _lifecycle = self.lifecycle_guard();
        let gid = group.recover().gid();
        if matches!(
            group.recover().status(),
            DownloadStatus::Complete | DownloadStatus::Error(_) | DownloadStatus::Removed
        ) {
            warn!(gid = gid.value(), "Ignoring stale terminal request group");
            return;
        }
        if group.recover().options().uses_memory_download() {
            group.recover().mark_in_memory_download();
        }
        if !self.register_group(Arc::clone(&group)) {
            debug!(gid = gid.value(), "Request group is already registered");
            return;
        }
        self.next_gid
            .fetch_max(gid.value().saturating_add(1), Ordering::SeqCst);
        self.reserved.push_back(group);
        info!(
            "Adding download task #{} (reserved, pre-constructed)",
            gid.value()
        );
    }

    /// Add a fully restored group while preserving its GID and state.
    ///
    /// Unlike `add_group_arc`, this path validates identity and advances the
    /// automatic allocator before queueing the group.
    pub fn add_restored_group(&self, group: Arc<std::sync::RwLock<RequestGroup>>) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        let gid = group.recover().gid();
        if !self.register_group(Arc::clone(&group)) {
            return Err(crate::error::Aria2Error::DownloadFailed(format!(
                "GID {} already exists",
                gid.to_hex_string()
            )));
        }

        self.next_gid
            .fetch_max(gid.value().saturating_add(1), Ordering::SeqCst);
        self.reserved.push_back(group);
        info!("Restored download task #{} (reserved)", gid.to_hex_string());
        Ok(())
    }

    /// Insert a batch of groups at the front of the reserved queue.
    ///
    /// Mirrors C++ `RequestGroupMan::insertReservedGroup(0, nextGroups)`:
    /// child groups from `postDownloadProcessing()` are inserted at
    /// position 0 so they are promoted before other waiting downloads.
    pub fn insert_reserved_at_front(&self, groups: Vec<Arc<std::sync::RwLock<RequestGroup>>>) {
        let _lifecycle = self.lifecycle_guard();
        let mut registered = Vec::with_capacity(groups.len());
        for group in groups {
            let gid = group.recover().gid();
            if self.register_group(Arc::clone(&group)) {
                registered.push(group);
            } else {
                warn!(gid = gid.value(), "Ignoring duplicate child request group");
            }
        }
        let count = registered.len();
        self.reserved.insert_front_batch(registered);
        debug!("Inserted {} groups at front of reserved queue", count);
    }

    /// Insert a metadata/payload graph atomically into the reserved queue.
    /// Metadata is queued first so the payload dependency can only resolve
    /// after the prerequisite has been promoted and completed.
    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    /// Add a Metalink metadata/payload request graph atomically.
    pub fn add_metalink_graph(
        &self,
        graph: metalink_request_graph::MetalinkRequestGraph,
    ) -> Result<(GroupId, GroupId)> {
        let _lifecycle = self.lifecycle_guard();
        let _insert_guard = self.graph_insert_lock.lock().map_err(|_| {
            crate::error::Aria2Error::DownloadFailed("graph insertion lock poisoned".to_string())
        })?;
        let metadata_gid = graph.metadata.recover().gid();
        let payload_gid = graph.payload.recover().gid();
        if metadata_gid == payload_gid {
            return Err(crate::error::Aria2Error::DownloadFailed(
                "Metalink graph metadata and payload must have distinct GIDs".to_string(),
            ));
        }
        if !self.register_group(Arc::clone(&graph.metadata)) {
            return Err(crate::error::Aria2Error::DownloadFailed(
                "Metalink graph contains a duplicate GID".to_string(),
            ));
        }
        if !self.register_group(Arc::clone(&graph.payload)) {
            self.unregister_group(metadata_gid);
            return Err(crate::error::Aria2Error::DownloadFailed(
                "Metalink graph contains a duplicate GID".to_string(),
            ));
        }
        self.next_gid.fetch_max(
            metadata_gid
                .value()
                .max(payload_gid.value())
                .saturating_add(1),
            Ordering::SeqCst,
        );
        self.reserved
            .push_back_batch([graph.metadata, graph.payload]);
        info!(
            metadata_gid = metadata_gid.value(),
            payload_gid = payload_gid.value(),
            "Added Metalink request graph"
        );
        Ok((metadata_gid, payload_gid))
    }

    /// Insert a download group under a caller-chosen GID (used by RPC).
    /// Returns `Err` if the GID already exists.
    pub fn add_group_with_gid(
        &self,
        gid: GroupId,
        uris: Vec<String>,
        options: DownloadOptions,
    ) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        let memory_download = options.uses_memory_download();
        let group = RequestGroup::new(gid, uris, options);
        if memory_download {
            group.mark_in_memory_download();
        }
        let group = Arc::new(std::sync::RwLock::new(group));
        if !self.register_group(Arc::clone(&group)) {
            return Err(crate::error::Aria2Error::DownloadFailed(format!(
                "GID {} already exists",
                gid.to_hex_string()
            )));
        }
        // Keep automatically generated GIDs ahead of explicitly assigned ones.
        self.next_gid
            .fetch_max(gid.value().saturating_add(1), Ordering::SeqCst);
        self.reserved.push_back(group);
        info!(
            "Adding download task (RPC) #{} (reserved)",
            gid.to_hex_string()
        );
        Ok(())
    }

    // ── Group Lookup ────────────────────────────────────────────────────

    /// Find a non-terminal group by numeric GID.
    ///
    /// The scheduling stores may be changing concurrently, so lookup must not
    /// derive identity by probing `active` and then `reserved`. The canonical
    /// index remains populated while a group moves between those stores.
    pub fn find_group(&self, gid: GroupId) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        self.groups.get(&gid).map(|entry| entry.value().clone())
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

    /// Change a reserved group's queue position and return its new index.
    pub fn change_position(
        &self,
        gid: GroupId,
        pos: i32,
        mode: ChangePositionMode,
    ) -> Result<usize> {
        let _lifecycle = self.lifecycle_guard();
        if self.reserved.is_empty() {
            return Err(crate::error::Aria2Error::InvalidArgument(
                "reserved queue is empty".to_string(),
            ));
        }
        if pos < 0 && matches!(mode, PositionMode::SetFromStart | PositionMode::SetFromEnd) {
            return Err(crate::error::Aria2Error::InvalidArgument(
                "position must not be negative for absolute modes".to_string(),
            ));
        }
        let position = self
            .reserved
            .change_position(gid, pos, mode)
            .ok_or_else(|| {
                crate::error::Aria2Error::InvalidArgument(
                    "group is not in the reserved queue".to_string(),
                )
            })?;
        self.activity_signal.notify();
        Ok(position)
    }

    // ── Group Removal ───────────────────────────────────────────────────

    /// Remove a group by numeric GID from either active or reserved.
    pub fn remove_group_by_id(&self, gid: GroupId) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        let _lifecycle = self.lifecycle_guard();
        // Try active first, then reserved.
        let removed = self
            .active
            .remove(&gid)
            .map(|(_, v)| v)
            .or_else(|| self.reserved.remove_by_gid(gid));
        if removed.is_some() {
            self.unregister_group(gid);
        }
        removed
    }

    pub fn remove_group(&self, gid: GroupId) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        // Keep active groups in requestGroups_ and only mark them for halt;
        // RequestGroupMan removes them after their last command exits.
        if let Some(group_lock) = self.active.get(&gid).map(|entry| entry.value().clone()) {
            let group = group_lock.recover();
            group.request_halt(HaltReason::UserRequest);
            info!("Requested removal of active download task #{}", gid.value());
            return Ok(());
        }

        self.remove_reserved_group(gid)
    }

    /// Remove a reserved group while the lifecycle transition lock is held.
    fn remove_reserved_group(&self, gid: GroupId) -> Result<()> {
        let group_lock = self.reserved.find_by_gid(gid).ok_or_else(|| {
            crate::error::Aria2Error::InvalidArgument(format!("GID {} not found", gid.value()))
        })?;

        // Match aria2_original's removeDownload() contract: a reserved group
        // whose dependency is unresolved cannot be removed independently. The
        // prerequisite graph must first reach a terminal state so the manager
        // can resolve or fail the dependent payload coherently.
        if !group_lock.recover().is_dependency_resolved() {
            return Err(crate::error::Aria2Error::InvalidArgument(format!(
                "GID#{} cannot be removed now",
                gid.to_hex_string()
            )));
        }

        // A reserved group has no command to drain, so it can be removed now.
        if let Some(group_lock) = self.reserved.remove_by_gid(gid) {
            let mut group = group_lock.recover_mut();
            group.remove()?;
            self.unregister_group(gid);
            info!("Removing reserved download task #{}", gid.value());
            self.stopped.add(group.create_download_result());
        }
        Ok(())
    }

    /// Request immediate removal of an active group.
    ///
    /// The engine still owns task abortion and completion accounting; this
    /// method only publishes the C++ force-halt intent on the group.
    pub fn force_remove_group(&self, gid: GroupId) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        if let Some(group_lock) = self.active.get(&gid).map(|entry| entry.value().clone()) {
            group_lock
                .recover()
                .request_force_halt(HaltReason::UserRequest);
            info!(
                "Requested force removal of active download task #{}",
                gid.value()
            );
            return Ok(());
        }
        // Reserved groups have no in-flight task, so remove them synchronously.
        self.remove_reserved_group(gid)
    }

    /// Mark an active command as timed out while preserving finalization.
    pub fn timeout_group(&self, gid: GroupId) -> bool {
        if let Some(group_lock) = self.active.get(&gid).map(|entry| entry.value().clone()) {
            let group = group_lock.recover();
            group.request_halt(HaltReason::Timeout);
            group.set_last_error(
                crate::request::request_group::DownloadResultCode::TimeOut,
                "Download timed out",
            );
            return true;
        }
        false
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
        let _lifecycle = self.lifecycle_guard();
        if let Some((_, group)) = self.active.remove(&gid) {
            self.unregister_group(gid);
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

    /// Remove a reserved dependency payload that cannot ever be promoted.
    ///
    /// This is deliberately separate from `fail_spawned_group`: no command
    /// exists yet, so leaving the group in `reserved` would make it appear as
    /// waiting forever and prevent the engine from reaching an idle state.
    pub(super) fn fail_reserved_group_with_code(
        &self,
        gid: GroupId,
        code: crate::request::request_group::DownloadResultCode,
        message: String,
    ) -> bool {
        let _lifecycle = self.lifecycle_guard();
        let Some(group) = self.reserved.remove_by_gid(gid) else {
            warn!(gid = gid.value(), "Failed reserved group not found");
            return false;
        };

        self.unregister_group(gid);
        group.recover().mark_error_with_code(code, message);
        self.stopped.add(group.recover().create_download_result());
        info!(
            gid = gid.value(),
            "Recorded failed reserved dependency group"
        );
        true
    }

    #[cfg(feature = "bittorrent")]
    pub(super) fn fail_reserved_group(&self, gid: GroupId, message: &str) -> bool {
        self.fail_reserved_group_with_code(
            gid,
            crate::request::request_group::DownloadResultCode::BittorrentParseError,
            message.to_string(),
        )
    }

    /// Return both sides of a standard Metalink metadata/payload graph.
    ///
    /// Session restore materializes the metadata prerequisite and payload as
    /// separate Rust groups, while the persisted task identity is the
    /// metadata GID. Keep lifecycle operations on that identity coherent
    /// without treating arbitrary `belongs_to` follow children as one task.
    fn metalink_graph_groups(&self, gid: GroupId) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
        let Some(target) = self.find_group(gid) else {
            return Vec::new();
        };

        let (metadata_gid, payload_gid) =
            if let Some(metadata_info) = target.recover().metadata_info() {
                let Some(metadata_gid) = metadata_info.gid() else {
                    return vec![Arc::clone(&target)];
                };
                let Some(metadata) = self.find_group(metadata_gid) else {
                    return vec![Arc::clone(&target)];
                };
                if metadata.recover().belongs_to_gid() != Some(gid) {
                    return vec![Arc::clone(&target)];
                }
                (metadata_gid, gid)
            } else if let Some(payload_gid) = target.recover().belongs_to_gid() {
                let Some(payload) = self.find_group(payload_gid) else {
                    return vec![Arc::clone(&target)];
                };
                let Some(metadata_gid) = payload
                    .recover()
                    .metadata_info()
                    .and_then(|info| info.gid())
                else {
                    return vec![Arc::clone(&target)];
                };
                if metadata_gid != gid {
                    return vec![Arc::clone(&target)];
                }
                (gid, payload_gid)
            } else {
                return vec![Arc::clone(&target)];
            };

        let Some(metadata) = self.find_group(metadata_gid) else {
            return vec![Arc::clone(&target)];
        };
        let Some(payload) = self.find_group(payload_gid) else {
            return vec![Arc::clone(&target)];
        };
        vec![metadata, payload]
    }

    // ── Pause/Unpause ───────────────────────────────────────────────────

    pub fn pause_group(&self, gid: GroupId) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        let group_lock = self.find_group(gid).ok_or_else(|| {
            crate::error::Aria2Error::InvalidArgument(format!("GID {} not found", gid.value()))
        })?;
        let group = group_lock.recover();
        if !matches!(
            group.status(),
            DownloadStatus::Active | DownloadStatus::Waiting
        ) {
            return Err(crate::error::Aria2Error::InvalidArgument(format!(
                "GID#{} cannot be paused now",
                gid.to_hex_string()
            )));
        }
        drop(group);
        for group_lock in self.metalink_graph_groups(gid) {
            group_lock.recover_mut().pause()?;
        }
        info!("Pausing download task #{}", gid.value());
        Ok(())
    }

    pub fn unpause_group(&self, gid: GroupId) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        let group_lock = self.find_group(gid).ok_or_else(|| {
            crate::error::Aria2Error::InvalidArgument(format!("GID {} not found", gid.value()))
        })?;
        let group = group_lock.recover();
        if !group.status().is_paused() {
            return Err(crate::error::Aria2Error::InvalidArgument(format!(
                "GID#{} cannot be unpaused now",
                gid.to_hex_string()
            )));
        }
        drop(group);
        for group_lock in self.metalink_graph_groups(gid) {
            group_lock.recover_mut().resume()?;
        }
        info!("Resuming download task #{}", gid.value());
        Ok(())
    }

    pub fn force_pause_group(&self, gid: GroupId) -> Result<()> {
        let _lifecycle = self.lifecycle_guard();
        let group_lock = self.find_group(gid).ok_or_else(|| {
            crate::error::Aria2Error::InvalidArgument(format!("GID {} not found", gid.value()))
        })?;
        let group = group_lock.recover();
        if !matches!(
            group.status(),
            DownloadStatus::Active | DownloadStatus::Waiting
        ) {
            return Err(crate::error::Aria2Error::InvalidArgument(format!(
                "GID#{} cannot be paused now",
                gid.to_hex_string()
            )));
        }
        drop(group);
        for group_lock in self.metalink_graph_groups(gid) {
            group_lock.recover_mut().force_pause()?;
        }
        Ok(())
    }

    pub fn pause_all(&self) {
        let _lifecycle = self.lifecycle_guard();
        let gids: Vec<_> = self.groups.iter().map(|entry| *entry.key()).collect();
        let mut visited = HashSet::with_capacity(gids.len());
        for gid in gids {
            for group_lock in self.metalink_graph_groups(gid) {
                let related_gid = group_lock.recover().gid();
                if visited.insert(related_gid) {
                    let _ = group_lock.recover_mut().pause();
                }
            }
        }
    }

    pub fn force_pause_all(&self) {
        let _lifecycle = self.lifecycle_guard();
        let gids: Vec<_> = self.groups.iter().map(|entry| *entry.key()).collect();
        let mut visited = HashSet::with_capacity(gids.len());
        for gid in gids {
            for group_lock in self.metalink_graph_groups(gid) {
                let related_gid = group_lock.recover().gid();
                if visited.insert(related_gid) {
                    let _ = group_lock.recover_mut().force_pause();
                }
            }
        }
    }

    pub fn unpause_all(&self) {
        let _lifecycle = self.lifecycle_guard();
        let gids: Vec<_> = self.groups.iter().map(|entry| *entry.key()).collect();
        let mut visited = HashSet::with_capacity(gids.len());
        for gid in gids {
            for group_lock in self.metalink_graph_groups(gid) {
                let related_gid = group_lock.recover().gid();
                if visited.insert(related_gid) {
                    let _ = group_lock.recover_mut().resume();
                }
            }
        }
    }

    // ── Halt ────────────────────────────────────────────────────────────

    pub fn halt_all(&self, reason: HaltReason) {
        for entry in self.groups.iter() {
            let group = entry.recover();
            group.request_halt(reason);
        }
    }

    pub fn force_halt_all(&self, reason: HaltReason) {
        if matches!(reason, HaltReason::ShutdownSignal) {
            self.force_shutdown_requested.store(true, Ordering::Release);
        }
        for entry in self.groups.iter() {
            let group = entry.recover();
            group.request_force_halt(reason);
        }
    }

    /// Whether the process was explicitly asked to force-shutdown.
    pub fn force_shutdown_requested(&self) -> bool {
        self.force_shutdown_requested.load(Ordering::Acquire)
    }

    /// Remove all groups that have not started yet.
    ///
    /// A force shutdown must not leave queued work behind while the engine is
    /// terminating. Active groups are deliberately untouched here: the engine
    /// still owns their command handles and will force-halt them through
    /// [`Self::force_halt_all`].
    pub fn force_remove_reserved(&self) -> usize {
        let _lifecycle = self.lifecycle_guard();
        let groups = self.reserved.drain();
        let removed = groups.len();
        for group_lock in groups {
            let gid = group_lock.recover().gid();
            let mut group = group_lock.recover_mut();
            let _ = group.remove();
            self.unregister_group(gid);
            self.stopped.add(group.create_download_result());
        }
        if removed > 0 {
            info!(removed, "Removed reserved downloads during force shutdown");
        }
        removed
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

        group.recover_mut().apply_runtime_options(changes)
    }

    /// Apply task-level runtime changes with aria2-compatible
    /// immediate/pending semantics. The lifecycle transition belongs here so
    /// RPC, C API, and future adapters cannot disagree about when a change is
    /// visible or when an active command must be restarted.
    pub fn change_group_options(
        &self,
        gid_hex: &str,
        changes: HashMap<String, serde_json::Value>,
    ) -> std::result::Result<(), String> {
        let group = self
            .group_by_hex(gid_hex)
            .ok_or_else(|| format!("GID {} not found", gid_hex))?;
        let gid = group.recover().gid();
        let classified = group.recover().classify_runtime_options(changes)?;
        let should_restart =
            !classified.pending.is_empty() && group.recover().status().is_running();

        {
            let mut group = group.recover_mut();
            group.apply_runtime_options(classified.immediate)?;
            if !classified.pending.is_empty() {
                group.set_pending_options(classified.pending);
            }
        }

        if should_restart {
            self.pause_group(gid).map_err(|error| error.to_string())?;
            self.find_group(gid)
                .ok_or_else(|| format!("GID {} disappeared during option update", gid.value()))?
                .recover()
                .request_restart();
        }
        Ok(())
    }

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

    fn generate_gid(&self) -> GroupId {
        let gid = self.next_gid.fetch_add(1, Ordering::SeqCst);
        GroupId(gid)
    }

    /// Register a group in the canonical non-terminal index.
    ///
    /// Registration is an atomic insert so explicitly supplied GIDs cannot
    /// replace an existing task when multiple callers add work concurrently.
    fn register_group(&self, group: Arc<std::sync::RwLock<RequestGroup>>) -> bool {
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
    fn unregister_group(&self, gid: GroupId) {
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

    #[test]
    fn stale_terminal_add_command_does_not_reinsert_group() {
        let man = RequestGroupMan::new();
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        let group = man.find_group(gid).unwrap();

        // A queued AddDownload can outlive a concurrent remove. Replaying it
        // must not put a terminal group back into the canonical index.
        man.remove_group(gid).unwrap();
        assert!(man.find_group(gid).is_none());

        man.add_group_arc(group);

        assert!(man.find_group(gid).is_none());
        assert_eq!(man.reserved.len(), 0);
        assert_eq!(man.stopped_count(), 1);
    }

    #[test]
    fn add_group_arc_marks_memory_download_from_options() {
        let man = RequestGroupMan::new();
        let options = DownloadOptions {
            follow_metalink: Some(crate::request::request_group::FollowMode::Memory),
            ..DownloadOptions::default()
        };
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(73),
            vec!["http://example.com/index.meta4".to_string()],
            options,
        )));

        assert!(!group.recover().is_in_memory_download());
        man.add_group_arc(Arc::clone(&group));

        assert!(
            group.recover().is_in_memory_download(),
            "pre-constructed groups must honor memory-backed metadata options"
        );
    }

    #[test]
    fn control_file_save_requests_skip_terminal_groups() {
        let man = RequestGroupMan::new();
        let gids: Vec<_> = (0..6)
            .map(|index| {
                man.add_group(
                    vec![format!("http://example.com/file{index}.bin")],
                    DownloadOptions::default(),
                )
                .unwrap()
            })
            .collect();

        man.fill_from_reserver();
        man.find_group(gids[1])
            .unwrap()
            .recover_mut()
            .pause()
            .unwrap();
        man.find_group(gids[2]).unwrap().recover().mark_complete();
        man.find_group(gids[3])
            .unwrap()
            .recover()
            .mark_error("failed".to_string());
        man.find_group(gids[4]).unwrap().recover().mark_removed();

        man.request_control_file_saves();

        assert!(
            man.find_group(gids[0])
                .unwrap()
                .recover()
                .is_save_control_file_requested()
        );
        assert!(
            man.find_group(gids[1])
                .unwrap()
                .recover()
                .is_save_control_file_requested()
        );
        assert!(
            man.find_group(gids[5])
                .unwrap()
                .recover()
                .is_save_control_file_requested()
        );
        for gid in &gids[2..5] {
            assert!(
                !man.find_group(*gid)
                    .unwrap()
                    .recover()
                    .is_save_control_file_requested()
            );
        }
    }

    #[test]
    fn registered_group_receives_session_transfer_counters() {
        let man = RequestGroupMan::new();
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        let group = man.find_group(gid).expect("registered group");
        let stats = group
            .recover()
            .global_net_stat()
            .expect("manager counters must be injected");

        stats.update_download(7);

        assert_eq!(stats.session_download_length_for_test(), 7);
    }

    #[tokio::test]
    async fn activity_signal_wakes_for_registration_and_progress_changes() {
        let man = RequestGroupMan::new();
        let activity = man.activity_signal();
        let mut observed = activity.generation();

        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            activity.wait_for_change(&mut observed),
        )
        .await
        .expect("group registration must wake activity observers");
        assert!(man.find_group(gid).is_some());

        let group = man.find_group(gid).expect("registered group");
        let previous_generation = observed;
        group.recover().update_progress(1);

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            activity.wait_for_change(&mut observed),
        )
        .await
        .expect("progress changes must wake activity observers");
        assert!(observed > previous_generation);
        assert_eq!(group.recover().get_completed_length(), 1);
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn test_add_metalink_graph_is_metadata_first_and_dependency_gated() {
        let man = RequestGroupMan::new();
        let graph = crate::engine::metalink_request_graph::MetalinkRequestGraph::new(
            "https://example.test/file.torrent",
            "file.bin",
            &DownloadOptions::default(),
            GroupId::new(42),
            GroupId::new(43),
        )
        .unwrap();
        let (metadata_gid, payload_gid) = man.add_metalink_graph(graph).unwrap();
        assert_eq!(metadata_gid, GroupId::new(42));
        assert_eq!(payload_gid, GroupId::new(43));
        assert_eq!(
            man.reserved.iter_snapshot()[0].recover().gid(),
            metadata_gid
        );
        assert_eq!(man.reserved.iter_snapshot()[1].recover().gid(), payload_gid);
        assert!(
            !man.find_group(payload_gid)
                .unwrap()
                .recover()
                .is_dependency_resolved()
        );
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn test_add_metalink_graph_rejects_duplicate_without_insertion() {
        let man = RequestGroupMan::new();
        man.add_group_with_gid(
            GroupId::new(42),
            vec!["https://example.test/existing".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();
        let graph = crate::engine::metalink_request_graph::MetalinkRequestGraph::new(
            "https://example.test/file.torrent",
            "file.bin",
            &DownloadOptions::default(),
            GroupId::new(42),
            GroupId::new(43),
        )
        .unwrap();
        assert!(man.add_metalink_graph(graph).is_err());
        assert!(man.find_group(GroupId::new(43)).is_none());
    }

    #[test]
    fn test_add_group_with_gid_preserves_gid_and_advances_allocator() {
        let man = RequestGroupMan::new();
        let explicit_gid = GroupId::new(42);

        man.add_group_with_gid(
            explicit_gid,
            vec!["http://example.com/file.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();

        assert!(man.find_group(explicit_gid).is_some());
        let generated_gid = man
            .add_group(
                vec!["http://example.com/next.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        assert!(generated_gid.value() > explicit_gid.value());
    }

    #[test]
    fn test_add_restored_group_preserves_gid_and_advances_allocator() {
        let man = RequestGroupMan::new();
        let gid = GroupId::new(0x2a);
        let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            gid,
            vec!["http://example.com/restored.bin".to_string()],
            DownloadOptions::default(),
        )));

        man.add_restored_group(group).unwrap();
        assert!(man.find_group(gid).is_some());
        assert!(man.generate_gid().value() > gid.value());
    }

    #[test]
    fn test_add_restored_group_rejects_duplicate_gid() {
        let man = RequestGroupMan::new();
        let gid = GroupId::new(0x2a);
        let group = || {
            Arc::new(std::sync::RwLock::new(RequestGroup::new(
                gid,
                vec!["http://example.com/restored.bin".to_string()],
                DownloadOptions::default(),
            )))
        };

        man.add_restored_group(group()).unwrap();
        assert!(man.add_restored_group(group()).is_err());
    }

    #[test]
    fn test_dependency_blocks_promotion_until_metadata_completes() {
        let man = RequestGroupMan::new();
        let metadata_gid = GroupId::new(10);
        let payload_gid = GroupId::new(11);
        let metadata = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            metadata_gid,
            vec!["https://example.test/file.torrent".to_string()],
            DownloadOptions::default(),
        )));
        let payload = Arc::new(std::sync::RwLock::new(RequestGroup::new(
            payload_gid,
            vec!["bt://payload".to_string()],
            DownloadOptions::default(),
        )));
        payload.recover().set_dependency(Box::new(
            crate::request::request_group::CompletionDependency::new(metadata_gid),
        ));
        payload.recover().set_belongs_to_gid(metadata_gid);

        man.add_restored_group(Arc::clone(&metadata)).unwrap();
        man.add_restored_group(Arc::clone(&payload)).unwrap();

        let promoted = man.fill_from_reserver();
        assert_eq!(promoted.len(), 1);
        assert_eq!(promoted[0].recover().gid(), metadata_gid);
        assert!(man.find_group(payload_gid).is_some());
        assert!(!payload.recover().is_dependency_resolved());

        man.resolve_dependencies_for(metadata_gid);
        let promoted = man.fill_from_reserver();
        assert_eq!(promoted.len(), 1);
        assert_eq!(promoted[0].recover().gid(), payload_gid);
    }

    #[test]
    fn failed_completion_dependency_does_not_leave_reserved_group_stuck() {
        use crate::request::request_group::DownloadResultCode;

        for (prerequisite_status, expected_message) in [
            (
                DownloadStatus::Error("metadata failed".to_string()),
                "completion dependency failed: metadata failed",
            ),
            (DownloadStatus::Removed, "completion dependency was removed"),
        ] {
            let man = RequestGroupMan::new();
            let prerequisite_gid = GroupId::new(60);
            let dependent_gid = GroupId::new(61);
            let prerequisite = Arc::new(std::sync::RwLock::new(RequestGroup::new(
                prerequisite_gid,
                vec!["http://example.com/prerequisite.bin".to_string()],
                DownloadOptions::default(),
            )));
            let dependent = Arc::new(std::sync::RwLock::new(RequestGroup::new(
                dependent_gid,
                vec!["http://example.com/dependent.bin".to_string()],
                DownloadOptions::default(),
            )));
            dependent.recover().set_dependency(Box::new(
                crate::request::request_group::CompletionDependency::new(prerequisite_gid),
            ));
            man.add_restored_group(prerequisite).unwrap();
            man.add_restored_group(dependent).unwrap();

            man.resolve_dependencies_for_status(prerequisite_gid, prerequisite_status);

            assert!(
                man.find_group(dependent_gid).is_none(),
                "failed dependency must leave the canonical group index"
            );
            let result = man
                .find_stopped_result(&dependent_gid.to_hex_string())
                .expect("failed dependency must be recorded as stopped");
            assert_eq!(result.code, DownloadResultCode::UnknownError);
            assert_eq!(
                result.status,
                DownloadStatus::Error(expected_message.to_string())
            );
        }
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn test_failed_metadata_with_direct_fallback_releases_payload() {
        let man = RequestGroupMan::new();
        let graph = crate::engine::metalink_request_graph::MetalinkRequestGraph::new_with_fallback(
            "https://example.test/file.torrent",
            "file.bin",
            &DownloadOptions::default(),
            GroupId::new(30),
            GroupId::new(31),
            vec!["https://mirror.test/file.bin".to_string()],
        )
        .unwrap();
        man.add_metalink_graph(graph).unwrap();

        man.resolve_dependencies_for_status(
            GroupId::new(30),
            DownloadStatus::Error("metadata unavailable".to_string()),
        );

        let payload = man.find_group(GroupId::new(31)).expect("payload retained");
        assert!(payload.recover().is_dependency_resolved());
        assert_eq!(
            payload.recover().uris().iter().map(|uri| uri.as_ref()).collect::<Vec<_>>(),
            ["https://mirror.test/file.bin"]
        );
    }

    #[cfg(feature = "metalink")]
    #[test]
    fn completed_stopped_result_includes_followed_by_child_gids() {
        let man = RequestGroupMan::new();
        let parent_gid = man
            .add_group(
                vec!["https://example.test/index.meta4".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        let parent = man.find_group(parent_gid).expect("parent group");
        parent
            .recover()
            .set_content_type("application/metalink4+xml");
        parent.recover().set_in_memory_data(
            br#"<?xml version="1.0"?><metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="payload.bin"><url>https://example.test/payload.bin</url></file></metalink>"#.to_vec(),
        );

        man.fill_from_reserver();
        parent.recover().mark_complete();

        let demoted = man.remove_stopped_groups(None);

        assert_eq!(demoted, vec![parent_gid]);
        let result = man
            .find_stopped_result(&parent_gid.to_hex_string())
            .expect("completed result must be stored");
        assert_eq!(result.followed_by.len(), 1);
        let child_gid = result.followed_by[0];
        assert!(child_gid != parent_gid);
        assert!(man.find_group(child_gid).is_some());
        assert_eq!(man.reserved.len(), 1);
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn test_failed_torrent_only_metadata_is_stopped_as_error() {
        let man = RequestGroupMan::new();
        let graph = crate::engine::metalink_request_graph::MetalinkRequestGraph::new(
            "https://example.test/file.torrent",
            "file.bin",
            &DownloadOptions::default(),
            GroupId::new(40),
            GroupId::new(41),
        )
        .unwrap();
        man.add_metalink_graph(graph).unwrap();

        man.resolve_dependencies_for_status(
            GroupId::new(40),
            DownloadStatus::Error("metadata unavailable".to_string()),
        );

        assert!(man.find_group(GroupId::new(41)).is_none());
        assert_eq!(
            man.find_stopped_result(&GroupId::new(41).to_hex_string())
                .map(|result| result.code),
            Some(crate::request::request_group::DownloadResultCode::BittorrentParseError)
        );
    }

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn test_remove_rejects_dependency_blocked_metalink_payload() {
        let man = RequestGroupMan::new();
        let metadata_gid = GroupId::new(50);
        let payload_gid = GroupId::new(51);
        let graph = crate::engine::metalink_request_graph::MetalinkRequestGraph::new(
            "https://example.test/file.torrent",
            "file.bin",
            &DownloadOptions::default(),
            metadata_gid,
            payload_gid,
        )
        .unwrap();
        man.add_metalink_graph(graph).unwrap();

        let error = man
            .remove_group(payload_gid)
            .expect_err("an unresolved dependency cannot be removed yet");
        assert!(
            error.to_string().contains("cannot be removed now"),
            "unexpected remove error: {error}"
        );
        let force_error = man
            .force_remove_group(payload_gid)
            .expect_err("force-remove must also respect an unresolved dependency");
        assert!(
            force_error.to_string().contains("cannot be removed now"),
            "unexpected force-remove error: {force_error}"
        );
        assert!(man.find_group(metadata_gid).is_some());
        assert!(man.find_group(payload_gid).is_some());
        assert_eq!(man.stopped_count(), 0);
    }

    #[test]
    fn test_add_group_with_gid_accepts_zero_gid() {
        let man = RequestGroupMan::new();
        let zero_gid = GroupId::new(0);

        man.add_group_with_gid(
            zero_gid,
            vec!["http://example.com/zero.bin".to_string()],
            DownloadOptions::default(),
        )
        .unwrap();

        assert!(man.find_group(zero_gid).is_some());
        let generated_gid = man
            .add_group(
                vec!["http://example.com/next.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        assert_eq!(generated_gid, GroupId::new(1));
    }

    #[test]
    fn test_add_group_with_gid_rejects_duplicate_gid() {
        let man = RequestGroupMan::new();
        let gid = GroupId::new(42);
        let options = DownloadOptions::default();
        man.add_group_with_gid(
            gid,
            vec!["http://example.com/file.bin".to_string()],
            options.clone(),
        )
        .unwrap();

        let result = man.add_group_with_gid(
            gid,
            vec!["http://example.com/other.bin".to_string()],
            options,
        );
        assert!(result.is_err());
        assert_eq!(man.count(), 1);
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

    #[test]
    fn test_seed_only_groups_do_not_consume_active_limit() {
        let man = RequestGroupMan::new();
        let gid = man
            .add_group(
                vec!["magnet:?xt=urn:btih:test".to_string()],
                DownloadOptions {
                    bt_detach_seed_only: true,
                    ..DownloadOptions::default()
                },
            )
            .unwrap();
        man.fill_from_reserver();
        let group = man.find_group(gid).unwrap();
        group.recover().enable_seed_only();
        assert_eq!(man.active_count(), 0);
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

    #[test]
    fn blocked_reserved_group_does_not_starve_later_runnable_group() {
        let man = RequestGroupMan::new();
        man.set_max_concurrent(1);

        let blocked = man
            .add_group(
                vec!["http://example.com/blocked.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        man.find_group(blocked)
            .expect("blocked group should be registered")
            .recover_mut()
            .pause()
            .unwrap();

        let runnable = man
            .add_group(
                vec!["http://example.com/runnable.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();

        let promoted = man.fill_from_reserver();
        assert_eq!(promoted.len(), 1);
        assert_eq!(promoted[0].recover().gid(), runnable);
        assert!(
            man.find_group(blocked)
                .unwrap()
                .recover()
                .status()
                .is_paused()
        );
        assert_eq!(man.reserved.len(), 1);
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
        assert!(man.register_group(Arc::clone(&group2)));
        man.active.insert(gid2, group2);

        // Should find gid1 in reserved.
        assert!(man.find_group(gid1).is_some());
        // Should find gid2 in active.
        assert!(man.find_group(gid2).is_some());
        // Should not find nonexistent.
        assert!(man.find_group(GroupId(12345)).is_none());
    }

    #[test]
    fn test_find_group_stays_visible_during_queue_transfer() {
        let man = RequestGroupMan::new();
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        let promoted = man.fill_from_reserver();
        assert_eq!(promoted.len(), 1);
        let canonical = man.find_group(gid).expect("group must be indexed");

        // Exercise the two storage mutations separately. The canonical index
        // must keep the GID visible in the interval between them.
        let moved = man.active.remove(&gid).expect("group must be active").1;
        let during_active_removal = man
            .find_group(gid)
            .expect("lookup must survive active removal");
        assert!(Arc::ptr_eq(&canonical, &during_active_removal));

        man.reserved.push_front(Arc::clone(&moved));
        let during_reserved_insert = man
            .find_group(gid)
            .expect("lookup must survive reserved insertion");
        assert!(Arc::ptr_eq(&canonical, &during_reserved_insert));

        let moved_back = man.reserved.pop_front().expect("group must be reserved");
        let during_reserved_removal = man
            .find_group(gid)
            .expect("lookup must survive reserved removal");
        assert!(Arc::ptr_eq(&canonical, &during_reserved_removal));
        man.active.insert(gid, moved_back);
        assert_eq!(man.count(), 1);
    }

    #[test]
    fn test_group_snapshots_preserve_active_first_and_reserved_fifo() {
        let man = RequestGroupMan::new();
        man.set_max_concurrent(1);
        let first = man
            .add_group(
                vec!["http://example.com/first.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        let second = man
            .add_group(
                vec!["http://example.com/second.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        let third = man
            .add_group(
                vec!["http://example.com/third.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();

        assert_eq!(man.fill_from_reserver().len(), 1);
        let gids: Vec<_> = man.all_groups().into_iter().map(|(gid, _)| gid).collect();
        assert_eq!(gids, vec![first, second, third]);

        let waiting: Vec<_> = man
            .get_waiting_groups()
            .into_iter()
            .map(|group| group.recover().gid())
            .collect();
        assert_eq!(waiting, vec![second, third]);

        let active = man.active.remove(&first).expect("group must be active").1;
        let during_transfer: Vec<_> = man.all_groups().into_iter().map(|(gid, _)| gid).collect();
        assert_eq!(during_transfer, vec![second, third, first]);

        man.reserved.push_front(active);
        let after_requeue: Vec<_> = man.all_groups().into_iter().map(|(gid, _)| gid).collect();
        assert_eq!(after_requeue, vec![first, second, third]);
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
        assert!(
            man.find_group(gid).is_some(),
            "paused group must still exist"
        );

        // Unpause then promote → the download restarts.
        man.unpause_group(gid).unwrap();
        let promoted = man.fill_from_reserver();
        assert_eq!(promoted.len(), 1, "unpaused group must be re-promoted");
        assert_eq!(man.active_count(), 1);
        let status = man.find_group(gid).unwrap().recover().status();
        assert_eq!(status, DownloadStatus::Active);
    }

    #[test]
    fn paused_group_with_inflight_command_keeps_its_concurrency_slot() {
        let man = RequestGroupMan::new();
        man.set_max_concurrent(1);
        let first = man
            .add_group(
                vec!["http://example.com/first.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        let second = man
            .add_group(
                vec!["http://example.com/second.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();

        assert_eq!(man.fill_from_reserver().len(), 1);
        let first_group = man.find_group(first).unwrap();
        first_group.recover().inc_commands();
        man.pause_group(first).unwrap();

        assert_eq!(man.active_count(), 1);
        assert!(
            man.fill_from_reserver().is_empty(),
            "a paused command still draining must retain its active slot"
        );
        assert!(man.find_group(second).is_some());
        assert_eq!(man.reserved.len(), 1);
    }

    #[test]
    fn test_paused_reserved_group_is_not_promoted_until_unpaused() {
        let man = RequestGroupMan::new();
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();

        // Reproduce the race window between the pause-flag check and the
        // promotion status transition: the status is paused, but the flag
        // has already been consumed by another lifecycle operation.
        let group = man.find_group(gid).unwrap();
        {
            let mut group = group.recover_mut();
            group.pause().unwrap();
            group.control_flags.clear_pause();
        }

        let promoted = man.fill_from_reserver();
        assert!(promoted.is_empty(), "paused group must remain reserved");
        assert_eq!(man.reserved.len(), 1);
        assert_eq!(man.active.len(), 0);
        assert!(man.find_group(gid).unwrap().recover().status().is_paused());

        man.unpause_group(gid).unwrap();
        let promoted = man.fill_from_reserver();
        assert_eq!(promoted.len(), 1, "unpaused group should be promoted");
        assert_eq!(man.active_count(), 1);
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

    #[test]
    fn test_active_remove_requests_halt_without_removing_group() {
        let man = RequestGroupMan::new();
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        man.fill_from_reserver();

        man.remove_group(gid).unwrap();

        let group = man.find_group(gid).expect("active group must be retained");
        let guard = group.recover();
        assert!(guard.is_halt_requested());
        assert_eq!(guard.get_halt_reason(), HaltReason::UserRequest);
        assert_eq!(man.stopped_count(), 0);
    }

    #[test]
    fn test_force_remove_requests_force_halt_without_removing_group() {
        let man = RequestGroupMan::new();
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        man.fill_from_reserver();

        man.force_remove_group(gid).unwrap();

        let group = man.find_group(gid).expect("active group must be retained");
        let guard = group.recover();
        assert!(guard.is_force_halt_requested());
        assert_eq!(guard.get_halt_reason(), HaltReason::UserRequest);
    }

    #[test]
    fn test_force_remove_waits_for_lifecycle_transition_lock() {
        let man = Arc::new(RequestGroupMan::new());
        let gid = man
            .add_group(
                vec!["http://example.com/file.bin".to_string()],
                DownloadOptions::default(),
            )
            .unwrap();
        man.fill_from_reserver();

        let lifecycle = man.lifecycle_lock.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (completed_tx, completed_rx) = std::sync::mpsc::channel();
        let worker = Arc::clone(&man);
        let handle = thread::spawn(move || {
            started_tx.send(()).unwrap();
            completed_tx.send(worker.force_remove_group(gid)).unwrap();
        });

        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(
            completed_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "force removal must wait for the lifecycle transition lock"
        );

        drop(lifecycle);
        assert!(
            completed_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );
        handle.join().unwrap();

        let group = man
            .find_group(gid)
            .expect("active group must remain indexed");
        assert!(group.recover().is_force_halt_requested());
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
        assert!(
            man.find_group(gid).is_none(),
            "group must leave the manager"
        );
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

    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    #[test]
    fn batch_pause_operations_cover_both_metalink_graph_groups() {
        let man = RequestGroupMan::new();
        let graph = crate::engine::metalink_request_graph::MetalinkRequestGraph::new(
            "https://example.test/file.torrent",
            "file.bin",
            &DownloadOptions::default(),
            GroupId::new(81),
            GroupId::new(82),
        )
        .unwrap();
        let (metadata_gid, payload_gid) = man.add_metalink_graph(graph).unwrap();

        man.pause_all();
        for gid in [metadata_gid, payload_gid] {
            let group = man.find_group(gid).unwrap();
            assert!(group.recover().status().is_paused());
        }

        man.unpause_all();
        for gid in [metadata_gid, payload_gid] {
            let group = man.find_group(gid).unwrap();
            assert_eq!(group.recover().status(), DownloadStatus::Waiting);
        }

        man.force_pause_all();
        for gid in [metadata_gid, payload_gid] {
            let group = man.find_group(gid).unwrap();
            let group = group.recover();
            assert!(group.status().is_paused());
            assert!(group.is_force_pause_requested());
        }
    }
}

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
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64};
use tokio::sync::Notify;
use tracing::info;

use reserved::ReservedQueue;
use stopped::StoppedResults;

pub use reserved::PositionMode as ChangePositionMode;

use super::global_net_stat::GlobalNetStat;
use super::request_group::{ActivitySignal, GroupId, RequestGroup};

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
}

mod group_addition;
mod group_lookup;
mod lifecycle;
mod queries;

impl Default for RequestGroupMan {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;

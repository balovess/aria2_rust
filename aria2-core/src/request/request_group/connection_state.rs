use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use super::RequestGroup;
use crate::util::rwlock_ext::RwLockRecover;

impl RequestGroup {
    /// Return the number of currently active protocol connections.
    pub fn active_connection_count(&self) -> u32 {
        self.active_connection_count.load(Ordering::Acquire)
    }

    /// Publish the current protocol connection count for status consumers.
    pub(crate) fn set_active_connection_count(&self, count: usize) {
        self.active_connection_count
            .store(count.min(u32::MAX as usize) as u32, Ordering::Release);
        self.notify_activity_changed();
    }
}

/// Clears a protocol connection count when an async protocol operation exits.
///
/// The guard keeps cleanup local to the operation that owns the sockets. This
/// prevents early-return paths from leaving a stale connection count in RPC
/// or terminal UI snapshots.
pub(crate) struct ActiveConnectionGuard {
    group: Arc<RwLock<RequestGroup>>,
}

impl ActiveConnectionGuard {
    pub(crate) fn new(group: Arc<RwLock<RequestGroup>>) -> Self {
        group.recover().set_active_connection_count(0);
        Self { group }
    }

    pub(crate) fn set(&self, count: usize) {
        self.group.recover().set_active_connection_count(count);
    }
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.group.recover().set_active_connection_count(0);
    }
}

/// Extension trait for `std::sync::RwLock` that recovers from lock poisoning
/// instead of panicking.
///
/// Lock poisoning occurs when a thread panics while holding a write lock.
/// In a download manager, a panic in one download task should not crash
/// the entire process — the data guarded by the lock may be partially
/// modified, but continuing is preferable to terminating.
///
/// Usage:
/// ```ignore
/// use crate::util::rwlock_ext::RwLockRecover;
/// let g = group.read().recover();
/// let mut g = group.write().recover();
/// ```
use std::sync::RwLock;

pub trait RwLockRecover<T> {
    fn recover(&self) -> std::sync::RwLockReadGuard<'_, T>;
    fn recover_mut(&self) -> std::sync::RwLockWriteGuard<'_, T>;
}

impl<T> RwLockRecover<T> for RwLock<T> {
    /// Acquire a read lock, recovering from poison if necessary.
    ///
    /// If the lock is poisoned (a previous holder panicked), this returns
    /// the guard anyway — the data may be inconsistent, but continuing
    /// is better than crashing.
    fn recover(&self) -> std::sync::RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Acquire a write lock, recovering from poison if necessary.
    fn recover_mut(&self) -> std::sync::RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(|e| e.into_inner())
    }
}

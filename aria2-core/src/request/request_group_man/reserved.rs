//! Reserved (waiting) download queue operations.
//!
//! Reserved groups are downloads that have been added but not yet activated.
//! They are stored in a `VecDeque` behind a `std::sync::RwLock` to preserve
//! FIFO insertion order for promotion. When an active slot frees up, the
//! engine pops the front of this queue to activate the next download.

use std::collections::VecDeque;
use std::sync::Arc;

use super::RequestGroup;
use crate::util::rwlock_ext::RwLockRecover;

/// The reserved (waiting) download queue.
///
/// Uses `VecDeque` for O(1) front removal during promotion.
/// The `RwLock` allows concurrent reads from RPC status queries
/// while the engine loop has write access for promotion.
pub struct ReservedQueue {
    groups: std::sync::RwLock<VecDeque<Arc<std::sync::RwLock<RequestGroup>>>>,
}

impl ReservedQueue {
    /// Create an empty reserved queue.
    pub fn new() -> Self {
        Self {
            groups: std::sync::RwLock::new(VecDeque::new()),
        }
    }

    /// Add a group to the back of the reserved queue.
    pub fn push_back(&self, group: Arc<std::sync::RwLock<RequestGroup>>) {
        self.groups.recover_mut().push_back(group);
    }

    /// Add a group to the front of the reserved queue (used when re-queueing
    /// a paused group so it gets priority when unpaused).
    pub fn push_front(&self, group: Arc<std::sync::RwLock<RequestGroup>>) {
        self.groups.recover_mut().push_front(group);
    }

    /// Insert a batch of groups at the front of the reserved queue.
    ///
    /// Mirrors C++ `RequestGroupMan::insertReservedGroup(0, nextGroups)`:
    /// child groups from `postDownloadProcessing()` are inserted at position 0
    /// so they are promoted before other waiting downloads.
    ///
    /// Groups are inserted in order: the first element of `groups` will be
    /// at the front of the queue (promoted first).
    pub fn insert_front_batch(&self, groups: Vec<Arc<std::sync::RwLock<RequestGroup>>>) {
        let mut queue = self.groups.recover_mut();
        for group in groups.into_iter().rev() {
            queue.push_front(group);
        }
    }

    /// Append multiple groups while holding the queue lock once.
    #[cfg(all(feature = "metalink", feature = "bittorrent"))]
    pub fn push_back_batch(
        &self,
        groups: impl IntoIterator<Item = Arc<std::sync::RwLock<RequestGroup>>>,
    ) {
        let mut queue = self.groups.recover_mut();
        queue.extend(groups);
    }

    /// Pop the front group from the reserved queue.
    /// Returns `None` if the queue is empty.
    pub fn pop_front(&self) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        self.groups.recover_mut().pop_front()
    }

    /// Number of groups in the reserved queue.
    pub fn len(&self) -> usize {
        self.groups.recover().len()
    }

    /// Whether the reserved queue is empty.
    pub fn is_empty(&self) -> bool {
        self.groups.recover().is_empty()
    }

    /// Find a reserved group by GID.
    pub fn find_by_gid(
        &self,
        gid: crate::request::request_group::GroupId,
    ) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        self.groups
            .recover()
            .iter()
            .find(|g| g.recover().gid() == gid)
            .cloned()
    }

    /// Find a reserved group by hex GID string.
    /// Used by RPC `tellWaiting` / `changePosition`.
    #[allow(dead_code)]
    pub fn find_by_hex(&self, hex: &str) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        let gid = crate::request::request_group::GroupId::from_hex_string(hex)?;
        self.find_by_gid(gid)
    }

    /// Remove a group from the reserved queue by GID.
    pub fn remove_by_gid(
        &self,
        gid: crate::request::request_group::GroupId,
    ) -> Option<Arc<std::sync::RwLock<RequestGroup>>> {
        let mut groups = self.groups.recover_mut();
        let pos = groups.iter().position(|g| g.recover().gid() == gid)?;
        groups.remove(pos)
    }

    /// Iterate over all reserved groups (read-only snapshot).
    pub fn iter_snapshot(&self) -> Vec<Arc<std::sync::RwLock<RequestGroup>>> {
        self.groups.recover().iter().cloned().collect()
    }

    /// Change the position of a group in the reserved queue.
    /// Mirrors C++ `RequestGroupMan::changeReservedGroupPosition`.
    #[allow(dead_code)]
    pub fn change_position(
        &self,
        gid: crate::request::request_group::GroupId,
        pos: i32,
        how: PositionMode,
    ) -> Option<usize> {
        let mut groups = self.groups.recover_mut();
        let current = groups.iter().position(|g| g.recover().gid() == gid)?;

        let new_pos = match how {
            PositionMode::SetFromStart => pos as usize,
            PositionMode::MoveFromStart => (current as i32 + pos).max(0) as usize,
            PositionMode::SetFromEnd => groups.len().saturating_sub(pos as usize + 1),
            PositionMode::MoveFromEnd => {
                let from_end = groups.len() as i32 - current as i32 - 1;
                (groups.len() as i32 - from_end - pos - 1).max(0) as usize
            }
        };
        let new_pos = new_pos.min(groups.len() - 1);

        let item = groups.remove(current)?;
        groups.insert(new_pos, item);
        Some(new_pos)
    }
}

/// Position mode for `change_position`, matching C++ `OffsetMode`.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PositionMode {
    /// Set position from the start of the queue.
    SetFromStart,
    /// Move position relative to current, from the start.
    MoveFromStart,
    /// Set position from the end of the queue.
    SetFromEnd,
    /// Move position relative to current, from the end.
    MoveFromEnd,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::{DownloadOptions, GroupId};

    fn make_group(id: u64) -> Arc<std::sync::RwLock<RequestGroup>> {
        Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId(id),
            vec![format!("http://example.com/file{}.bin", id)],
            DownloadOptions::default(),
        )))
    }

    #[test]
    fn test_push_pop_fifo() {
        let q = ReservedQueue::new();
        let g1 = make_group(1);
        let g2 = make_group(2);
        q.push_back(g1);
        q.push_back(g2);
        assert_eq!(q.len(), 2);
        let first = q.pop_front().unwrap();
        assert_eq!(first.recover().gid(), GroupId(1));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn test_find_by_gid() {
        let q = ReservedQueue::new();
        q.push_back(make_group(10));
        q.push_back(make_group(20));
        let found = q.find_by_gid(GroupId(20));
        assert!(found.is_some());
        assert_eq!(found.unwrap().recover().gid(), GroupId(20));
        assert!(q.find_by_gid(GroupId(99)).is_none());
    }

    #[test]
    fn test_remove_by_gid() {
        let q = ReservedQueue::new();
        q.push_back(make_group(1));
        q.push_back(make_group(2));
        q.push_back(make_group(3));
        let removed = q.remove_by_gid(GroupId(2));
        assert!(removed.is_some());
        assert_eq!(q.len(), 2);
        assert!(q.find_by_gid(GroupId(2)).is_none());
    }
}

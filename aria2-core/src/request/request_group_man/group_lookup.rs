//! Lookup and reserved-position operations for request groups.

use std::sync::Arc;

use super::{GroupId, PositionMode, RequestGroup, RequestGroupMan};
use crate::error::Result;

impl RequestGroupMan {
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
        let gid = self.resolve_group_id(hex)?;
        self.find_group(gid)
    }

    /// Resolve a unique full GID or high-order hexadecimal prefix.
    ///
    /// This mirrors aria2's `GroupId::expandUnique`: full 16-digit IDs take
    /// the indexed path, while abbreviated IDs scan the canonical index and
    /// are accepted only when exactly one group matches.
    pub fn resolve_group_id(&self, hex: &str) -> Option<GroupId> {
        let (prefix, mask) = GroupId::hex_prefix(hex)?;
        if mask == u64::MAX {
            return self
                .groups
                .contains_key(&GroupId(prefix))
                .then_some(GroupId(prefix));
        }

        let mut matched = None;
        for entry in self.groups.iter() {
            if entry.key().0 & mask == prefix {
                if matched.is_some() {
                    return None;
                }
                matched = Some(*entry.key());
            }
        }
        matched
    }

    /// Resolve a unique GID prefix across live groups and stopped results.
    pub fn resolve_gid_hex(&self, hex: &str) -> Option<GroupId> {
        let live = self.resolve_group_id(hex);
        let stopped = self.stopped.find_gid_by_hex(hex);
        match (live, stopped) {
            (Some(live), None) => Some(live),
            (None, Some(stopped)) => Some(stopped),
            _ => None,
        }
    }

    /// Change a reserved group's queue position and return its new index.
    pub fn change_position(&self, gid: GroupId, pos: i32, mode: PositionMode) -> Result<usize> {
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
}

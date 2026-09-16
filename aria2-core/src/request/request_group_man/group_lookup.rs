//! Lookup and reserved-position operations for request groups.

use std::sync::Arc;

use super::reserved::PositionMode;
use super::{ChangePositionMode, GroupId, RequestGroup, RequestGroupMan};
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
}

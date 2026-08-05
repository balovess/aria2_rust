//! DownloadResult creation and structured error reporting.
//!
//! Mirrors C++ `RequestGroup::downloadResult()` logic which maps
//! download status, halt reason, and error codes into a structured
//! `DownloadResult` for RPC consumers.

use crate::util::rwlock_ext::RwLockRecover;

use super::download_result::DownloadResult;
use super::group_id::GroupId;
use super::halt_reason::HaltReason;
use super::result_code::DownloadResultCode;
use super::status::DownloadStatus;

impl super::RequestGroup {
    /// Create a structured `DownloadResult` for this download.
    ///
    /// Mirrors C++ `RequestGroup::downloadResult()` logic:
    /// - If download is complete and no checksum verification pending → Finished
    /// - If halt reason is UserRequest → Removed
    /// - If last error is unknown and halt reason is ShutdownSignal → InProgress
    /// - If last error is unknown → UnknownError
    /// - Otherwise → the recorded last error code
    pub fn create_download_result(&self) -> DownloadResult {
        let status = self.status.recover().clone();
        let halt_reason = *self.halt_reason.recover();
        let last_code = *self.last_error_code.recover();
        let last_msg = self.last_error_message.recover().clone();
        let gid = self.gid;

        let (code, message) = match status {
            DownloadStatus::Complete => {
                // Check if checksum verification is still pending
                let checksum_pending = self
                    .download_context
                    .recover()
                    .as_ref()
                    .map(|ctx| ctx.is_checksum_verification_pending())
                    .unwrap_or(false);

                if checksum_pending {
                    (
                        DownloadResultCode::ChecksumError,
                        "Checksum verification pending".to_string(),
                    )
                } else {
                    (DownloadResultCode::Finished, "OK".to_string())
                }
            }
            DownloadStatus::Removed => (
                DownloadResultCode::Removed,
                "Download removed by user".to_string(),
            ),
            DownloadStatus::Paused => (DownloadResultCode::Paused, "Download paused".to_string()),
            DownloadStatus::Error(_) => {
                // Use structured error code if available
                if last_code != DownloadResultCode::UnknownError {
                    (last_code, last_msg)
                } else {
                    (
                        DownloadResultCode::UnknownError,
                        "Unknown error".to_string(),
                    )
                }
            }
            DownloadStatus::Waiting | DownloadStatus::Active => {
                // Download was interrupted (e.g. by shutdown)
                match halt_reason {
                    HaltReason::ShutdownSignal => (
                        DownloadResultCode::InProgress,
                        "Download interrupted by shutdown".to_string(),
                    ),
                    HaltReason::UserRequest => (
                        DownloadResultCode::Removed,
                        "Download removed by user".to_string(),
                    ),
                    HaltReason::Timeout => (
                        DownloadResultCode::TimeOut,
                        "Download timed out".to_string(),
                    ),
                    HaltReason::None => {
                        if last_code != DownloadResultCode::UnknownError {
                            (last_code, last_msg)
                        } else {
                            (
                                DownloadResultCode::InProgress,
                                "Download interrupted".to_string(),
                            )
                        }
                    }
                }
            }
        };

        let mut result = DownloadResult::new(gid, status, code);
        result.message = message;
        result.fill_from_group(self);

        // Fill parent-child relationships from RequestGroup fields.
        // Mirrors C++ `DownloadResult::followedBy` / `following`.
        result.following = *self.following_gid.recover();
        result.followed_by = self.followed_by_gids.recover().clone();
        result.belongs_to = *self.belongs_to_gid.recover();

        result
    }

    /// Set the GID of the parent download that spawned this one.
    ///
    /// Mirrors C++ `RequestGroup::following(gid)`. Called when a
    /// post-download handler creates a child group.
    pub fn set_following_gid(&self, parent_gid: GroupId) {
        *self.following_gid.recover_mut() = Some(parent_gid);
    }

    /// Get the GID of the parent download that spawned this one.
    pub fn following_gid(&self) -> Option<GroupId> {
        *self.following_gid.recover()
    }

    /// Add a child GID to the followed-by list.
    ///
    /// Mirrors C++ `RequestGroup::followedBy()` which registers
    /// child groups on the parent. Called when a post-download handler
    /// creates child groups.
    pub fn add_followed_by_gid(&self, child_gid: GroupId) {
        let mut followed_by_gids = self.followed_by_gids.recover_mut();
        if !followed_by_gids.contains(&child_gid) {
            followed_by_gids.push(child_gid);
        }
    }

    /// Get the list of child GIDs spawned by this download.
    pub fn followed_by_gids(&self) -> Vec<GroupId> {
        self.followed_by_gids.recover().clone()
    }

    /// Set the parent GID this group belongs to.
    pub fn set_belongs_to_gid(&self, parent_gid: GroupId) {
        *self.belongs_to_gid.recover_mut() = Some(parent_gid);
    }

    /// Get the parent GID this group belongs to.
    pub fn belongs_to_gid(&self) -> Option<GroupId> {
        *self.belongs_to_gid.recover()
    }
}

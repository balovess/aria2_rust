//! Verify reusable local data against protocol-provided piece hashes.

use std::sync::Arc;

use tracing::{info, warn};

use crate::checksum::check_integrity::{self, IntegrityTrailingGarbageAction};
use crate::checksum::message_digest::HashType;
use crate::error::{Aria2Error, Result};
use crate::filesystem::resume_helper::ResumeState;
use crate::util::rwlock_ext::RwLockRecover;

use super::DownloadCommand;

impl DownloadCommand {
    /// Verify existing payload before allocation or transfer.
    ///
    /// Returns `true` when the existing file was fully verified and completed
    /// the task; a failed check clears resume offsets so execution redownloads.
    pub(super) async fn verify_existing_integrity(
        &mut self,
        total_length: u64,
        resume_state: &mut ResumeState,
    ) -> Result<bool> {
        if !self.check_integrity || total_length == 0 {
            return Ok(false);
        }

        IntegrityTrailingGarbageAction::single_file(self.output_path.clone(), total_length)
            .apply()
            .await?;

        // Copy metadata while the group lock is held, then release it before
        // awaiting the shared integrity worker.
        let piece_info = self.group.recover().get_download_context().map(|context| {
            (
                context
                    .get_piece_hashes()
                    .iter()
                    .map(|hash| hash.to_string())
                    .collect(),
                context.get_piece_length() as u64,
                context.get_piece_hash_type().to_string(),
            )
        });
        let Some((hashes, piece_len, hash_type)) = piece_info else {
            return Ok(false);
        };

        let algorithm = if hash_type.is_empty() {
            HashType::Sha1
        } else {
            HashType::from_str(&hash_type).ok_or_else(|| {
                Aria2Error::Parse(format!("unknown piece hash algorithm: {hash_type}"))
            })?
        };
        let Some(task) = check_integrity::man::file_task(
            &self.output_path,
            piece_len.max(1),
            total_length,
            hashes,
            algorithm,
        )?
        else {
            return Ok(false);
        };

        let gid = self.group.recover().gid().value();
        info!(
            gid,
            "Checking integrity of existing data against piece hashes"
        );
        let outcome = check_integrity::man::enqueue_with_outcome_for_group(
            &check_integrity::man::shared(),
            Arc::clone(&self.group),
            task,
        )
        .await?;
        if !outcome.verified {
            warn!(
                gid,
                "Integrity check failed; discarding resume state and re-downloading"
            );
            resume_state.should_resume = false;
            resume_state.start_offset = 0;
            resume_state.is_complete = false;
            return Ok(false);
        }

        info!(gid, "Integrity check passed");
        if resume_state.existing_length < total_length {
            return Ok(false);
        }

        self.completed_bytes = total_length;
        resume_state.start_offset = total_length;
        resume_state.is_complete = true;
        {
            let mut group = self.group.recover_mut();
            group.set_completed_length(total_length);
            group.complete()?;
        }
        self.completed = true;
        self.release_output_path().await;
        Ok(true)
    }
}

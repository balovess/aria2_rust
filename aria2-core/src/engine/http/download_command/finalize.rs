use std::sync::Arc;

use crate::checksum::checksum::Checksum;
use crate::checksum::message_digest::HashType;
use crate::engine::active_output_registry::global_registry;
use crate::error::{Aria2Error, Result};
use crate::util::rwlock_ext::RwLockRecover;

use super::DownloadCommand;

impl DownloadCommand {
    pub(super) async fn finalize_attempt(&mut self, download_result: Result<()>) -> Result<()> {
        self.update_tail_reclaim_progress();

        let result = async {
            if download_result.is_ok() {
                let checksum_config = {
                    let group = self.group.recover();
                    group.options().checksum.clone()
                };
                if let Some((ref algorithm, ref expected)) = checksum_config
                    && let Some(hash_type) = HashType::from_str(algorithm)
                {
                    let checksum = Checksum::new(hash_type, expected)?;
                    let total_length = self.group.recover().total_length();
                    let verified =
                        crate::checksum::check_integrity::man::enqueue_file_checksum_for_group(
                            &crate::checksum::check_integrity::man::shared(),
                            Arc::clone(&self.group),
                            &self.output_path,
                            total_length,
                            checksum,
                        )
                        .await?;
                    if !verified {
                        tracing::error!(
                            algorithm,
                            path = %self.output_path.display(),
                            "Checksum mismatch"
                        );
                        return Err(Aria2Error::Checksum(format!(
                            "{} checksum mismatch for {}",
                            algorithm,
                            self.output_path.display()
                        )));
                    }
                    tracing::info!(
                        algorithm,
                        path = %self.output_path.display(),
                        "Checksum verified successfully"
                    );
                    self.group.recover().set_checksum_verified(true);
                }

                self.completed = true;
                let group = self.group.recover();
                let total = group.total_length();
                group.update_progress(total);
                group.set_completed_length(total);
            }
            download_result
        }
        .await;

        self.release_output_path().await;
        result
    }

    pub(super) async fn release_output_path(&self) {
        global_registry().release(&self.output_path).await;
    }
}

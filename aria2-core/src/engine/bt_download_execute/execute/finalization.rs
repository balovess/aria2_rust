use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_post_download_handler::{DownloadStatus, HookContext};
use crate::error::Result;
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    /// Finalize the download: update progress, unregister from BtRegistry,
    /// announce to DHT, clean up progress file, and fire post-download hooks.
    pub(super) async fn finalize_download(
        &mut self,
        start_time: Instant,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
    ) -> Result<()> {
        let final_speed = {
            let elapsed = start_time.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                (self.completed_bytes as f64 / elapsed) as u64
            } else {
                0
            }
        };
        {
            self.progress.set_completed_length(self.completed_bytes);
            self.progress.set_download_speed(final_speed);
            self.progress.set_upload_speed(self.total_uploaded);
            self.group.recover().set_uploaded_length(self.total_uploaded);
            let mut g = self.group.recover_mut();
            g.complete()?;
        }

        info!(
            "BT command done: downloaded={} uploaded={}",
            self.completed_bytes, self.total_uploaded
        );

        // Unregister from BtRegistry. In C++ aria2, this is done when
        // DownloadEngine removes the RequestGroup. Here we do it explicitly
        // on download completion/finalization so the registry stays clean.
        if let Some(ref registry) = self.bt_registry {
            let gid = self.group.recover().gid().value();
            if let Ok(mut reg) = registry.write() {
                if reg.remove(gid) {
                    info!(gid, "Removed BT download from BtRegistry on finalization");
                }
            }
        }

        if let Some(ref engine) = self.dht_engine {
            if let Err(e) = engine.announce_peer(&meta.info_hash.bytes, 0).await {
                warn!("[BT] DHT announce failed: {}", e);
            } else {
                info!(
                    "[BT] DHT announce_peer sent for {}",
                    meta.info_hash.as_hex()
                );
            }
            engine.shutdown();
        }

        // P1 integration: clean up completed download progress file
        if let Some(ref mgr) = self.progress_manager {
            if let Err(e) = mgr.remove_progress(&meta.info_hash.bytes) {
                warn!(
                    error = %e,
                    "Failed to remove progress file after completion"
                );
            } else {
                info!("BT progress file removed after successful download");
            }
        }

        // P2 integration: trigger post-download completion hooks
        if let Some(ref hm) = self.hook_manager {
            // Get gid (extracted from group)
            let gid = {
                let g = self.group.recover();
                g.gid()
            };

            let ctx = HookContext {
                gid,
                file_path: self.output_path.clone(),
                status: DownloadStatus::Complete,
                stats: crate::engine::bt_post_download_handler::DownloadStats {
                    uploaded_bytes: self.total_uploaded,
                    downloaded_bytes: self.completed_bytes,
                    upload_speed: 0.0,
                    download_speed: final_speed as f64,
                    elapsed_seconds: start_time.elapsed().as_secs(),
                },
                error: None,
            };

            match hm.fire_complete(&ctx).await {
                Ok(results) => {
                    info!(
                        hook_count = results.len(),
                        "All post-download hooks executed successfully"
                    );
                    for result in &results {
                        debug!(result = %result, "Hook execution result");
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "Post-download hook execution failed (non-fatal)"
                    );
                }
            }
        }

        Ok(())
    }
}

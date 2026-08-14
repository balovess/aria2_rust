use std::time::Instant;
use tracing::{info, warn};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::download_event_hooks::{DownloadEvent, DownloadEventHooks};
use crate::engine::hook_manager::{DownloadStatus, HookContext};
use crate::error::Result;
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    /// Finalize a completed download and release per-torrent resources.
    pub(super) async fn finalize_download(
        &mut self,
        start_time: Instant,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
    ) -> Result<()> {
        self.return_all_checked_out_peers();
        let final_speed = elapsed_speed(self.completed_bytes, start_time);
        self.progress.set_completed_length(self.completed_bytes);
        self.progress.set_download_speed(final_speed);
        self.progress.set_upload_speed(self.total_uploaded);
        self.group
            .recover()
            .set_uploaded_length(self.total_uploaded);

        if !self.bt_complete_event_emitted {
            DownloadEventHooks::shared()
                .fire_event(DownloadEvent::BtComplete, &self.group.recover());
        }
        self.group.recover_mut().complete()?;

        info!(
            "BT command done: downloaded={} uploaded={}",
            self.completed_bytes, self.total_uploaded
        );

        if let Some(ref registry) = self.bt_registry {
            let gid = self.group.recover().gid().value();
            if let Ok(mut reg) = registry.write()
                && reg.remove(gid)
            {
                info!(gid, "Removed BT download from BtRegistry on finalization");
            }
        }

        if let Some(ref engine) = self.dht_engine {
            if let Err(error) = engine.announce_peer(&meta.info_hash.bytes, 0).await {
                warn!(%error, "BT DHT announce failed");
            } else {
                info!("BT DHT announce_peer sent for {}", meta.info_hash.as_hex());
            }
        }

        if let Some(ref manager) = self.progress_manager
            && let Err(error) = manager.remove_progress(&meta.info_hash.bytes)
        {
            warn!(%error, "Failed to remove progress file after completion");
        }

        if let Some(checkpoint) = self.checkpoint.take() {
            checkpoint.remove().await?;
        }

        if let Some(ref hooks) = self.hook_manager {
            let context = HookContext {
                gid: self.group.recover().gid(),
                file_path: self.output_path.clone(),
                status: DownloadStatus::Complete,
                stats: crate::engine::hook_manager::DownloadStats {
                    uploaded_bytes: self.total_uploaded,
                    downloaded_bytes: self.completed_bytes,
                    upload_speed: 0.0,
                    download_speed: final_speed as f64,
                    elapsed_seconds: start_time.elapsed().as_secs(),
                },
                error: None,
            };
            match hooks.fire_complete(&context).await {
                Ok(results) => info!(hook_count = results.len(), "Post-download hooks completed"),
                Err(error) => warn!(%error, "Post-download hook execution failed"),
            }
        }

        Ok(())
    }
}

fn elapsed_speed(bytes: u64, start_time: Instant) -> u64 {
    let elapsed = start_time.elapsed().as_secs_f64();
    if elapsed > 0.0 {
        (bytes as f64 / elapsed) as u64
    } else {
        0
    }
}

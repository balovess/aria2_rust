use std::time::Instant;
use tracing::{info, warn};

use crate::download::download_context::{ContextAttributeType, DownloadContext};
use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::download_event_hooks::{DownloadEvent, DownloadEventHooks};
use crate::engine::hook_manager::{DownloadStatus, HookContext};
use crate::error::Result;
use crate::request::request_group::DownloadOptions;
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

        let context = self.group.recover().get_download_context();
        if let Some(context) = context.as_deref()
            && should_remove_unselected_files(self.group.recover().options(), context)
        {
            remove_unselected_files(context);
        }

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

fn should_remove_unselected_files(options: &DownloadOptions, context: &DownloadContext) -> bool {
    options.bt_remove_unselected_file
        && !options.uses_memory_download()
        && context.has_attribute(ContextAttributeType::BitTorrent)
        && context
            .get_file_entries()
            .iter()
            .any(|entry| !entry.is_requested())
}

fn remove_unselected_files(context: &DownloadContext) {
    for entry in context
        .get_file_entries()
        .iter()
        .filter(|entry| !entry.is_requested())
    {
        let path = std::path::Path::new(entry.path());
        if path.as_os_str().is_empty() {
            continue;
        }

        match std::fs::remove_file(path) {
            Ok(()) => info!(path = %path.display(), "Removed unselected BitTorrent file"),
            Err(error) => {
                warn!(path = %path.display(), %error, "Could not remove unselected BitTorrent file")
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::download_context::{BtFileMode, TorrentAttribute};
    use crate::download::file_entry::FileEntry;

    fn bt_context(entries: Vec<FileEntry>) -> DownloadContext {
        let mut context = DownloadContext::new_default();
        context.set_file_entries(entries);
        context.set_attribute(
            ContextAttributeType::BitTorrent,
            Box::new(TorrentAttribute {
                name: "test".to_string(),
                mode: BtFileMode::Multi,
                announce_list: Vec::new(),
                nodes: Vec::new(),
                info_hash: String::new(),
                metadata: Vec::new(),
                metadata_size: 0,
                private_torrent: false,
                creation_date: 0,
                comment: String::new(),
                created_by: String::new(),
                url_list: Vec::new(),
            }),
        );
        context
    }

    #[test]
    fn removes_only_unselected_files_after_success() {
        let temp_dir = tempfile::tempdir().expect("temporary directory");
        let selected_path = temp_dir.path().join("selected.bin");
        let unselected_path = temp_dir.path().join("unselected.bin");
        std::fs::write(&selected_path, b"selected").expect("selected file");
        std::fs::write(&unselected_path, b"unselected").expect("unselected file");

        let mut selected = FileEntry::new(
            selected_path.to_string_lossy().into_owned(),
            8,
            0,
            Vec::new(),
        );
        selected.set_requested(true);
        let mut unselected = FileEntry::new(
            unselected_path.to_string_lossy().into_owned(),
            10,
            8,
            Vec::new(),
        );
        unselected.set_requested(false);
        let context = bt_context(vec![selected, unselected]);

        let options = DownloadOptions {
            bt_remove_unselected_file: true,
            ..DownloadOptions::default()
        };
        assert!(should_remove_unselected_files(&options, &context));
        remove_unselected_files(&context);

        assert!(selected_path.exists());
        assert!(!unselected_path.exists());
    }

    #[test]
    fn memory_downloads_never_remove_unselected_files() {
        let context = bt_context(vec![{
            let mut entry = FileEntry::new("missing.bin".to_string(), 1, 0, Vec::new());
            entry.set_requested(false);
            entry
        }]);
        let options = DownloadOptions {
            bt_remove_unselected_file: true,
            follow_torrent: Some(crate::request::request_group::FollowMode::Memory),
            ..DownloadOptions::default()
        };

        assert!(!should_remove_unselected_files(&options, &context));
    }
}

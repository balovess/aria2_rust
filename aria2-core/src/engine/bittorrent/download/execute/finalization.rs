use std::time::Instant;
use tracing::{info, warn};

use crate::download::download_context::{ContextAttributeType, DownloadContext};
use crate::engine::bittorrent::download::command::BtDownloadCommand;
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
        // A completed task accepts no more peers. Release its route now so a
        // replacement task can register while this command finishes cleanup.
        if self.bt_peer_route.take().is_some() {
            tracing::debug!("[BT] Incoming peer route released on completion");
        }
        let final_speed = elapsed_speed(self.completed_bytes, start_time);
        self.progress.set_completed_length(self.completed_bytes);
        self.progress.set_download_speed(final_speed);
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

        if let Some(ref manager) = self.progress_manager
            && let Err(error) = manager.remove_progress(&meta.network_info_hash())
        {
            warn!(%error, "Failed to remove progress file after completion");
        }

        if let Some(checkpoint) = self.checkpoint.take() {
            checkpoint.remove().await?;
        }

        // All output/checkpoint cleanup is complete. Release the reservation
        // before the potentially slow DHT announce so a replacement task can
        // safely reuse these paths while the old command remains alive.
        self.output_path_reservation = None;

        if let Some(port) = dht_announce_port(self.listen_port) {
            for (engine_addr, result) in self
                .dht_engines
                .announce_peer(&meta.network_info_hash(), port)
                .await
            {
                if let Err(error) = result {
                    warn!(%engine_addr, %error, "BT DHT announce failed");
                } else {
                    info!(
                        %engine_addr,
                        port,
                        "BT DHT announce_peer sent for {}",
                        meta.info_hash.as_hex()
                    );
                }
            }
        }

        if let (Some(manager), Some(info_hash)) =
            (&self.lpd_manager, self.lpd_registered_info_hash.take())
        {
            let info_hash_hex = hex::encode(info_hash);
            manager.unregister_torrent(&info_hash_hex).await;
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

fn dht_announce_port(listen_port: u16) -> Option<u16> {
    (listen_port != 0).then_some(listen_port)
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

    #[test]
    fn dht_announce_uses_actual_peer_port() {
        assert_eq!(dht_announce_port(45123), Some(45123));
        assert_eq!(dht_announce_port(0), None);
    }

    #[tokio::test]
    async fn completion_releases_route_and_output_reservation_before_drop() {
        use crate::engine::bittorrent::download::command_tests::build_test_torrent;
        use crate::engine::bittorrent::peer::listener::{BtPeerListenerManager, BtPeerRouteConfig};
        use crate::engine::bittorrent::peer::storage::DefaultPeerStorage;
        use crate::request::request_group::GroupId;
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().expect("temporary BT output directory");
        let torrent = build_test_torrent();
        let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent)
            .expect("test torrent should parse");
        let mut command = BtDownloadCommand::new(
            GroupId::new(73_201),
            &torrent,
            &DownloadOptions::default(),
            Some(dir.path().to_str().unwrap()),
        )
        .expect("test torrent should construct");
        let manager = BtPeerListenerManager::new();
        let route_config = |caretaker_id| BtPeerRouteConfig {
            bind_ip: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash: meta.network_info_hash(),
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id,
            max_peers: 1,
            peer_storage: Arc::new(Mutex::new(DefaultPeerStorage::new())),
            crypto_policy: Default::default(),
            dht_enabled: false,
        };
        let (_, _incoming, route) = manager.register(route_config(1)).await.unwrap();
        command.bt_peer_route = Some(route);
        command
            .reserve_output_paths()
            .expect("outputs should reserve");
        assert!(
            manager.register(route_config(2)).await.is_err(),
            "the running task holds the torrent's route"
        );

        command
            .group
            .recover_mut()
            .start()
            .expect("group should start");
        command
            .finalize_download(Instant::now(), &meta)
            .await
            .expect("finalization should succeed");

        // The command is still alive (its task may announce for a while
        // longer), but a new task for the same torrent can start.
        assert!(
            manager.register(route_config(3)).await.is_ok(),
            "a completed task must release the torrent's route"
        );
        let _reservation = crate::engine::active_output_registry::global_registry()
            .reserve_exact_paths(vec![command.output_path.clone()])
            .expect("a completed task must release its output reservation");
        drop(command);
    }
}

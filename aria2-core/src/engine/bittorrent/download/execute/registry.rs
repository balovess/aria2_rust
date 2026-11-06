use std::sync::Arc;

use tracing::info;

use crate::download::download_context::{ContextAttributeType, TorrentAttribute};
use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    pub(super) fn reserve_output_paths(&mut self) -> crate::error::Result<()> {
        let paths = {
            let group = self.group.recover();
            if group.options().uses_memory_download() {
                return Ok(());
            }

            group
                .get_download_context()
                .map(|context| {
                    context
                        .get_file_entries()
                        .iter()
                        .map(|entry| std::path::PathBuf::from(entry.path()))
                        .collect::<Vec<_>>()
                })
                .filter(|paths| !paths.is_empty())
                .unwrap_or_else(|| vec![self.output_path.clone()])
        };

        match crate::engine::active_output_registry::global_registry().reserve_exact_paths(paths) {
            Ok(reservation) => {
                self.output_path_reservation = Some(reservation);
                Ok(())
            }
            Err(_) => {
                let message = format!(
                    "File {} is being downloaded by other command.",
                    self.output_path.display()
                );
                self.group.recover().set_last_error(
                    crate::request::request_group::DownloadResultCode::DuplicateDownload,
                    message.clone(),
                );
                Err(crate::error::Aria2Error::DownloadFailed(message))
            }
        }
    }

    pub(super) fn register_bt_download(&mut self) -> std::result::Result<(), String> {
        let Some(registry) = self.bt_registry.as_ref() else {
            return Ok(());
        };

        let (gid, download_context, dht_external_ip) = {
            let group = self.group.recover();
            (
                group.gid().value(),
                group.get_download_context(),
                group
                    .options()
                    .bt_external_ip
                    .as_deref()
                    .and_then(|address| address.parse::<std::net::IpAddr>().ok()),
            )
        };
        let (announce_list, announce_url) = {
            if let Some(ref ctx) = download_context {
                if let Some(attr) = ctx.get_attribute(ContextAttributeType::BitTorrent) {
                    if let Some(ta) = attr.downcast_ref::<TorrentAttribute>() {
                        let list = &ta.announce_list;
                        let url = ta
                            .announce_list
                            .first()
                            .and_then(|tier| tier.first())
                            .cloned();
                        (list.clone(), url)
                    } else {
                        (Vec::new(), None)
                    }
                } else {
                    (Vec::new(), None)
                }
            } else {
                (Vec::new(), None)
            }
        };
        let bt_announce = Arc::new(
            crate::engine::bittorrent::tracker::communication::BtAnnounce::new(
                &announce_list,
                &announce_url,
            ),
        );
        // Keep the RPC view attached to the live TrackerAnnouncer.  The
        // compatibility BtAnnounce handle only contains torrent metadata and
        // cannot see public trackers appended during discovery or refresh.
        let tracker_runtime = Arc::new(std::sync::RwLock::new(
            crate::engine::bittorrent::tracker::communication::TrackerRuntimeSnapshot::from_bt_announce(&bt_announce),
        ));
        self.tracker_runtime = Some(Arc::clone(&tracker_runtime));
        let bt_object = crate::engine::bittorrent::registry::BtObject::builder()
            .bt_announce(bt_announce)
            .tracker_runtime(tracker_runtime)
            .download_context(download_context.unwrap_or_else(|| {
                Arc::new(crate::download::DownloadContext::new(0, 0, String::new()))
            }))
            .peer_rejection(self.peer_rejection.clone())
            .build();
        let mut reg = registry
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reg.put_unless_info_hash_registered(gid, bt_object)
            .map_err(|info_hash| format!("InfoHash {info_hash} is already registered."))?;
        reg.set_dht_external_ip(gid, dht_external_ip);
        info!(
            gid,
            "Registered BT download into BtRegistry with BtAnnounce"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::bittorrent::download::command_tests::build_test_torrent;
    use crate::engine::command::Command;
    use crate::request::request_group::{DownloadOptions, GroupId};

    #[test]
    fn registration_exposes_a_live_tracker_runtime_snapshot() {
        let torrent = build_test_torrent();
        let options = DownloadOptions::default();
        let mut command = BtDownloadCommand::new(GroupId::new(777), &torrent, &options, None)
            .expect("test torrent should construct");
        let registry = Arc::new(std::sync::RwLock::new(
            crate::engine::bittorrent::registry::BtRegistry::new(),
        ));
        command.set_bt_registry(Arc::clone(&registry));
        command
            .register_bt_download()
            .expect("unique BT task should register");

        let registry = registry.read().expect("BT registry should be readable");
        let object = registry.get(777).expect("BT task should be registered");
        let runtime = object
            .tracker_runtime
            .as_ref()
            .expect("registered task should expose live tracker state");
        let snapshot = runtime
            .read()
            .expect("tracker runtime snapshot should be readable");
        assert_eq!(snapshot.tracker_tiers.len(), 1);
        assert_eq!(
            snapshot.tracker_tiers[0][0],
            "http://tracker.example.com/announce"
        );
    }

    #[tokio::test]
    async fn duplicate_info_hash_registration_preserves_the_first_task() {
        let torrent = build_test_torrent();
        let options = DownloadOptions::default();
        let registry = Arc::new(std::sync::RwLock::new(
            crate::engine::bittorrent::registry::BtRegistry::new(),
        ));

        let mut first = BtDownloadCommand::new(GroupId::new(777), &torrent, &options, None)
            .expect("test torrent should construct");
        first.set_bt_registry(Arc::clone(&registry));
        first
            .register_bt_download()
            .expect("first task with this info-hash should register");
        let info_hash = first
            .group()
            .get_download_context()
            .and_then(|context| context.get_bt_info_hash_hex())
            .expect("test torrent should have an info-hash");

        let mut second = BtDownloadCommand::new(GroupId::new(778), &torrent, &options, None)
            .expect("same test torrent should construct for another GID");
        second.set_bt_registry(Arc::clone(&registry));

        let message = format!("InfoHash {info_hash} is already registered.");
        assert_eq!(
            second.execute().await.unwrap_err(),
            crate::error::Aria2Error::DownloadFailed(message.clone())
        );
        let failed_group = second.group();
        assert_eq!(
            failed_group.get_last_error_code(),
            crate::request::request_group::DownloadResultCode::DuplicateInfoHash
        );
        assert_eq!(failed_group.get_last_error_message(), message);
        drop(failed_group);

        let registry = registry.read().expect("BT registry should be readable");
        assert!(
            registry.get(777).is_some(),
            "the first task must remain registered"
        );
        assert!(
            registry.get(778).is_none(),
            "the duplicate task must not be inserted"
        );
        assert_eq!(registry.info_hash_index.get(&info_hash), Some(&777));
    }
}

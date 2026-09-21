use std::sync::Arc;

use tracing::info;

use crate::download::download_context::{ContextAttributeType, TorrentAttribute};
use crate::engine::bt_download_command::BtDownloadCommand;
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    pub(super) fn register_bt_download(&mut self) {
        let Some(registry) = self.bt_registry.as_ref() else {
            return;
        };

        let gid = self.group.recover().gid().value();
        let download_context = self.group.recover().get_download_context();
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
        let bt_announce = Arc::new(crate::engine::bt_tracker_comm::BtAnnounce::new(
            &announce_list,
            &announce_url,
        ));
        // Keep the RPC view attached to the live TrackerAnnouncer.  The
        // compatibility BtAnnounce handle only contains torrent metadata and
        // cannot see public trackers appended during discovery or refresh.
        let tracker_runtime = Arc::new(std::sync::RwLock::new(
            crate::engine::bt_tracker_comm::TrackerRuntimeSnapshot::from_bt_announce(&bt_announce),
        ));
        self.tracker_runtime = Some(Arc::clone(&tracker_runtime));
        let bt_object = crate::engine::bt_registry::BtObject::builder()
            .bt_announce(bt_announce)
            .tracker_runtime(tracker_runtime)
            .download_context(download_context.unwrap_or_else(|| {
                Arc::new(crate::download::DownloadContext::new(0, 0, String::new()))
            }))
            .peer_rejection(self.peer_rejection.clone())
            .build();
        if let Ok(mut reg) = registry.write() {
            reg.put(gid, bt_object);
            info!(
                gid,
                "Registered BT download into BtRegistry with BtAnnounce"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::bt_download_command_tests::build_test_torrent;
    use crate::request::request_group::{DownloadOptions, GroupId};

    #[test]
    fn registration_exposes_a_live_tracker_runtime_snapshot() {
        let torrent = build_test_torrent();
        let options = DownloadOptions::default();
        let mut command = BtDownloadCommand::new(GroupId::new(777), &torrent, &options, None)
            .expect("test torrent should construct");
        let registry = Arc::new(std::sync::RwLock::new(
            crate::engine::bt_registry::BtRegistry::new(),
        ));
        command.set_bt_registry(Arc::clone(&registry));
        command.register_bt_download();

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
}

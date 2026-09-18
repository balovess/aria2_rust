use std::sync::Arc;

use tracing::info;

use crate::download::download_context::{ContextAttributeType, TorrentAttribute};
use crate::engine::bt_download_command::BtDownloadCommand;
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    pub(super) fn register_bt_download(&self) {
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
        let bt_object = crate::engine::bt_registry::BtObject::builder()
            .bt_announce(bt_announce)
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

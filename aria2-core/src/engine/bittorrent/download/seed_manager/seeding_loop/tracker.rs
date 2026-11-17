use std::sync::Arc;

use tracing::{debug, warn};

use super::super::BtSeedManager;

impl BtSeedManager {
    pub(super) fn start_tracker_announce(&mut self) {
        if self.tracker_actor.is_some() || self.pending_tracker_announce.is_some() {
            return;
        }
        let Some(announcer) = self.announcer.as_ref() else {
            return;
        };
        let ready = announcer
            .try_lock()
            .is_ok_and(|announcer| announcer.is_default_announce_ready());
        if !ready {
            return;
        }

        let announcer = Arc::clone(announcer);
        let info_hash = self.info_hash;
        let peer_id = self.peer_id;
        let downloaded = self.total_downloaded;
        let uploaded = self.total_uploaded;
        self.pending_tracker_announce = Some(tokio::spawn(async move {
            announcer
                .lock()
                .await
                .announce(&info_hash, &peer_id, downloaded, 0, uploaded)
                .await
        }));
    }

    pub(super) fn finish_tracker_announce(
        &mut self,
        result: Result<
            Option<crate::engine::bittorrent::tracker::communication::AnnounceResult>,
            tokio::task::JoinError,
        >,
    ) {
        self.pending_tracker_announce.take();
        match result {
            Ok(Some(result)) => {
                debug!(
                    "[Seed] Re-announced to {} ({:?} seeders, {:?} leechers)",
                    result.tracker_url, result.seeders, result.leechers
                );
                self.store_tracker_peers(result.peers);
            }
            Ok(None) => {}
            Err(error) => warn!(%error, "Seeding tracker announce task failed"),
        }
    }

    pub(super) async fn finish_pending_tracker_announce(&mut self) {
        let Some(task) = self.pending_tracker_announce.take() else {
            return;
        };
        if let Err(error) = task.await {
            warn!(%error, "Seeding tracker announce task failed during shutdown");
        }
    }
}

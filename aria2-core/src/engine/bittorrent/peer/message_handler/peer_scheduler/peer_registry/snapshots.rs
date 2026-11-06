use std::sync::Arc;
use std::time::Instant;

use super::PeerSwarm;

impl PeerSwarm {
    pub(crate) fn attach_peer_snapshot_store(
        &mut self,
        store: Arc<std::sync::RwLock<Vec<crate::request::request_group::BtPeerSnapshot>>>,
    ) {
        self.peer_snapshot_store = Some(store);
        self.publish_peer_snapshots();
    }

    pub(crate) fn peer_snapshots(&self) -> Vec<crate::request::request_group::BtPeerSnapshot> {
        let now = Instant::now();
        self.actors
            .iter()
            .filter(|actor| !actor.dead)
            .map(|actor| crate::request::request_group::BtPeerSnapshot {
                peer_id: actor.stats.peer_id,
                client: actor
                    .client
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
                addr: actor.advertised_endpoint.unwrap_or(actor.endpoint),
                is_incoming: actor.incoming,
                source: actor.source,
                bitfield: actor.has_bitfield.then(|| actor.bitfield.clone()),
                uploaded_bytes: actor.stats.uploaded_bytes,
                downloaded_bytes: actor.stats.downloaded_bytes,
                upload_speed: actor.stats.recent_upload_speed_at(now) as f64,
                download_speed: actor.stats.recent_download_speed_at(now) as f64,
                avg_upload_speed: actor.stats.avg_upload_speed,
                avg_download_speed: actor.stats.avg_download_speed,
                am_choking: actor.stats.am_choking,
                peer_choking: actor.stats.peer_choking,
                am_interested: actor.stats.am_interested,
                peer_interested: actor.stats.peer_interested,
                outstanding_upload_requests: actor.stats.outstanding_upload_count,
                outstanding_download_requests: actor
                    .pending_download_requests
                    .load(std::sync::atomic::Ordering::Relaxed),
                seeder: Some(actor.seeder),
                connection_duration_secs: actor.stats.connection_duration_secs(),
                last_data_age_secs: actor
                    .stats
                    .last_data_time
                    .map_or(actor.stats.age().as_secs(), |time| time.elapsed().as_secs()),
                is_snubbed: actor.stats.is_snubbed,
                is_banned: actor.stats.is_banned,
            })
            .collect()
    }

    pub(super) fn publish_peer_snapshots(&self) {
        let Some(store) = &self.peer_snapshot_store else {
            return;
        };
        *store
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self.peer_snapshots();
    }
}

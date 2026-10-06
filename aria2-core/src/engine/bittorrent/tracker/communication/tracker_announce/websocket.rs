use super::*;

impl TrackerAnnouncer {
    /// Execute a WebSocket tracker announce using the same lifecycle state
    /// machine as HTTP and UDP trackers.
    pub(super) async fn announce_websocket(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
        tracker_url: &str,
    ) -> Option<AnnounceResult> {
        let event = self.announce.announce_list().get_event();
        self.announce.announce_start();
        self.publish_runtime_snapshot();

        let response = crate::engine::bittorrent::tracker::websocket::announce_with_policy(
            tracker_url,
            crate::engine::bittorrent::tracker::websocket::AnnounceRequest {
                info_hash,
                peer_id,
                downloaded,
                left,
                uploaded,
                numwant: self.announce.numwant(),
                port: self.announce.tcp_port(),
                event,
                options: &self.websocket_options,
            },
            &self.outbound_network_policy,
        )
        .await;

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                warn!(tracker = %tracker_url, %error, "WebSocket tracker announce failed");
                self.last_failure_kind = Some(error.kind);
                self.announce.announce_failure();
                return None;
            }
        };

        self.announce.process_announce_stats(
            response.interval,
            response.min_interval,
            response.seeders,
            response.leechers,
        );
        self.update_tracker_stats(
            tracker_url,
            response.interval.unwrap_or_default(),
            response.min_interval.unwrap_or_default(),
            response.seeders,
            response.leechers,
            None,
        );
        self.announce.announce_success();

        let peers = response
            .peers
            .into_iter()
            .map(|peer| (peer.ip().to_string(), peer.port()))
            .collect();
        Some(AnnounceResult {
            peers,
            interval: self.announce.interval(),
            seeders: self.announce.complete(),
            leechers: self.announce.incomplete(),
            event,
            tracker_url: tracker_url.to_string(),
        })
    }
}

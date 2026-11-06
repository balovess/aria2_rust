use super::*;

impl TrackerAnnouncer {
    /// Execute a UDP tracker announce.
    pub(super) async fn announce_udp(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
        tracker_url: &str,
    ) -> Option<AnnounceResult> {
        let event = self.announce.announce_list().get_event();
        let udp_event = self.announce.current_udp_event();
        let tracker_addr = if let Some((_, addr)) = self
            .udp_tracker_endpoint
            .as_ref()
            .filter(|(url, _)| url == tracker_url)
        {
            *addr
        } else {
            match resolve_udp_tracker_addr(tracker_url, &self.outbound_network_policy).await {
                Ok(addr) => {
                    self.udp_tracker_endpoint = Some((tracker_url.to_owned(), addr));
                    addr
                }
                Err(error) => {
                    warn!(tracker = %tracker_url, %error, "Failed to resolve UDP tracker using the outbound network policy");
                    self.last_failure_kind = Some(TrackerFailureKind::Network);
                    self.announce.announce_failure();
                    return None;
                }
            }
        };
        let tracker_ipv6 = tracker_addr.is_ipv6();

        if self.udp_client.is_none() || self.udp_family_ipv6 != Some(tracker_ipv6) {
            match UdpTrackerClient::new_with_policy_for_family(
                0,
                &self.outbound_network_policy,
                tracker_ipv6,
            )
            .await
            {
                Ok(client) => self.udp_client = Some(client),
                Err(error) => {
                    warn!(%error, "Failed to create UDP tracker client");
                    self.last_failure_kind = Some(TrackerFailureKind::Network);
                    self.announce.announce_failure();
                    return None;
                }
            }
            self.udp_family_ipv6 = Some(tracker_ipv6);
        }

        self.announce.announce_start();
        self.publish_runtime_snapshot();

        debug!(
            "[BT] Announcing to UDP tracker {} (event={:?}, udp_event={})",
            tracker_url, event, udp_event
        );

        let response = match self
            .udp_client
            .as_mut()?
            .announce(
                UdpAnnounceParams {
                    tracker_addr,
                    info_hash,
                    peer_id,
                    downloaded: downloaded as i64,
                    left: left as i64,
                    uploaded: uploaded as i64,
                    event: udp_event,
                    num_want: self.announce.numwant() as i32,
                },
                Duration::from_secs(self.tracker_timeout_secs),
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                warn!(tracker = %tracker_url, %error, "UDP tracker announce failed");
                self.last_failure_kind = Some(match error {
                    UdpError::TrackerError => TrackerFailureKind::TrackerRejected,
                    UdpError::MalformedResponse => TrackerFailureKind::MalformedResponse,
                    UdpError::Network => TrackerFailureKind::Network,
                    UdpError::Timeout => TrackerFailureKind::Timeout,
                });
                self.announce.announce_failure();
                return None;
            }
        };

        let mut peers = self.announce.process_udp_announce_response(&response);
        self.update_tracker_stats(
            tracker_url,
            response.interval as u64,
            response.interval as u64,
            Some(response.seeders as i64),
            Some(response.leechers as i64),
            None,
        );
        self.announce.announce_success();
        peers.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        peers.dedup();

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

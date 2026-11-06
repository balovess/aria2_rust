use super::*;

impl TrackerAnnouncer {
    /// Execute an HTTP tracker announce.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn announce_http(
        &mut self,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        downloaded: u64,
        left: u64,
        uploaded: u64,
        event: AnnounceEvent,
        tracker_url: &str,
    ) -> Option<AnnounceResult> {
        // Build the announce URL through BtAnnounce state machine
        let url = self.announce.get_announce_url_without_adjustment(
            info_hash, peer_id, uploaded, downloaded, left, None,
        )?;

        // Signal announce start
        self.announce.announce_start();
        self.publish_runtime_snapshot();

        debug!(
            "[BT] Announcing to HTTP tracker {} (event={:?})",
            tracker_url, event
        );

        // Send HTTP request
        let parsed_url = reqwest::Url::parse(&url).ok();
        let tracker_port = parsed_url
            .as_ref()
            .and_then(reqwest::Url::port_or_known_default)
            .unwrap_or(80);
        let local_address = match parsed_url.as_ref().and_then(reqwest::Url::host_str) {
            Some(host) => {
                match self
                    .outbound_network_policy
                    .source_for_host(host, tracker_port)
                    .await
                {
                    Ok(address) => address,
                    Err(error) => {
                        warn!(tracker = %tracker_url, %error, "HTTP tracker has no compatible outbound source");
                        self.last_failure_kind = Some(TrackerFailureKind::Network);
                        self.announce.announce_failure();
                        return None;
                    }
                }
            }
            None if self.outbound_network_policy.is_direct() => None,
            None => {
                warn!(tracker = %tracker_url, "HTTP tracker URL has no host");
                self.last_failure_kind = Some(TrackerFailureKind::Network);
                self.announce.announce_failure();
                return None;
            }
        };

        let client = if let Some(client) = self.http_clients.get(&local_address) {
            client.clone()
        } else {
            let client = match crate::engine::bittorrent::tracker::http_client::build_tracker_client_with_source(
                self.tracker_timeout_secs,
                self.tracker_connect_timeout_secs,
                &self.http_tls,
                local_address,
            ) {
                Ok(client) => client,
                Err(error) => {
                    warn!(tracker = %tracker_url, %error, "Failed to build HTTP tracker client");
                    self.last_failure_kind = Some(TrackerFailureKind::Network);
                    self.announce.announce_failure();
                    return None;
                }
            };
            self.http_clients.insert(local_address, client.clone());
            client
        };

        match client.get(&url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    warn!(
                        "[BT] HTTP tracker {} returned status {}",
                        tracker_url,
                        resp.status()
                    );
                    self.last_failure_kind = Some(
                        if resp.status().is_server_error()
                            || matches!(resp.status().as_u16(), 408 | 425 | 429)
                        {
                            TrackerFailureKind::RemoteTemporary
                        } else {
                            TrackerFailureKind::TrackerRejected
                        },
                    );
                    self.announce.announce_failure();
                    return None;
                }

                match resp.bytes().await {
                    Ok(body) => {
                        match aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(
                            &body,
                        ) {
                            Ok(tracker_resp) => {
                                if tracker_resp.is_failure() {
                                    let reason = tracker_resp
                                        .failure_reason
                                        .unwrap_or_else(|| "tracker failure".to_string());
                                    warn!("[BT] HTTP tracker {} failure: {}", tracker_url, reason);
                                    self.last_failure_kind =
                                        Some(TrackerFailureKind::TrackerRejected);
                                    self.announce.announce_failure();
                                    return None;
                                }

                                // Process through BtAnnounce state machine
                                match self.announce.process_announce_response(&tracker_resp) {
                                    Ok(peers) => {
                                        self.update_tracker_stats(
                                            tracker_url,
                                            tracker_resp.interval as u64,
                                            tracker_resp.min_interval.map_or(0, u64::from),
                                            tracker_resp.seeders.map(i64::from),
                                            tracker_resp.leechers.map(i64::from),
                                            tracker_resp.tracker_id.as_deref(),
                                        );
                                        self.update_tracker_downloaded(
                                            tracker_url,
                                            tracker_resp.downloaded,
                                        );
                                        self.announce.announce_success();
                                        let interval = self.announce.interval();
                                        let seeders = self.announce.complete();
                                        let leechers = self.announce.incomplete();
                                        Some(AnnounceResult {
                                            peers,
                                            interval,
                                            seeders,
                                            leechers,
                                            event,
                                            tracker_url: tracker_url.to_string(),
                                        })
                                    }
                                    Err(e) => {
                                        warn!(
                                            "[BT] HTTP tracker {} response processing failed: {}",
                                            tracker_url, e
                                        );
                                        self.last_failure_kind =
                                            Some(TrackerFailureKind::MalformedResponse);
                                        self.announce.announce_failure();
                                        None
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "[BT] HTTP tracker {} response parse failed: {}",
                                    tracker_url, e
                                );
                                self.last_failure_kind =
                                    Some(TrackerFailureKind::MalformedResponse);
                                self.announce.announce_failure();
                                None
                            }
                        }
                    }
                    Err(e) => {
                        warn!("[BT] HTTP tracker {} body read failed: {}", tracker_url, e);
                        self.last_failure_kind = Some(if e.is_timeout() {
                            TrackerFailureKind::Timeout
                        } else {
                            TrackerFailureKind::Network
                        });
                        self.announce.announce_failure();
                        None
                    }
                }
            }
            Err(e) => {
                warn!("[BT] HTTP tracker {} request failed: {}", tracker_url, e);
                self.last_failure_kind = Some(if e.is_timeout() {
                    TrackerFailureKind::Timeout
                } else {
                    TrackerFailureKind::Network
                });
                self.announce.announce_failure();
                None
            }
        }
    }
}

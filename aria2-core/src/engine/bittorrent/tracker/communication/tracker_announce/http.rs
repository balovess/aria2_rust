use super::*;

const MAX_TRACKER_HTTP_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

enum TrackerBodyReadError {
    TooLarge,
    SizeOverflow,
    Transport(reqwest::Error),
}

impl std::fmt::Display for TrackerBodyReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(
                formatter,
                "HTTP tracker response exceeds {} byte limit",
                MAX_TRACKER_HTTP_RESPONSE_BYTES
            ),
            Self::SizeOverflow => formatter.write_str("HTTP tracker response size overflow"),
            Self::Transport(error) => write!(formatter, "Read body failed: {error}"),
        }
    }
}

fn ensure_tracker_response_size(size: usize) -> Result<(), TrackerBodyReadError> {
    if size > MAX_TRACKER_HTTP_RESPONSE_BYTES {
        return Err(TrackerBodyReadError::TooLarge);
    }
    Ok(())
}

async fn read_tracker_response_body(
    mut response: reqwest::Response,
) -> Result<Vec<u8>, TrackerBodyReadError> {
    if let Some(size) = response.content_length()
        && size > MAX_TRACKER_HTTP_RESPONSE_BYTES as u64
    {
        return Err(TrackerBodyReadError::TooLarge);
    }

    let capacity = response
        .content_length()
        .map(|size| size as usize)
        .unwrap_or(0);
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(TrackerBodyReadError::Transport)?
    {
        let size = body
            .len()
            .checked_add(chunk.len())
            .ok_or(TrackerBodyReadError::SizeOverflow)?;
        ensure_tracker_response_size(size)?;
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

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
            aria2_protocol::http::redirect::url_origin_for_logging(tracker_url),
            event
        );

        // Send HTTP request
        let parsed_url = reqwest::Url::parse(&url).ok();
        let origin_port = parsed_url
            .as_ref()
            .and_then(reqwest::Url::port_or_known_default)
            .unwrap_or(80);
        let proxy = crate::engine::bittorrent::tracker::http_client::configured_tracker_proxy(
            &url,
            &self.websocket_options,
        );
        let (tracker_host, tracker_port) = proxy
            .as_ref()
            .map(|(host, port, _)| (Some(host.as_str()), *port))
            .unwrap_or_else(|| {
                (
                    parsed_url.as_ref().and_then(reqwest::Url::host_str),
                    origin_port,
                )
            });
        let local_address = match tracker_host {
            Some(_host) if self.outbound_network_policy.is_direct() => None,
            Some(host) => {
                let source = if let Some(resolver) = &self.dns_resolver {
                    resolver
                        .resolve(host, tracker_port)
                        .await
                        .and_then(|addresses| {
                            self.outbound_network_policy
                                .source_for_addresses(&addresses)
                        })
                } else {
                    self.outbound_network_policy
                        .source_for_host(host, tracker_port)
                        .await
                };
                match source {
                    Ok(address) => address,
                    Err(error) => {
                        warn!(
                            tracker = %aria2_protocol::http::redirect::url_origin_for_logging(tracker_url),
                            %error,
                            "HTTP tracker has no compatible outbound source"
                        );
                        self.last_failure_kind = Some(TrackerFailureKind::Network);
                        self.announce.announce_failure();
                        return None;
                    }
                }
            }
            None if self.outbound_network_policy.is_direct() => None,
            None => {
                warn!(
                    tracker = %aria2_protocol::http::redirect::url_origin_for_logging(tracker_url),
                    "HTTP tracker URL has no host"
                );
                self.last_failure_kind = Some(TrackerFailureKind::Network);
                self.announce.announce_failure();
                return None;
            }
        };

        let client_key = (
            local_address,
            proxy
                .as_ref()
                .and_then(|_| parsed_url.as_ref().map(|url| url.scheme().to_string())),
        );
        let client = if let Some(client) = self.http_clients.get(&client_key) {
            client.clone()
        } else {
            let client = match crate::engine::bittorrent::tracker::http_client::build_tracker_client_with_source_and_resolution(
                self.tracker_timeout_secs,
                self.tracker_connect_timeout_secs,
                &self.http_tls,
                local_address,
                None,
                proxy.map(|(_, _, proxy)| proxy),
                self.dns_resolver.as_ref().map(|resolver| {
                    resolver.reqwest_resolver(Arc::clone(&self.outbound_network_policy))
                }),
            ) {
                Ok(client) => client,
                Err(error) => {
                    warn!(
                        tracker = %aria2_protocol::http::redirect::url_origin_for_logging(tracker_url),
                        %error,
                        "Failed to build HTTP tracker client"
                    );
                    self.last_failure_kind = Some(TrackerFailureKind::Network);
                    self.announce.announce_failure();
                    return None;
                }
            };
            self.http_clients.insert(client_key, client.clone());
            client
        };

        match client.get(&url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    warn!(
                        "[BT] HTTP tracker {} returned status {}",
                        aria2_protocol::http::redirect::url_origin_for_logging(tracker_url),
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

                match read_tracker_response_body(resp).await {
                    Ok(body) => {
                        match aria2_protocol::bittorrent::tracker::response::TrackerResponse::parse(
                            &body,
                        ) {
                            Ok(tracker_resp) => {
                                if tracker_resp.is_failure() {
                                    let reason = tracker_resp
                                        .failure_reason
                                        .unwrap_or_else(|| "tracker failure".to_string());
                                    warn!(
                                        "[BT] HTTP tracker {} failure: {}",
                                        aria2_protocol::http::redirect::url_origin_for_logging(
                                            tracker_url
                                        ),
                                        reason
                                    );
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
                                            aria2_protocol::http::redirect::url_origin_for_logging(
                                                tracker_url
                                            ),
                                            e
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
                                    aria2_protocol::http::redirect::url_origin_for_logging(
                                        tracker_url
                                    ),
                                    e
                                );
                                self.last_failure_kind =
                                    Some(TrackerFailureKind::MalformedResponse);
                                self.announce.announce_failure();
                                None
                            }
                        }
                    }
                    Err(e) => {
                        let error_kind = match &e {
                            TrackerBodyReadError::TooLarge => "response_too_large",
                            TrackerBodyReadError::SizeOverflow => "response_size_overflow",
                            TrackerBodyReadError::Transport(error) if error.is_timeout() => {
                                "timeout"
                            }
                            TrackerBodyReadError::Transport(_) => "network",
                        };
                        warn!(
                            "[BT] HTTP tracker {} body read failed ({})",
                            aria2_protocol::http::redirect::url_origin_for_logging(tracker_url),
                            error_kind
                        );
                        self.last_failure_kind = Some(match e {
                            TrackerBodyReadError::TooLarge | TrackerBodyReadError::SizeOverflow => {
                                TrackerFailureKind::MalformedResponse
                            }
                            TrackerBodyReadError::Transport(error) if error.is_timeout() => {
                                TrackerFailureKind::Timeout
                            }
                            TrackerBodyReadError::Transport(_) => TrackerFailureKind::Network,
                        });
                        self.announce.announce_failure();
                        None
                    }
                }
            }
            Err(e) => {
                let timed_out = e.is_timeout();
                warn!(
                    "[BT] HTTP tracker {} request failed ({})",
                    aria2_protocol::http::redirect::url_origin_for_logging(tracker_url),
                    if timed_out { "timeout" } else { "network" }
                );
                self.last_failure_kind = Some(if timed_out {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_response_size_limit_is_inclusive() {
        assert!(ensure_tracker_response_size(MAX_TRACKER_HTTP_RESPONSE_BYTES).is_ok());
        assert!(matches!(
            ensure_tracker_response_size(MAX_TRACKER_HTTP_RESPONSE_BYTES + 1),
            Err(TrackerBodyReadError::TooLarge)
        ));
    }
}

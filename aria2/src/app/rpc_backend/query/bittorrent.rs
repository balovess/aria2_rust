#[cfg(feature = "bittorrent")]
use std::sync::Arc;

#[cfg(feature = "bittorrent")]
use aria2_core::engine::bittorrent::tracker::communication::TrackerRuntimeSnapshot;
use aria2_core::util::rwlock_ext::RwLockRecover;
use aria2_rpc::{BackendError, BackendResponse, BackendResult, PeerInfo};

use super::super::{CoreRpcBackend, rpc_peer_id, rpc_peer_port};

impl CoreRpcBackend {
    pub(in crate::app::rpc_backend) fn get_peers(
        &self,
        gid: String,
    ) -> Result<BackendResult, BackendError> {
        let gid = self.parse_gid(&gid)?.to_hex_string();
        let group = self
            .group_man
            .group_by_hex(&gid)
            .ok_or_else(|| Self::execution(format!("No peer data is available for GID#{gid}")))?;
        let peers = group
            .recover()
            .status_snapshot()
            .bt
            .map(|bt| bt.peers)
            .unwrap_or_default()
            .into_iter()
            .map(|peer| PeerInfo {
                peer_id: rpc_peer_id(&peer.peer_id),
                ip: peer.addr.ip().to_string(),
                source: peer.source.as_str().to_string(),
                port: rpc_peer_port(peer.addr, peer.is_incoming),
                bitfield: Some(
                    peer.bitfield
                        .map(|bitfield| bitfield.iter().map(|byte| format!("{byte:02x}")).collect())
                        .unwrap_or_default(),
                ),
                am_choking: peer.am_choking,
                peer_choking: peer.peer_choking,
                download_speed: peer.download_speed.max(0.0) as u64,
                upload_speed: peer.upload_speed.max(0.0) as u64,
                seeder: Some(peer.seeder.unwrap_or(false).to_string()),
            })
            .collect();
        Ok(BackendResult::response(BackendResponse::Peers(peers)))
    }

    pub(in crate::app::rpc_backend) fn get_peer_stats(
        &self,
        gid: String,
    ) -> Result<BackendResult, BackendError> {
        let gid = self.parse_gid(&gid)?.to_hex_string();
        let group = self
            .group_man
            .group_by_hex(&gid)
            .ok_or_else(|| Self::execution(format!("No peer data is available for GID#{gid}")))?;
        let peers = group
            .recover()
            .status_snapshot()
            .bt
            .map(|bt| bt.peers)
            .unwrap_or_default();
        let peer_count = peers.len();
        let seeders = peers
            .iter()
            .filter(|peer| peer.seeder == Some(true))
            .count();
        let leechers = peers
            .iter()
            .filter(|peer| peer.seeder == Some(false))
            .count();
        Ok(BackendResult::response(BackendResponse::PeerStats(
            aria2_rpc::PeerStats {
                peer_count,
                seeders,
                leechers,
                unknown: peer_count - seeders - leechers,
            },
        )))
    }

    pub(in crate::app::rpc_backend) fn get_peer_details(
        &self,
        gid: String,
    ) -> Result<BackendResult, BackendError> {
        let gid = self.parse_gid(&gid)?.to_hex_string();
        let group = self
            .group_man
            .group_by_hex(&gid)
            .ok_or_else(|| Self::execution(format!("No peer data is available for GID#{gid}")))?;
        let snapshot = group.recover().status_snapshot();
        let num_pieces = snapshot.bt.as_ref().map_or(0, |bt| bt.num_pieces);
        let peers = snapshot
            .bt
            .map(|bt| bt.peers)
            .unwrap_or_default()
            .into_iter()
            .map(|peer| aria2_rpc::PeerDetails {
                peer_id: rpc_peer_id(&peer.peer_id),
                ip: peer.addr.ip().to_string(),
                source: peer.source.as_str().to_string(),
                port: rpc_peer_port(peer.addr, peer.is_incoming),
                client: peer.client,
                bitfield: peer
                    .bitfield
                    .as_ref()
                    .map(|bits| bits.iter().map(|byte| format!("{byte:02x}")).collect()),
                progress_percent: peer_progress_percent(
                    peer.seeder,
                    peer.bitfield.as_deref(),
                    num_pieces,
                ),
                seeder: peer.seeder,
                flags: Some(aria2_rpc::PeerFlags {
                    am_choking: peer.am_choking,
                    peer_choking: peer.peer_choking,
                    am_interested: peer.am_interested,
                    peer_interested: peer.peer_interested,
                    snubbed: peer.is_snubbed,
                    incoming: peer.is_incoming,
                }),
                uploaded_bytes: Some(peer.uploaded_bytes.to_string()),
                downloaded_bytes: Some(peer.downloaded_bytes.to_string()),
                download_speed: peer.download_speed.max(0.0) as u64,
                upload_speed: peer.upload_speed.max(0.0) as u64,
                avg_download_speed: peer.avg_download_speed,
                avg_upload_speed: peer.avg_upload_speed,
                outstanding_requests_to_peer: Some(peer.outstanding_download_requests),
                outstanding_requests_from_peer: Some(peer.outstanding_upload_requests),
            })
            .collect();
        Ok(BackendResult::response(BackendResponse::PeerDetails(peers)))
    }

    #[cfg(feature = "bittorrent")]
    pub(in crate::app::rpc_backend) fn get_trackers(
        &self,
        gid: String,
    ) -> Result<BackendResult, BackendError> {
        let gid = self.parse_gid(&gid)?.value();
        let registry = self
            .bt_registry
            .as_ref()
            .ok_or_else(|| Self::execution("BitTorrent registry is unavailable"))?;
        let guard = registry
            .read()
            .map_err(|_| BackendError::Internal("Failed to lock BitTorrent registry".into()))?;
        let object = guard
            .get(gid)
            .ok_or_else(|| Self::execution(format!("GID {gid:x} is not a BitTorrent task")))?;
        if let Some(runtime) = object.tracker_runtime.as_ref() {
            let snapshot = runtime.read().map_err(|_| {
                BackendError::Internal("Failed to lock tracker runtime snapshot".into())
            })?;
            return Ok(BackendResult::response(BackendResponse::Trackers(
                tracker_infos_from_runtime(&snapshot, unix_millis(std::time::SystemTime::now())),
            )));
        }
        let announce = object
            .bt_announce
            .as_ref()
            .ok_or_else(|| Self::execution("Tracker announcer is unavailable"))?;
        let current_announce = announce.as_ref();
        let current = current_announce
            .announce_list()
            .get_announce()
            .map(str::to_owned);
        let mut trackers = Vec::new();
        for tier in 0..current_announce.announce_list().tier_count() {
            let mut entry = 0;
            while let Some(uri) = current_announce
                .announce_list()
                .get_tracker_url(tier, entry)
            {
                trackers.push(aria2_rpc::TrackerInfo {
                    uri: uri.clone(),
                    tier: tier + 1,
                    current: current.as_deref() == Some(uri.as_str()),
                    last_attempt: false,
                    announce_ready: current.as_deref() == Some(uri.as_str())
                        && announce.is_announce_ready(),
                    all_failed: false,
                    in_flight: if current.as_deref() == Some(uri.as_str()) {
                        current_announce.in_flight_announces()
                    } else {
                        0
                    },
                    interval: if current.as_deref() == Some(uri.as_str()) {
                        announce.interval().as_secs()
                    } else {
                        0
                    },
                    min_interval: if current.as_deref() == Some(uri.as_str()) {
                        announce.min_interval().as_secs()
                    } else {
                        0
                    },
                    seeders: None,
                    leechers: None,
                    downloaded: None,
                    last_failure_kind: None,
                    tracker_id: if current.as_deref() == Some(uri.as_str()) {
                        current_announce.tracker_id().to_string()
                    } else {
                        String::new()
                    },
                    seconds_since_last_success: if current.as_deref() == Some(uri.as_str()) {
                        current_announce.seconds_since_last_success()
                    } else {
                        None
                    },
                    last_success_at_unix_millis: None,
                    snapshot_at_unix_millis: unix_millis(std::time::SystemTime::now()).to_string(),
                    status: if current.as_deref() == Some(uri.as_str())
                        && announce.is_announce_ready()
                    {
                        "ready".to_string()
                    } else {
                        "unknown".to_string()
                    },
                });
                entry += 1;
            }
        }
        Ok(BackendResult::response(BackendResponse::Trackers(trackers)))
    }

    #[cfg(feature = "bittorrent")]
    pub(in crate::app::rpc_backend) async fn get_dht_status(
        &self,
    ) -> Result<BackendResult, BackendError> {
        let engines = self
            .bt_registry
            .as_ref()
            .and_then(|registry| {
                registry
                    .read()
                    .ok()
                    .map(|registry| registry.get_dht_engines())
            })
            .unwrap_or_default();
        let mut stats = aria2_protocol::bittorrent::dht::engine::DhtEngineStats {
            total_nodes: 0,
            good_nodes: 0,
            pending_transactions: 0,
            questionable_nodes: 0,
            bad_nodes: 0,
            cached_nodes: 0,
            bucket_count: 0,
            peer_info_hashes: 0,
            stored_peers: 0,
            peer_storage_evictions: 0,
            max_peer_info_hashes: 0,
            persistence_enabled: false,
            persistence_max_age_secs: 0,
            cleanup_interval_secs: 0,
            save_interval_secs: 0,
            state: aria2_protocol::bittorrent::dht::engine::DhtEngineState::Stopped,
        };
        for engine in engines {
            let current = engine.stats().await;
            stats.total_nodes += current.total_nodes;
            stats.good_nodes += current.good_nodes;
            stats.pending_transactions += current.pending_transactions;
            stats.questionable_nodes += current.questionable_nodes;
            stats.bad_nodes += current.bad_nodes;
            stats.cached_nodes += current.cached_nodes;
            stats.bucket_count += current.bucket_count;
            stats.peer_info_hashes += current.peer_info_hashes;
            stats.stored_peers += current.stored_peers;
            stats.peer_storage_evictions += current.peer_storage_evictions;
            stats.max_peer_info_hashes += current.max_peer_info_hashes;
            stats.persistence_enabled |= current.persistence_enabled;
            stats.persistence_max_age_secs = stats
                .persistence_max_age_secs
                .max(current.persistence_max_age_secs);
            stats.cleanup_interval_secs = stats
                .cleanup_interval_secs
                .max(current.cleanup_interval_secs);
            stats.save_interval_secs = stats.save_interval_secs.max(current.save_interval_secs);
            if dht_state_priority(current.state) > dht_state_priority(stats.state) {
                stats.state = current.state;
            }
        }
        Ok(BackendResult::response(BackendResponse::DhtStatus(
            aria2_rpc::DhtStatus {
                state: format!("{:?}", stats.state).to_lowercase(),
                total_nodes: stats.total_nodes,
                good_nodes: stats.good_nodes,
                pending_transactions: stats.pending_transactions,
                questionable_nodes: stats.questionable_nodes,
                bad_nodes: stats.bad_nodes,
                cached_nodes: stats.cached_nodes,
                bucket_count: stats.bucket_count,
                peer_info_hashes: stats.peer_info_hashes,
                stored_peers: stats.stored_peers,
                peer_storage_evictions: stats.peer_storage_evictions,
                max_peer_info_hashes: stats.max_peer_info_hashes,
                persistence_enabled: stats.persistence_enabled,
                persistence_max_age_secs: stats.persistence_max_age_secs,
                cleanup_interval_secs: stats.cleanup_interval_secs,
                save_interval_secs: stats.save_interval_secs,
            },
        )))
    }

    #[cfg(feature = "bittorrent")]
    pub(in crate::app::rpc_backend) async fn save_dht_state(
        &self,
    ) -> Result<BackendResult, BackendError> {
        let engines = self.dht_engines();
        if engines.is_empty() {
            return Err(Self::execution("DHT engine is not running"));
        }
        for engine in engines {
            engine
                .save_state()
                .await
                .map_err(|error| Self::execution(format!("Failed to save DHT state: {error}")))?;
        }
        Ok(BackendResult::response(BackendResponse::Text("OK".into())))
    }

    #[cfg(feature = "bittorrent")]
    pub(in crate::app::rpc_backend) async fn evict_dht_nodes(
        &self,
    ) -> Result<BackendResult, BackendError> {
        let engines = self.dht_engines();
        if engines.is_empty() {
            return Err(Self::execution("DHT engine is not running"));
        }
        let mut evicted = 0;
        let mut replacements = 0;
        for engine in engines {
            let (current_evicted, current_replacements) = engine.evict_nodes().await;
            evicted += current_evicted;
            replacements += current_replacements;
        }
        Ok(BackendResult::response(BackendResponse::Counts([
            evicted,
            replacements,
        ])))
    }

    #[cfg(feature = "bittorrent")]
    fn dht_engines(&self) -> Vec<Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>> {
        self.bt_registry
            .as_ref()
            .and_then(|registry| {
                registry
                    .read()
                    .ok()
                    .map(|registry| registry.get_dht_engines())
            })
            .unwrap_or_default()
    }
}
fn peer_progress_percent(
    seeder: Option<bool>,
    bitfield: Option<&[u8]>,
    num_pieces: u32,
) -> Option<f64> {
    if seeder == Some(true) {
        return Some(100.0);
    }
    let bitfield = bitfield?;
    if num_pieces == 0 {
        return None;
    }
    let full_bytes = (num_pieces / 8) as usize;
    let mut completed = bitfield
        .iter()
        .take(full_bytes)
        .map(|byte| byte.count_ones())
        .sum::<u32>();
    let remaining_bits = num_pieces % 8;
    if remaining_bits > 0 {
        let mask = 0xff << (8 - remaining_bits);
        completed += bitfield
            .get(full_bytes)
            .map_or(0, |byte| (byte & mask).count_ones());
    }
    Some((f64::from(completed.min(num_pieces)) * 100.0) / f64::from(num_pieces))
}

#[cfg(feature = "bittorrent")]
fn unix_millis(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(feature = "bittorrent")]
fn tracker_infos_from_runtime(
    snapshot: &TrackerRuntimeSnapshot,
    now_unix_millis: u64,
) -> Vec<aria2_rpc::TrackerInfo> {
    if !snapshot.trackers.is_empty() {
        return snapshot
            .trackers
            .iter()
            .map(|tracker| aria2_rpc::TrackerInfo {
                uri: tracker.uri.clone(),
                tier: tracker.tier,
                current: tracker.current,
                last_attempt: tracker.last_attempt,
                announce_ready: tracker.announce_ready,
                all_failed: tracker.all_failed,
                in_flight: tracker.in_flight,
                interval: tracker.interval_secs,
                min_interval: tracker.min_interval_secs,
                seeders: tracker.seeders,
                leechers: tracker.leechers,
                downloaded: tracker.downloaded.map(|value| value.to_string()),
                last_failure_kind: tracker
                    .last_failure_kind
                    .map(|failure| failure.as_str().to_string()),
                tracker_id: tracker.tracker_id.clone(),
                seconds_since_last_success: tracker
                    .last_success_at_unix_millis
                    .map(|last_success| now_unix_millis.saturating_sub(last_success) / 1_000)
                    .or(tracker.seconds_since_last_success),
                last_success_at_unix_millis: tracker
                    .last_success_at_unix_millis
                    .map(|value| value.to_string()),
                snapshot_at_unix_millis: tracker.snapshot_at_unix_millis.to_string(),
                status: tracker.status.clone(),
            })
            .collect();
    }

    snapshot
        .tracker_tiers
        .iter()
        .enumerate()
        .flat_map(|(tier, uris)| {
            uris.iter().map(move |uri| aria2_rpc::TrackerInfo {
                uri: uri.clone(),
                tier: tier + 1,
                current: snapshot.current_url.as_deref() == Some(uri.as_str()),
                last_attempt: snapshot.last_attempt_url.as_deref() == Some(uri.as_str()),
                announce_ready: snapshot.announce_ready
                    && snapshot.current_url.as_deref() == Some(uri.as_str()),
                all_failed: false,
                in_flight: if snapshot.current_url.as_deref() == Some(uri.as_str()) {
                    snapshot.in_flight
                } else {
                    0
                },
                interval: if snapshot.current_url.as_deref() == Some(uri.as_str()) {
                    snapshot.interval_secs
                } else {
                    0
                },
                min_interval: if snapshot.current_url.as_deref() == Some(uri.as_str()) {
                    snapshot.min_interval_secs
                } else {
                    0
                },
                seeders: None,
                leechers: None,
                downloaded: None,
                last_failure_kind: if snapshot.last_attempt_url.as_deref() == Some(uri.as_str()) {
                    snapshot
                        .last_failure_kind
                        .map(|failure| failure.as_str().to_string())
                } else {
                    None
                },
                tracker_id: if snapshot.current_url.as_deref() == Some(uri.as_str()) {
                    snapshot.tracker_id.clone()
                } else {
                    String::new()
                },
                seconds_since_last_success: if snapshot.current_url.as_deref() == Some(uri.as_str())
                {
                    snapshot.seconds_since_last_success
                } else {
                    None
                },
                last_success_at_unix_millis: None,
                snapshot_at_unix_millis: unix_millis(std::time::SystemTime::now()).to_string(),
                status: if snapshot.in_flight > 0 {
                    "announcing".to_string()
                } else if snapshot.all_failed {
                    "failed".to_string()
                } else if snapshot.last_attempt_url.as_deref() == Some(uri.as_str()) {
                    "succeeded".to_string()
                } else {
                    "unknown".to_string()
                },
            })
        })
        .collect()
}

#[cfg(feature = "bittorrent")]
fn dht_state_priority(state: aria2_protocol::bittorrent::dht::engine::DhtEngineState) -> u8 {
    use aria2_protocol::bittorrent::dht::engine::DhtEngineState;
    match state {
        DhtEngineState::Stopped => 1,
        DhtEngineState::ShuttingDown => 2,
        DhtEngineState::Bootstrapping => 3,
        DhtEngineState::Running => 4,
    }
}

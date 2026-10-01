use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use futures::future::join_all;

use crate::engine::bittorrent::download::command::MAX_PUBLIC_TRACKERS_TO_TRY;
use crate::engine::bittorrent::tracker::communication::{
    AnnounceResult, SharedTrackerRuntime, TrackerAnnouncer, TrackerRuntimeInfo,
    TrackerRuntimeSnapshot,
};

pub(super) const MAX_PUBLIC_TRACKERS_IN_FANOUT: usize = 3;

pub(super) async fn add_public_tracker_announcers(
    primary: &TrackerAnnouncer,
    public_announcers: &mut Vec<TrackerAnnouncer>,
    active_urls: &mut HashSet<String>,
    max_active: usize,
) -> usize {
    let remaining = max_active.saturating_sub(public_announcers.len());
    if remaining == 0 {
        return 0;
    }
    let candidates = primary
        .available_public_tracker_urls(active_urls, remaining)
        .await;
    let added = candidates.len();
    for url in candidates {
        active_urls.insert(url.clone());
        public_announcers.push(primary.fork_public_tracker(&url));
    }
    added
}

pub(super) async fn announce_initial_primary(
    announcer: &mut TrackerAnnouncer,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    left: u64,
) -> Vec<AnnounceResult> {
    let mut results = Vec::new();
    for _ in 0..MAX_PUBLIC_TRACKERS_TO_TRY {
        if !announcer.is_announce_ready() {
            break;
        }
        let Some(result) = announcer.announce(info_hash, peer_id, 0, left, 0).await else {
            continue;
        };
        tracing::info!(
            peers = result.peers.len(),
            tracker = %result.tracker_url,
            event = ?result.event,
            interval_secs = result.interval.as_secs(),
            "Initial BitTorrent tracker announce completed"
        );
        let has_peers = !result.peers.is_empty();
        results.push(result);
        if has_peers || !announcer.is_announce_ready() {
            break;
        }
    }
    results
}

pub(super) async fn announce_many(
    announcers: &mut [TrackerAnnouncer],
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    downloaded: u64,
    left: u64,
    uploaded: u64,
) -> Vec<AnnounceResult> {
    join_all(announcers.iter_mut().map(|announcer| async move {
        announcer
            .announce(info_hash, peer_id, downloaded, left, uploaded)
            .await
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

pub(super) async fn announce_if_ready(
    announcer: &mut TrackerAnnouncer,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    downloaded: u64,
    left: u64,
    uploaded: u64,
) -> Vec<AnnounceResult> {
    if !announcer.is_default_announce_ready() {
        return Vec::new();
    }
    announcer
        .announce(info_hash, peer_id, downloaded, left, uploaded)
        .await
        .into_iter()
        .collect()
}

pub(super) async fn announce_due_public(
    announcers: &mut [TrackerAnnouncer],
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    downloaded: u64,
    left: u64,
    uploaded: u64,
) -> Vec<AnnounceResult> {
    join_all(announcers.iter_mut().map(|announcer| async move {
        if !announcer.is_default_announce_ready() {
            return None;
        }
        announcer
            .announce(info_hash, peer_id, downloaded, left, uploaded)
            .await
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

pub(super) async fn announce_completed_many(
    announcers: &mut [TrackerAnnouncer],
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    downloaded: u64,
    uploaded: u64,
) {
    join_all(announcers.iter_mut().map(|announcer| async move {
        announcer
            .announce_completed(info_hash, peer_id, downloaded, uploaded)
            .await;
    }))
    .await;
}

pub(super) async fn announce_stopped_many(
    announcers: &mut [TrackerAnnouncer],
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    downloaded: u64,
    left: u64,
    uploaded: u64,
) {
    join_all(announcers.iter_mut().map(|announcer| async move {
        announcer
            .announce_stopped(info_hash, peer_id, downloaded, left, uploaded)
            .await;
    }))
    .await;
}

pub(super) fn next_tracker_announce_delay(
    primary: &TrackerAnnouncer,
    public_announcers: &[TrackerAnnouncer],
) -> Option<Duration> {
    std::iter::once(primary)
        .chain(public_announcers)
        .filter_map(TrackerAnnouncer::next_default_announce_delay)
        .min()
}

pub(super) fn results_to_peer_addrs(
    results: impl IntoIterator<Item = AnnounceResult>,
) -> Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> {
    let mut peers = Vec::new();
    for result in results {
        for (ip, port) in result.peers {
            let peer = aria2_protocol::bittorrent::peer::connection::PeerAddr::new(&ip, port);
            if !peers.iter().any(
                |existing: &aria2_protocol::bittorrent::peer::connection::PeerAddr| {
                    existing.ip == peer.ip && existing.port == peer.port
                },
            ) {
                peers.push(peer);
            }
        }
    }
    peers
}

pub(super) fn merge_pending_peers(
    existing: Option<Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>>,
    added: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
) -> Option<Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>> {
    let mut merged = existing.unwrap_or_default();
    for peer in added {
        if !merged
            .iter()
            .any(|known| known.ip == peer.ip && known.port == peer.port)
        {
            merged.push(peer);
        }
    }
    (!merged.is_empty()).then_some(merged)
}

pub(super) fn publish_tracker_runtime(
    shared: Option<&SharedTrackerRuntime>,
    primary: &TrackerAnnouncer,
    public_announcers: &[TrackerAnnouncer],
    public_urls: &[String],
) {
    let Some(shared) = shared else {
        return;
    };
    let snapshot = aggregate_tracker_runtime(
        primary.runtime_snapshot(),
        public_announcers
            .iter()
            .map(TrackerAnnouncer::runtime_snapshot)
            .collect(),
        public_urls,
    );
    if let Ok(mut current) = shared.write() {
        *current = snapshot;
    }
}

fn aggregate_tracker_runtime(
    primary: TrackerRuntimeSnapshot,
    public: Vec<TrackerRuntimeSnapshot>,
    public_urls: &[String],
) -> TrackerRuntimeSnapshot {
    let mut combined = primary;
    let mut known = combined
        .tracker_tiers
        .iter()
        .flatten()
        .cloned()
        .chain(combined.trackers.iter().map(|tracker| tracker.uri.clone()))
        .collect::<HashSet<_>>();
    let mut next_tier = combined.tracker_tiers.len();

    for url in public_urls {
        if known.insert(url.clone()) {
            next_tier += 1;
            combined.tracker_tiers.push(vec![url.clone()]);
            combined.trackers.push(idle_tracker_info(url, next_tier));
        }
    }

    let mut snapshots = Vec::with_capacity(public.len());
    for snapshot in public {
        combined.announce_ready |= snapshot.announce_ready;
        combined.in_flight = combined.in_flight.saturating_add(snapshot.in_flight);
        if combined.current_url.is_none() {
            combined.current_url.clone_from(&snapshot.current_url);
        }
        if combined.last_attempt_url.is_none() {
            combined
                .last_attempt_url
                .clone_from(&snapshot.last_attempt_url);
        }
        if combined.interval_secs == 0 {
            combined.interval_secs = snapshot.interval_secs;
        }
        if combined.min_interval_secs == 0 {
            combined.min_interval_secs = snapshot.min_interval_secs;
        }
        if combined.seeders.is_none() {
            combined.seeders = snapshot.seeders;
        }
        if combined.leechers.is_none() {
            combined.leechers = snapshot.leechers;
        }
        if combined.tracker_id.is_empty() {
            combined.tracker_id.clone_from(&snapshot.tracker_id);
        }
        if combined.seconds_since_last_success.is_none() {
            combined.seconds_since_last_success = snapshot.seconds_since_last_success;
        }
        if combined.last_failure_kind.is_none() {
            combined.last_failure_kind = snapshot.last_failure_kind;
        }
        snapshots.push(snapshot);
    }

    let mut dynamic_tier = combined.tracker_tiers.len();
    for snapshot in snapshots {
        for tier in &snapshot.tracker_tiers {
            let added = tier
                .iter()
                .filter(|uri| known.insert((*uri).clone()))
                .cloned()
                .collect::<Vec<_>>();
            if !added.is_empty() {
                dynamic_tier += 1;
                combined.tracker_tiers.push(added.clone());
                for url in added {
                    if !combined.trackers.iter().any(|tracker| tracker.uri == url) {
                        combined
                            .trackers
                            .push(idle_tracker_info(&url, dynamic_tier));
                    }
                }
            }
        }

        for tracker in snapshot.trackers {
            if let Some(existing) = combined
                .trackers
                .iter_mut()
                .find(|candidate| candidate.uri == tracker.uri)
            {
                let tier = existing.tier;
                *existing = tracker;
                existing.tier = tier;
                continue;
            }
            dynamic_tier += 1;
            combined.tracker_tiers.push(vec![tracker.uri.clone()]);
            let mut tracker = tracker;
            tracker.tier = dynamic_tier;
            combined.trackers.push(tracker);
        }
    }

    combined.all_failed =
        !combined.trackers.is_empty() && combined.trackers.iter().all(|tracker| tracker.all_failed);
    combined
}

fn idle_tracker_info(uri: &str, tier: usize) -> TrackerRuntimeInfo {
    TrackerRuntimeInfo {
        uri: uri.to_owned(),
        tier,
        status: "idle".to_owned(),
        snapshot_at_unix_millis: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
        ..TrackerRuntimeInfo::default()
    }
}

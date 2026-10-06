use std::sync::Arc;
use std::time::{Instant, SystemTime};

use aria2_protocol::bittorrent::tracker::public_list::TrackerFailureKind;

use super::super::super::bt_announce::BtAnnounce;
use super::TrackerAnnouncer;

/// Shared tracker state exposed to the application/RPC layer.
///
/// The registry keeps this snapshot separate from the immutable compatibility
/// `BtAnnounce` handle. The download command owns the live `TrackerAnnouncer`,
/// so it publishes here whenever that state changes.
#[derive(Debug, Clone, Default)]
pub struct TrackerRuntimeSnapshot {
    /// Tracker URLs grouped by announce tier in their current failover order.
    pub tracker_tiers: Vec<Vec<String>>,
    /// Tracker selected for the next announce attempt.
    pub current_url: Option<String>,
    /// Tracker URL used by the most recent announce attempt.
    pub last_attempt_url: Option<String>,
    pub announce_ready: bool,
    pub all_failed: bool,
    /// Failure category from the most recent announce attempt, if it failed.
    pub last_failure_kind: Option<TrackerFailureKind>,
    pub in_flight: u32,
    pub interval_secs: u64,
    pub min_interval_secs: u64,
    pub seeders: Option<i64>,
    pub leechers: Option<i64>,
    pub tracker_id: String,
    pub seconds_since_last_success: Option<u64>,
    /// Live state for each tracker URL returned by `aria2.getTrackers`.
    pub trackers: Vec<TrackerRuntimeInfo>,
}

/// Live state for one tracker URL returned by `aria2.getTrackers`.
#[derive(Debug, Clone, Default)]
pub struct TrackerRuntimeInfo {
    pub uri: String,
    pub tier: usize,
    pub current: bool,
    pub last_attempt: bool,
    pub announce_ready: bool,
    pub all_failed: bool,
    pub in_flight: u32,
    pub interval_secs: u64,
    pub min_interval_secs: u64,
    pub seeders: Option<i64>,
    pub leechers: Option<i64>,
    pub downloaded: Option<u64>,
    /// Category of this URL's latest announce failure, cleared on success.
    pub last_failure_kind: Option<TrackerFailureKind>,
    pub tracker_id: String,
    pub seconds_since_last_success: Option<u64>,
    pub last_success_at_unix_millis: Option<u64>,
    pub snapshot_at_unix_millis: u64,
    pub status: String,
}

#[derive(Debug, Clone, Default)]
pub(super) struct TrackerState {
    pub(super) in_flight: bool,
    pub(super) interval_secs: u64,
    pub(super) min_interval_secs: u64,
    pub(super) seeders: Option<i64>,
    pub(super) leechers: Option<i64>,
    pub(super) downloaded: Option<u64>,
    pub(super) tracker_id: String,
    pub(super) last_success_at: Option<Instant>,
    pub(super) last_success_wall_time: Option<SystemTime>,
    pub(super) last_failure_kind: Option<TrackerFailureKind>,
}

/// Registry-safe handle for the live tracker snapshot.
pub type SharedTrackerRuntime = Arc<std::sync::RwLock<TrackerRuntimeSnapshot>>;

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

impl TrackerRuntimeSnapshot {
    /// Build an initial snapshot from the compatibility announce state.
    pub fn from_bt_announce(announce: &BtAnnounce) -> Self {
        let announce_list = announce.announce_list();
        let tracker_tiers: Vec<Vec<String>> = (0..announce_list.tier_count())
            .map(|tier| {
                let mut urls = Vec::new();
                let mut entry = 0;
                while let Some(url) = announce_list.get_tracker_url(tier, entry) {
                    urls.push(url.clone());
                    entry += 1;
                }
                urls
            })
            .collect();

        let trackers = tracker_tiers
            .iter()
            .enumerate()
            .flat_map(|(tier, uris)| {
                uris.iter().map(move |uri| TrackerRuntimeInfo {
                    uri: uri.clone(),
                    tier: tier + 1,
                    current: announce
                        .announce_list()
                        .get_announce()
                        .is_some_and(|current| current == uri),
                    ..TrackerRuntimeInfo::default()
                })
            })
            .collect();

        Self {
            tracker_tiers,
            current_url: announce.announce_list().get_announce().map(str::to_owned),
            last_attempt_url: None,
            last_failure_kind: None,
            announce_ready: announce.is_announce_ready(),
            all_failed: announce.is_all_announce_failed(),
            in_flight: announce.in_flight_announces(),
            interval_secs: announce.interval().as_secs(),
            min_interval_secs: announce.min_interval().as_secs(),
            seeders: announce.complete(),
            leechers: announce.incomplete(),
            tracker_id: announce.tracker_id().to_string(),
            seconds_since_last_success: announce.seconds_since_last_success(),
            trackers,
        }
    }
}

impl TrackerAnnouncer {
    /// Attach the registry-visible mirror of this announcer's live state.
    pub fn set_runtime_snapshot(&mut self, state: SharedTrackerRuntime) {
        self.runtime_state = Some(state);
        self.publish_runtime_snapshot();
    }

    /// Return the actor-owned snapshot destination for live torrent-wide
    /// tracker aggregation.
    pub(crate) fn shared_runtime_snapshot(&self) -> Option<SharedTrackerRuntime> {
        self.runtime_state.clone()
    }

    /// Return the complete live tracker state for RPC or diagnostics.
    pub fn runtime_snapshot(&self) -> TrackerRuntimeSnapshot {
        let mut snapshot = TrackerRuntimeSnapshot::from_bt_announce(&self.announce);
        snapshot.last_attempt_url = self.last_attempt_tracker_url.clone();
        snapshot.last_failure_kind = self.last_failure_kind;
        snapshot.trackers = self.tracker_runtime_infos();
        snapshot
    }

    fn tracker_runtime_infos(&self) -> Vec<TrackerRuntimeInfo> {
        let current = self.announce.announce_list().get_announce();
        let last_attempt = self.last_attempt_tracker_url.as_deref();
        let ready = self.announce.is_announce_ready();
        let mut trackers = Vec::new();
        let snapshot_at_unix_millis = unix_millis(SystemTime::now());

        for tier in 0..self.announce.announce_list().tier_count() {
            let mut entry = 0;
            while let Some(uri) = self.announce.announce_list().get_tracker_url(tier, entry) {
                let state = self.tracker_states.get(uri);
                trackers.push(TrackerRuntimeInfo {
                    uri: uri.clone(),
                    tier: tier + 1,
                    current: current == Some(uri.as_str()),
                    last_attempt: last_attempt == Some(uri.as_str()),
                    announce_ready: ready && current == Some(uri.as_str()),
                    all_failed: state.is_some_and(|state| state.last_failure_kind.is_some()),
                    in_flight: state.map_or(0, |state| u32::from(state.in_flight)),
                    interval_secs: state.map_or(0, |state| state.interval_secs),
                    min_interval_secs: state.map_or(0, |state| state.min_interval_secs),
                    seeders: state.and_then(|state| state.seeders),
                    leechers: state.and_then(|state| state.leechers),
                    downloaded: state.and_then(|state| state.downloaded),
                    last_failure_kind: state.and_then(|state| state.last_failure_kind),
                    tracker_id: state.map_or_else(String::new, |state| state.tracker_id.clone()),
                    seconds_since_last_success: state.and_then(|state| {
                        state.last_success_at.map(|time| time.elapsed().as_secs())
                    }),
                    last_success_at_unix_millis: state
                        .and_then(|state| state.last_success_wall_time.map(unix_millis)),
                    snapshot_at_unix_millis,
                    status: state.map_or_else(
                        || {
                            if ready && current == Some(uri.as_str()) {
                                "ready"
                            } else {
                                "unknown"
                            }
                            .to_string()
                        },
                        |state| {
                            if state.in_flight {
                                "announcing"
                            } else if state.last_failure_kind.is_some() {
                                "failed"
                            } else if state.last_success_at.is_some() {
                                "succeeded"
                            } else if ready && current == Some(uri.as_str()) {
                                "ready"
                            } else {
                                "idle"
                            }
                            .to_string()
                        },
                    ),
                });
                entry += 1;
            }
        }
        trackers
    }

    pub(super) fn tracker_attempt_started(&mut self, tracker_url: &str) {
        self.tracker_states
            .entry(tracker_url.to_string())
            .or_default()
            .in_flight = true;
    }

    pub(super) fn tracker_attempt_finished(&mut self, tracker_url: &str, succeeded: bool) {
        let state = self
            .tracker_states
            .entry(tracker_url.to_string())
            .or_default();
        state.in_flight = false;
        if succeeded {
            state.last_failure_kind = None;
            state.last_success_at = Some(Instant::now());
            state.last_success_wall_time = Some(SystemTime::now());
        } else {
            state.last_failure_kind = self.last_failure_kind;
        }
    }

    pub(super) fn update_tracker_downloaded(&mut self, tracker_url: &str, downloaded: Option<u64>) {
        self.tracker_states
            .entry(tracker_url.to_string())
            .or_default()
            .downloaded = downloaded;
    }

    pub(super) fn update_tracker_stats(
        &mut self,
        tracker_url: &str,
        interval_secs: u64,
        min_interval_secs: u64,
        seeders: Option<i64>,
        leechers: Option<i64>,
        tracker_id: Option<&str>,
    ) {
        let state = self
            .tracker_states
            .entry(tracker_url.to_string())
            .or_default();
        state.interval_secs = interval_secs;
        state.min_interval_secs = min_interval_secs;
        state.seeders = seeders;
        state.leechers = leechers;
        if let Some(tracker_id) = tracker_id {
            state.tracker_id = tracker_id.to_string();
        }
    }

    /// Publish the current live state without holding a lock across callers.
    pub fn publish_runtime_snapshot(&self) {
        let Some(state) = self.runtime_state.as_ref() else {
            return;
        };
        let snapshot = self.runtime_snapshot();
        if let Ok(mut current) = state.write() {
            merge_tracker_runtime_snapshot(&mut current, snapshot);
        }
    }
}

fn merge_tracker_runtime_snapshot(
    current: &mut TrackerRuntimeSnapshot,
    mut update: TrackerRuntimeSnapshot,
) {
    if current.trackers.is_empty() && current.tracker_tiers.is_empty() {
        *current = update;
        return;
    }

    let mut known = current
        .tracker_tiers
        .iter()
        .flatten()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    for tier in &update.tracker_tiers {
        let added = tier
            .iter()
            .filter(|uri| known.insert((*uri).clone()))
            .cloned()
            .collect::<Vec<_>>();
        if !added.is_empty() {
            current.tracker_tiers.push(added);
        }
    }

    for tracker in &mut update.trackers {
        if let Some(existing) = current
            .trackers
            .iter_mut()
            .find(|existing| existing.uri == tracker.uri)
        {
            tracker.tier = existing.tier;
            *existing = tracker.clone();
            continue;
        }

        let tier = current
            .tracker_tiers
            .iter()
            .position(|tier| tier.iter().any(|uri| uri == &tracker.uri))
            .map_or_else(
                || {
                    current.tracker_tiers.push(vec![tracker.uri.clone()]);
                    current.tracker_tiers.len()
                },
                |index| index + 1,
            );
        tracker.tier = tier;
        current.trackers.push(tracker.clone());
    }

    let update_has_last_attempt = update.last_attempt_url.is_some();
    current.current_url = update.current_url.or_else(|| current.current_url.take());
    current.last_attempt_url = update
        .last_attempt_url
        .or_else(|| current.last_attempt_url.take());
    current.announce_ready = current
        .trackers
        .iter()
        .any(|tracker| tracker.announce_ready);
    current.all_failed =
        !current.trackers.is_empty() && current.trackers.iter().all(|tracker| tracker.all_failed);
    current.in_flight = current.trackers.iter().fold(0u32, |total, tracker| {
        total.saturating_add(tracker.in_flight)
    });
    if update.interval_secs > 0 {
        current.interval_secs = update.interval_secs;
    }
    if update.min_interval_secs > 0 {
        current.min_interval_secs = update.min_interval_secs;
    }
    current.seeders = update.seeders.or(current.seeders);
    current.leechers = update.leechers.or(current.leechers);
    if !update.tracker_id.is_empty() {
        current.tracker_id = update.tracker_id;
    }
    current.seconds_since_last_success = update
        .seconds_since_last_success
        .or(current.seconds_since_last_success);
    if update_has_last_attempt {
        current.last_failure_kind = update.last_failure_kind;
    }
}

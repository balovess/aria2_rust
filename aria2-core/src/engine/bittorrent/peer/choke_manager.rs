//! Seeder-state choke/unchoke decisions for BitTorrent peers.

use std::time::{Duration, Instant};

use rand::Rng;

use crate::engine::bittorrent::peer::choking_algorithm::PeerIdentity;
use crate::engine::bittorrent::peer::stats::PeerStats;

const RECENT_UNCHOKE_WINDOW: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
struct SeederPeerEntry {
    identity: crate::engine::bittorrent::peer::choking_algorithm::PeerIdentity,
    index: usize,
    outstanding_upload: bool,
    last_unchoke_at: Instant,
    recently_unchoked: bool,
    upload_speed: i64,
}

impl SeederPeerEntry {
    fn from_peer(index: usize, peer: &PeerStats, upload_speed: u64, now: Instant) -> Self {
        let last_unchoke_at = peer.last_unchoke_at;
        Self {
            identity: peer.into(),
            index,
            outstanding_upload: peer.outstanding_upload_count > 0,
            last_unchoke_at,
            recently_unchoked: now.duration_since(last_unchoke_at) < RECENT_UNCHOKE_WINDOW,
            upload_speed: upload_speed.min(i64::MAX as u64) as i64,
        }
    }
}

impl Ord for SeederPeerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self.outstanding_upload, other.outstanding_upload) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }
        match (self.recently_unchoked, other.recently_unchoked) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            (true, true) => return other.last_unchoke_at.cmp(&self.last_unchoke_at),
            (false, false) => {}
        }
        other
            .upload_speed
            .cmp(&self.upload_speed)
            .then_with(|| self.identity.cmp(&other.identity))
    }
}

impl PartialOrd for SeederPeerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SeederPeerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl Eq for SeederPeerEntry {}

/// Seeder-state choke policy. The swarm owner schedules rounds and applies
/// resulting state changes to peer actors.
#[derive(Debug, Clone)]
pub struct BtSeederStateChoke {
    round: u32,
    base_unchoke_slots: usize,
    optimistic_unchoke_interval: Duration,
    current_optimistic_peer: Option<PeerIdentity>,
    last_optimistic_unchoke_at: Option<Instant>,
}

impl BtSeederStateChoke {
    pub fn new() -> Self {
        Self::with_slots(4)
    }

    pub fn with_slots(slots: usize) -> Self {
        Self::with_slots_and_optimistic_unchoke_interval(slots, 0)
    }

    pub fn with_slots_and_optimistic_unchoke_interval(
        slots: usize,
        optimistic_unchoke_interval_secs: u64,
    ) -> Self {
        Self {
            round: 0,
            base_unchoke_slots: slots,
            optimistic_unchoke_interval: Duration::from_secs(optimistic_unchoke_interval_secs),
            current_optimistic_peer: None,
            last_optimistic_unchoke_at: None,
        }
    }

    /// Execute a choke round using the system clock.
    pub fn execute_choke(&mut self, peers: &mut [&mut PeerStats]) {
        self.execute_choke_at(peers, Instant::now());
    }

    /// Deterministic clock seam for ranking regressions.
    pub(crate) fn execute_choke_at(&mut self, peers: &mut [&mut PeerStats], now: Instant) {
        tracing::debug!("Seeder state, {} choke round started", self.round);

        let mut entries = Vec::new();
        for (index, peer) in peers.iter_mut().enumerate() {
            if peer.is_banned {
                continue;
            }
            peer.am_choking = true;
            peer.opt_unchoking = false;
            if peer.peer_interested {
                entries.push(SeederPeerEntry::from_peer(
                    index,
                    peer,
                    peer.recent_upload_speed_at(now),
                    now,
                ));
            } else {
                peer.opt_unchoking = false;
            }
        }

        self.unchoke_peers(&mut entries, peers, now);
        self.round = (self.round + 1) % 3;
    }

    fn unchoke_peers(
        &mut self,
        entries: &mut [SeederPeerEntry],
        peers: &mut [&mut PeerStats],
        now: Instant,
    ) {
        if self.base_unchoke_slots == 0 {
            self.current_optimistic_peer = None;
            self.last_optimistic_unchoke_at = None;
            return;
        }

        let regular_slots = if self.round == 2 {
            self.base_unchoke_slots
        } else {
            self.base_unchoke_slots.saturating_sub(1)
        };
        entries.sort();

        let split_point = entries.len().min(regular_slots);
        for entry in entries.iter().take(split_point) {
            let peer = &mut peers[entry.index];
            peer.am_choking = false;
            peer.record_unchoke();
            tracing::debug!(
                "RU (seeder): peer idx={}, ulspd={}",
                entry.index,
                entry.upload_speed
            );
        }

        if self.round < 2 {
            let optimistic_candidates = &entries[split_point..];
            let current_is_optimistic_candidate =
                self.current_optimistic_peer.is_some_and(|current| {
                    optimistic_candidates
                        .iter()
                        .any(|entry| entry.identity == current)
                });
            let rotation_due = self.last_optimistic_unchoke_at.is_none_or(|last| {
                now.saturating_duration_since(last) >= self.optimistic_unchoke_interval
            });

            if !rotation_due && current_is_optimistic_candidate {
                let current = self
                    .current_optimistic_peer
                    .expect("eligible peer identity");
                let entry = optimistic_candidates
                    .iter()
                    .find(|entry| entry.identity == current)
                    .expect("current optimistic peer is a candidate");
                let peer = &mut peers[entry.index];
                peer.opt_unchoking = true;
                peer.am_choking = false;
            } else if !optimistic_candidates.is_empty() {
                let current = self.current_optimistic_peer;
                let alternative_count = optimistic_candidates
                    .iter()
                    .filter(|entry| Some(entry.identity) != current)
                    .count();
                let candidate_offset = if alternative_count > 0 {
                    let mut rng = rand::thread_rng();
                    rng.gen_range(0..alternative_count)
                } else {
                    let mut rng = rand::thread_rng();
                    rng.gen_range(0..optimistic_candidates.len())
                };
                let picked = if alternative_count > 0 {
                    optimistic_candidates
                        .iter()
                        .filter(|entry| Some(entry.identity) != current)
                        .nth(candidate_offset)
                        .expect("selected optimistic candidate")
                } else {
                    &optimistic_candidates[candidate_offset]
                };
                let picked_index = picked.index;
                self.current_optimistic_peer = Some(picked.identity);
                self.last_optimistic_unchoke_at = Some(now);
                let peer = &mut peers[picked_index];
                peer.opt_unchoking = true;
                peer.record_optimistic_unchoke_at(now);
                tracing::debug!("POU (seeder): peer idx={}", picked_index);
            } else {
                self.current_optimistic_peer = None;
            }
        }
    }

    pub fn round(&self) -> u32 {
        self.round
    }

    #[cfg(test)]
    pub fn set_round(&mut self, round: u32) {
        self.round = round;
    }
}

impl Default for BtSeederStateChoke {
    fn default() -> Self {
        Self::new()
    }
}

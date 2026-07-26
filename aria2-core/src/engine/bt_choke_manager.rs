//! BitTorrent Choke Manager — peer choking/unchoking algorithm state
//!
//! This module implements the choke/unchoke decision logic for BitTorrent
//! peer connections, including leecher-state and seeder-state choking
//! algorithms, snubbed-peer detection, and best-peer selection.
//!
//! # Algorithms
//!
//! ## Seeder-State Choking (`BtSeederStateChoke`)
//!
//! When we are a seeder (download complete), we rank peers by:
//! 1. Outstanding upload (currently uploading to us) — highest priority
//! 2. Recently unchoked (within 20 s window) — second priority
//! 3. Upload speed — fallback
//!
//! A 3-round cycle controls optimistic unchoke: rounds 0-1 pick one
//! random peer beyond the regular unchoke slots; round 2 does not.
//!
//! ## Leecher-State Choking (`BtLeecherStateChoke`)
//!
//! When we are still downloading, we unchoke peers that are sending us
//! data (regular unchokers: peerInterested AND received data within 30 s),
//! sorted by download speed. Round 0 triggers a planned optimistic unchoke.
//!
//! # C++ Equivalence
//!
//! | Rust | C++ |
//! |---|---|
//! | `BtLeecherStateChoke` | `BtLeecherStateChoke` |
//! | `BtSeederStateChoke` | `BtSeederStateChoke` |
//! | `add_peer_to_tracking()` | `PeerChokeCommand` peer addition |
//! | `check_snubbed_peers()` | `PeerChokeCommand` snub check |
//! | `on_peer_choke/unchoke()` | Choke/unchoke event handlers |
//! | `select_best_peer_for_request()` | Best peer selection in request loop |

use std::time::{Duration, Instant};

use rand::Rng;

use crate::engine::peer_stats::PeerStats;

// Re-export hooks for backward compatibility (importers use bt_choke_manager::*)
pub use crate::engine::bt_choke_hooks::{
    add_peer_to_tracking, check_snubbed_peers, handle_snubbed_peer, on_data_received_from_peer,
    on_peer_choke, on_peer_unchoke, on_piece_received, select_best_peer_for_request,
};

// ---------------------------------------------------------------------------
// Constants matching C++ aria2
// ---------------------------------------------------------------------------

/// Time frame for the "recently unchoked" classification in the seeder-state
/// algorithm. Peers unchoked within this window get second-highest ranking
/// priority. Mirrors C++ `TIME_FRAME = 20_s`.
const SEEDER_RECENT_UNCHOKE_TIME_FRAME: Duration = Duration::from_secs(20);

/// Window used to determine whether a peer is a "regular unchoker" in the
/// leecher-state algorithm. Peers that sent us data within this window are
/// eligible for regular unchoke. Mirrors C++ `30_s` in PeerEntry ctor.
const LEECHER_REGULAR_UNCHOKE_WINDOW: Duration = Duration::from_secs(30);

/// Number of regular unchoke slots in the leecher-state algorithm.
/// Mirrors C++ `int count = 3;` in `BtLeecherStateChoke::regularUnchoke()`.
const LEECHER_REGULAR_UNCHOKE_SLOTS: usize = 3;

// ===========================================================================
// Seeder-state peer entry (snapshot for ranking)
// ===========================================================================

/// Snapshot of a peer's state used for seeder-state ranking.
///
/// Captured at the start of each choke round so that ranking is based on a
/// consistent view. Mirrors C++ `BtSeederStateChoke::PeerEntry`.
#[derive(Debug, Clone)]
struct SeederPeerEntry {
    /// Index back into the caller's peer list
    index: usize,
    /// Whether this peer has outstanding (in-flight) upload requests
    outstanding_upload: bool,
    /// When we last unchoked this peer
    last_am_unchoking: Instant,
    /// Whether the last unchoke was within `SEEDER_RECENT_UNCHOKE_TIME_FRAME`
    recent_unchoking: bool,
    /// Peer's upload speed (bytes/sec), used as fallback ranking criterion
    upload_speed: i64,
}

impl SeederPeerEntry {
    fn from_peer(index: usize, peer: &PeerStats) -> Self {
        let now = Instant::now();
        let last_am_unchoking = peer.last_unchoke_at;
        let recent_unchoking =
            now.duration_since(last_am_unchoking) < SEEDER_RECENT_UNCHOKE_TIME_FRAME;
        Self {
            index,
            outstanding_upload: peer.outstanding_upload_count > 0,
            last_am_unchoking,
            recent_unchoking,
            upload_speed: peer.upload_speed as i64,
        }
    }
}

impl Ord for SeederPeerEntry {
    /// Comparison for sorting: lower ordinal = higher priority.
    ///
    /// Mirrors C++ `BtSeederStateChoke::PeerEntry::operator<`:
    /// 1. Outstanding upload peers rank first
    /// 2. Recently unchoked peers rank by recency (more recent first)
    /// 3. Fallback: higher upload speed ranks first
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Priority 1: outstanding upload
        match (self.outstanding_upload, other.outstanding_upload) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }
        // Priority 2: recently unchoked (more recent = higher priority)
        //
        // C++ logic:
        //   if (this->recentUnchoking_ && this->lastAmUnchoking_ > rhs.lastAmUnchoking_)
        //     return true;  // this < rhs, this ranks first
        //   else if (rhs.recentUnchoking_)
        //     return false; // rhs ranks first
        //   else
        //     compare by upload speed
        //
        // When this=recent, rhs=not-recent: this.lastAmUnchoking_ is always
        // more recent than rhs.lastAmUnchoking_ (by the TIME_FRAME invariant),
        // so the first condition is true → this ranks first.
        //
        // When both=recent: more recent timestamp ranks first.
        match (self.recent_unchoking, other.recent_unchoking) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            (true, true) => {
                // Both recently unchoked: more recent wins
                // C++ `this->lastAmUnchoking_ > rhs.lastAmUnchoking_`
                // means "this is more recent → this < rhs (ranks first)"
                return other.last_am_unchoking.cmp(&self.last_am_unchoking);
            }
            _ => {}
        }
        // Priority 3: higher upload speed = higher priority = "less than" in sort
        other.upload_speed.cmp(&self.upload_speed)
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

// ===========================================================================
// Leecher-state peer entry (snapshot for ranking)
// ===========================================================================

/// Snapshot of a peer's state used for leecher-state ranking.
///
/// Mirrors C++ `BtLeecherStateChoke::PeerEntry`.
#[derive(Debug, Clone)]
struct LeecherPeerEntry {
    /// Index back into the caller's peer list
    index: usize,
    /// Peer's download speed (bytes/sec), primary ranking criterion
    download_speed: i64,
    /// Whether this peer is a regular unchoker (interested AND sent data
    /// within `LEECHER_REGULAR_UNCHOKE_WINDOW`)
    regular_unchoker: bool,
}

impl LeecherPeerEntry {
    fn from_peer(index: usize, peer: &PeerStats) -> Self {
        let now = Instant::now();
        let regular_unchoker = peer.peer_interested
            && peer
                .last_data_time
                .is_some_and(|t| now.duration_since(t) < LEECHER_REGULAR_UNCHOKE_WINDOW);
        Self {
            index,
            download_speed: peer.download_speed as i64,
            regular_unchoker,
        }
    }
}

impl Ord for LeecherPeerEntry {
    /// Higher download speed = higher priority = "less than" in sort.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.download_speed.cmp(&self.download_speed)
    }
}

impl PartialOrd for LeecherPeerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for LeecherPeerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl Eq for LeecherPeerEntry {}

// ===========================================================================
// BtLeecherStateChoke — choking algorithm for leecher state
// ===========================================================================

/// Leecher-state choking algorithm.
///
/// When we are still downloading, we unchoke peers that provide the best
/// download speed (regular unchoke) and occasionally try a random peer
/// (optimistic unchoke).
///
/// Mirrors C++ `BtLeecherStateChoke`.
#[derive(Debug, Clone)]
pub struct BtLeecherStateChoke {
    /// Round counter (cycles 0..2, wrapping)
    round: u32,
    /// Timestamp of the last choke round execution.
    /// `None` means no round has been executed yet (mirrors C++ `Timer::zero()`).
    last_round: Option<Instant>,
}

impl BtLeecherStateChoke {
    /// Create a new leecher-state choke with default state.
    pub fn new() -> Self {
        Self {
            round: 0,
            last_round: None, // Mirrors C++ Timer::zero() — interval always elapsed
        }
    }

    /// Execute one round of the leecher-state choking algorithm.
    ///
    /// Algorithm:
    /// 1. Reset all peers to choked
    /// 2. Skip snubbed peers (no unchoke for them)
    /// 3. Round 0: planned optimistic unchoke on a random choked+interested peer
    /// 4. Regular unchoke: partition by regular-unchoker status, sort by speed,
    ///    unchoke top `LEECHER_REGULAR_UNCHOKE_SLOTS` interested peers
    ///
    /// Mirrors C++ `BtLeecherStateChoke::executeChoke()`.
    pub fn execute_choke(&mut self, peers: &mut [&mut PeerStats]) {
        tracing::debug!("Leecher state, {} choke round started", self.round);
        self.last_round = Some(Instant::now());

        // Phase 1: reset all peers to choked, collect entries (skip snubbed)
        let mut entries: Vec<LeecherPeerEntry> = Vec::new();
        for (i, peer) in peers.iter_mut().enumerate() {
            if peer.is_banned {
                continue;
            }
            peer.am_choking = true;
            if peer.is_snubbed {
                peer.opt_unchoking = false;
                continue;
            }
            entries.push(LeecherPeerEntry::from_peer(i, peer));
        }

        // Phase 2: planned optimistic unchoke (round 0 only)
        if self.round == 0 {
            self.planned_optimistic_unchoke(&mut entries, peers);
        }

        // Phase 3: regular unchoke
        self.regular_unchoke(&mut entries, peers);

        // Advance round (0 → 1 → 2 → 0 → …)
        self.round = (self.round + 1) % 3;
    }

    /// Planned optimistic unchoke: pick one random choked+interested peer.
    ///
    /// Mirrors C++ `BtLeecherStateChoke::plannedOptimisticUnchoke()`.
    fn planned_optimistic_unchoke(
        &mut self,
        entries: &mut [LeecherPeerEntry],
        peers: &mut [&mut PeerStats],
    ) {
        // Disable opt unchoking on all peers first
        for entry in entries.iter() {
            peers[entry.index].opt_unchoking = false;
        }

        // Partition: find choked+interested peers (mirrors C++ PeerFilter(true, true))
        let choked_interested: Vec<usize> = entries
            .iter()
            .filter(|e| peers[e.index].am_choking && peers[e.index].peer_interested)
            .map(|e| e.index)
            .collect();

        if choked_interested.is_empty() {
            return;
        }

        // Shuffle and pick first (mirrors C++ std::shuffle + pick begin)
        let mut rng = rand::thread_rng();
        let pick = choked_interested[rng.gen_range(0..choked_interested.len())];
        peers[pick].opt_unchoking = true;
        tracing::debug!("POU (leecher): peer idx={}", pick);
    }

    /// Regular unchoke: partition by regular-unchoker status, sort, unchoke top N.
    ///
    /// Mirrors C++ `BtLeecherStateChoke::regularUnchoke()`.
    ///
    /// **C++ equivalence note**: The C++ `for` loop decrements `count` in the
    /// increment expression (`--count`), which runs even on `continue`. This means
    /// not-interested peers *consume* an unchoke slot without being unchoked.
    /// We replicate this behavior for strict C++ equivalence.
    fn regular_unchoke(&mut self, entries: &mut [LeecherPeerEntry], peers: &mut [&mut PeerStats]) {
        // Partition: regular unchokers first, then sort by download speed
        entries.sort_by(|a, b| match (a.regular_unchoker, b.regular_unchoker) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.cmp(b),
        });

        // Shuffle the non-regular-unchoker partition for variety.
        // Mirrors C++ `std::shuffle(rest, end, rng)` after partition.
        let first_non_regular = entries
            .iter()
            .position(|e| !e.regular_unchoker)
            .unwrap_or(entries.len());
        if first_non_regular < entries.len() {
            let mut rng = rand::thread_rng();
            // Fisher-Yates partial shuffle on the tail
            for i in (first_non_regular..entries.len()).rev() {
                let range_size = i - first_non_regular;
                if range_size == 0 {
                    break;
                }
                let j = first_non_regular + rng.gen_range(0..=range_size);
                entries.swap(i, j);
            }
        }

        // Unchoke the top N peers. In C++, the for-loop increment decrements
        // count even on `continue` (when `!peer->peerInterested()`), so
        // not-interested peers consume a slot. We replicate this.
        let mut count = LEECHER_REGULAR_UNCHOKE_SLOTS as i32;
        let mut fast_opt_unchoker = false;

        for entry in entries.iter() {
            if count <= 0 {
                break;
            }
            let peer = &mut peers[entry.index];

            if !peer.peer_interested {
                // C++ `continue` still decrements count in the for-increment
                count -= 1;
                continue;
            }

            // Unchoke this peer
            peer.am_choking = false;
            peer.record_unchoke();
            count -= 1;

            tracing::debug!(
                "RU (leecher): peer idx={}, dlspd={}",
                entry.index,
                entry.download_speed
            );

            if peer.opt_unchoking {
                fast_opt_unchoker = true;
                peer.opt_unchoking = false;
            }
        }

        // If a regular unchoke consumed an optimistic-unchoke peer,
        // promote the next interested peer to optimistic unchoke
        if fast_opt_unchoker {
            for entry in entries.iter() {
                if !peers[entry.index].peer_interested {
                    continue;
                }
                peers[entry.index].opt_unchoking = true;
                tracing::debug!("OU (leecher): peer idx={}", entry.index);
                break;
            }
        }
    }

    /// Return the current round counter.
    pub fn round(&self) -> u32 {
        self.round
    }

    /// Set the round counter (for testing purposes).
    #[cfg(test)]
    pub fn set_round(&mut self, round: u32) {
        self.round = round;
    }

    /// Return the timestamp of the last choke round execution.
    ///
    /// Mirrors C++ `BtLeecherStateChoke::getLastRound()`.
    /// Returns `None` if no round has been executed yet (equivalent to C++
    /// `Timer::zero()` where the interval is always considered elapsed).
    pub fn last_round_time(&self) -> Option<Instant> {
        self.last_round
    }

    /// Check whether enough time has elapsed since the last choke round
    /// to warrant another execution.
    ///
    /// Mirrors the interval check in C++ `PeerChokeCommand::execute()`
    /// which calls `peerStorage_->chokeRoundIntervalElapsed()`.
    /// Returns `true` if no round has been executed yet.
    pub fn should_execute(&self, interval: Duration) -> bool {
        match self.last_round {
            None => true,
            Some(t) => t.elapsed() >= interval,
        }
    }
}

impl Default for BtLeecherStateChoke {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// BtSeederStateChoke — choking algorithm for seeder state
// ===========================================================================

/// Seeder-state choking algorithm.
///
/// When we are seeding (download complete), we rank peers by:
/// 1. Outstanding upload (currently uploading to us) — highest priority
/// 2. Recently unchoked (within 20 s window) — second priority
/// 3. Upload speed — fallback
///
/// A 3-round cycle controls optimistic unchoke: rounds 0-1 pick one
/// random peer beyond the regular unchoke slots; round 2 does not.
///
/// Mirrors C++ `BtSeederStateChoke`.
#[derive(Debug, Clone)]
pub struct BtSeederStateChoke {
    /// Round counter (cycles 0..2, wrapping)
    round: u32,
    /// Timestamp of the last choke round execution.
    /// `None` means no round has been executed yet (mirrors C++ `Timer::zero()`).
    last_round: Option<Instant>,
    /// Number of upload slots for regular unchoke.
    /// Round 2 uses +1 slot (4 vs 3) matching C++ logic.
    base_unchoke_slots: usize,
}

impl BtSeederStateChoke {
    /// Create a new seeder-state choke with default state (4 base slots).
    pub fn new() -> Self {
        Self {
            round: 0,
            last_round: None, // Mirrors C++ Timer::zero() — interval always elapsed
            base_unchoke_slots: 4,
        }
    }

    /// Create a seeder-state choke with a custom number of unchoke slots.
    pub fn with_slots(slots: usize) -> Self {
        Self {
            round: 0,
            last_round: None,
            base_unchoke_slots: slots,
        }
    }

    /// Execute one round of the seeder-state choking algorithm.
    ///
    /// Algorithm:
    /// 1. Reset all active peers to choked
    /// 2. Collect interested peers into ranking entries
    /// 3. Sort entries by priority (outstanding upload > recent unchoke > speed)
    /// 4. Unchoke top-N entries (N = base_slots, or base_slots+1 on round 2)
    /// 5. Rounds 0-1: optimistic unchoke on a random remaining peer
    ///
    /// Mirrors C++ `BtSeederStateChoke::executeChoke()`.
    pub fn execute_choke(&mut self, peers: &mut [&mut PeerStats]) {
        tracing::debug!("Seeder state, {} choke round started", self.round);
        self.last_round = Some(Instant::now());

        // Phase 1: reset all peers to choked, collect interested peers
        let mut entries: Vec<SeederPeerEntry> = Vec::new();
        for (i, peer) in peers.iter_mut().enumerate() {
            if peer.is_banned {
                continue;
            }
            peer.am_choking = true;
            if peer.peer_interested {
                entries.push(SeederPeerEntry::from_peer(i, peer));
                continue;
            }
            // Not interested → no optimistic unchoke either
            peer.opt_unchoking = false;
        }

        // Phase 2: unchoke top peers by ranking
        self.unchoke_peers(&mut entries, peers);

        // Advance round (0 → 1 → 2 → 0 → …)
        self.round = (self.round + 1) % 3;
    }

    /// Unchoke the top-ranked peers and optionally perform optimistic unchoke.
    ///
    /// Mirrors C++ `BtSeederStateChoke::unchoke()`.
    fn unchoke_peers(&mut self, entries: &mut Vec<SeederPeerEntry>, peers: &mut [&mut PeerStats]) {
        // Round 2 gets one more regular slot (C++: count = (round==2) ? 4 : 3)
        let regular_slots = if self.round == 2 {
            self.base_unchoke_slots
        } else {
            self.base_unchoke_slots.saturating_sub(1)
        };

        entries.sort();

        let split_point = entries.len().min(regular_slots);

        // Regular unchoke: top-N peers (use index-based iteration to avoid
        // long-lived mutable borrows from split_at_mut conflicting with
        // the optimistic-unchoke phase below).
        for i in 0..split_point {
            let entry = &entries[i];
            let peer = &mut peers[entry.index];
            peer.am_choking = false;
            peer.record_unchoke();
            tracing::debug!(
                "RU (seeder): peer idx={}, ulspd={}",
                entry.index,
                entry.upload_speed
            );
        }

        // Optimistic unchoke (rounds 0-1 only)
        if self.round < 2 {
            // Disable opt unchoking on all peers first
            for entry in entries.iter() {
                peers[entry.index].opt_unchoking = false;
            }

            // Pick a random peer from the tail (not regularly unchoked)
            if entries.len() > split_point {
                let mut rng = rand::thread_rng();
                let pick_idx = split_point + rng.gen_range(0..entries.len() - split_point);
                let picked_index = entries[pick_idx].index;
                peers[picked_index].opt_unchoking = true;
                peers[picked_index].am_choking = false;
                peers[picked_index].record_optimistic_unchoke();
                tracing::debug!("POU (seeder): peer idx={}", picked_index);
            }
        }
    }

    /// Return the timestamp of the last choke round execution.
    ///
    /// Mirrors C++ `BtSeederStateChoke::getLastRound()`.
    /// Returns `None` if no round has been executed yet (equivalent to C++
    /// `Timer::zero()` where the interval is always considered elapsed).
    pub fn last_round_time(&self) -> Option<Instant> {
        self.last_round
    }

    /// Return the current round counter.
    pub fn round(&self) -> u32 {
        self.round
    }

    /// Set the round counter (for testing purposes).
    #[cfg(test)]
    pub fn set_round(&mut self, round: u32) {
        self.round = round;
    }

    /// Check whether enough time has elapsed since the last choke round
    /// to warrant another execution.
    ///
    /// Mirrors the interval check in C++ `PeerChokeCommand::execute()`
    /// which calls `peerStorage_->chokeRoundIntervalElapsed()`.
    /// Returns `true` if no round has been executed yet.
    pub fn should_execute(&self, interval: Duration) -> bool {
        match self.last_round {
            None => true,
            Some(t) => t.elapsed() >= interval,
        }
    }
}

impl Default for BtSeederStateChoke {
    fn default() -> Self {
        Self::new()
    }
}

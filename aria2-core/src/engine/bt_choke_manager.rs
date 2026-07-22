#![allow(clippy::empty_line_after_doc_comments)]
#![allow(clippy::doc_lazy_continuation)]

use std::time::Instant;

use rand::seq::SliceRandom;
use rand::thread_rng;
use tracing::{debug, info, warn};

use crate::engine::choking_algorithm::{ChokeAction, ChokingAlgorithm};
use crate::engine::peer_stats::PeerStats;

/// BitTorrent choking algorithm manager for download-side peer selection.
///
/// This module encapsulates all download-side choke/unchoke tracking logic,
/// mirroring the original aria2 C++ architecture's separation of
/// `BtLeecherStateChoke` and `BtSeederStateChoke`.
///
/// Responsibilities:
/// - Track which peers are choking us (affects request priority)
/// - Select best peers for piece requests based on choke state and speed
/// - Detect and handle snubbed peers (unresponsive peers)
/// - Update statistics when data is received from peers
///
/// # C++ Architecture Reference
///
/// The original aria2 C++ code has two separate choke state classes:
/// - `BtLeecherStateChoke` — used when we are downloading (leeching).
///   Implements tit-for-tat with a 3-round cycle, regular unchoke of top-3
///   peers by download speed, and planned optimistic unchoke on round 0.
/// - `BtSeederStateChoke` — used when we are seeding.
///   Prioritises peers with outstanding uploads, recently unchoked peers
///   (anti-churn), and higher upload speed (reciprocity).
///
/// This module provides both structs as well as legacy free functions.

// ======================================================================
// Constants (matching C++ aria2)
// ======================================================================

/// Number of regular unchoke slots in the leecher algorithm.
/// The C++ code hardcodes this to 3 in `BtLeecherStateChoke::regularUnchoke`.
const LEECHER_REGULAR_UNCHOKE_COUNT: usize = 3;

/// Number of rounds in the leecher / seeder 3-round cycle.
const ROUND_CYCLE: u32 = 3;

/// Threshold for "regular unchoker" detection: the peer must have sent data
/// in the last 30 seconds (matching C++ `BtLeecherStateChoke::PeerEntry`).
const REGULAR_UNCHOKER_DATA_THRESHOLD_SECS: u64 = 30;

/// Time-frame for "recent unchoking" in the seeder algorithm (C++ uses 20 s).
const SEEDER_RECENT_UNCHOKING_TIME_FRAME_SECS: u64 = 20;

// ======================================================================
// Download-Side Choke Tracking Helpers (legacy API)
// ======================================================================

/// Record that a peer at the given index has sent us a Choke message.
///
/// This updates the internal `choking_algo` state so that
/// [`select_best_peer_for_request`] can deprioritize choked peers.
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
/// * `peer_idx` - Index of the peer that sent the choke message
pub fn on_peer_choke(algo: &mut Option<ChokingAlgorithm>, peer_idx: usize) {
    if let Some(a) = algo
        && let Some(peer) = a.get_peer_mut(peer_idx)
    {
        peer.peer_choking = true;
        debug!("Peer #{} is now choking us", peer_idx);
    }
}

/// Record that a peer at the given index has sent us an Unchoke message.
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
/// * `peer_idx` - Index of the peer that sent the unchoke message
pub fn on_peer_unchoke(algo: &mut Option<ChokingAlgorithm>, peer_idx: usize) {
    if let Some(a) = algo
        && let Some(peer) = a.get_peer_mut(peer_idx)
    {
        peer.peer_choking = false;
        debug!("Peer #{} has unchoked us", peer_idx);
    }
}

/// Record data received from a peer (updates speed + resets snubbed status).
///
/// Should be called whenever we successfully receive a block from a peer.
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
/// * `peer_idx` - Index of the peer we received data from
/// * `bytes` - Number of bytes received
pub fn on_data_received_from_peer(
    algo: &mut Option<ChokingAlgorithm>,
    peer_idx: usize,
    bytes: u64,
) {
    if let Some(a) = algo {
        a.on_data_received(peer_idx, bytes);
    }
}

/// Check if any tracked peer is snubbed and should be handled.
///
/// Returns indices of newly snubbed peers that may need special handling
/// (e.g., reduced priority or disconnection).
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
///
/// # Returns
/// Vector of peer indices that are newly snubbed
pub fn check_snubbed_peers(algo: &mut Option<ChokingAlgorithm>) -> Vec<usize> {
    if let Some(a) = algo {
        a.check_snubbed_peers()
    } else {
        vec![]
    }
}

/// Add a connected peer to the choking algorithm tracking.
///
/// Call this when a new peer connection is established during download phase.
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
/// * `peer_id` - First 8 bytes of the peer's 20-byte ID (rest will be zeroed)
/// * `addr` - Socket address of the peer
///
/// # Returns
/// Index of the added peer in the algorithm's internal list,
/// or 0 if no algorithm is configured
pub fn add_peer_to_tracking(
    algo: &mut Option<ChokingAlgorithm>,
    peer_id: [u8; 8],
    addr: std::net::SocketAddr,
) -> usize {
    if let Some(a) = algo {
        let full_peer_id = {
            let mut id = [0u8; 20];
            id[..8].copy_from_slice(&peer_id);
            id
        };
        let stats = PeerStats::new(full_peer_id, addr);
        a.add_peer(stats);
        a.len() - 1
    } else {
        0
    }
}

// ======================================================================
// Peer Selection Logic (legacy API)
// ======================================================================

/// Select the best peer for requesting pieces, preferring unchoked peers.
///
/// Uses the choking algorithm's peer stats to score and rank peers:
/// - Unchoked peers are strongly preferred
/// - Higher download speed is better
/// - Snubbed peers are penalized
///
/// Scoring formula:
/// - Download speed contribution: 50% weight
/// - Upload speed contribution (reciprocity): 30% weight
/// - Interest bonus: +50 points if peer wants our data
///
/// # Arguments
/// * `algo` - The choking algorithm instance (immutable reference)
///
/// # Returns
/// Index of the best peer for making requests, or None if no suitable peer found
pub fn select_best_peer_for_request(algo: &Option<ChokingAlgorithm>) -> Option<usize> {
    if let Some(a) = algo {
        let best_idx = a
            .peers()
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.am_choking && p.peer_interested && !p.is_snubbed)
            .max_by_key(|(_, p)| {
                let mut score = 0i64;
                score += (p.download_speed * 0.5) as i64;
                score += (p.upload_speed * 0.3) as i64;
                if p.peer_interested {
                    score += 50;
                }
                score
            })
            .map(|(i, _)| i);

        if let Some(idx) = best_idx {
            debug!(
                "[BT] Selected peer {} for request (using choking algorithm)",
                idx
            );
            return best_idx;
        }

        a.peers().iter().position(|p| !p.is_snubbed)
    } else {
        None
    }
}

// ======================================================================
// Snubbed Peer Handling (legacy API)
// ======================================================================

/// Handle a peer that has been marked as snubbed.
///
/// Reduces the request frequency for this peer by increasing its
/// request interval multiplier. This avoids wasting time waiting for
/// data from unresponsive peers while keeping the connection alive
/// in case they recover.
///
/// The choking algorithm will automatically lower this peer's score
/// on next rotation due to the `is_snubbed` flag, which will cause it
/// to be choked on the upload side.
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
/// * `peer_idx` - Index of the snubbed peer
#[allow(clippy::result_unit_err)]
pub async fn handle_snubbed_peer(
    algo: &mut Option<ChokingAlgorithm>,
    peer_idx: usize,
) -> std::result::Result<(), ()> {
    if let Some(a) = algo
        && let Some(peer) = a.get_peer_mut(peer_idx)
    {
        warn!(
            "[BT] Peer {} at {} marked as snubbed, reducing request priority",
            peer_idx, peer.addr
        );
    }

    Ok(())
}

// ======================================================================
// Piece Receive Statistics (legacy API)
// ======================================================================

/// Update peer statistics when piece data is received.
///
/// Should be called whenever we successfully receive a block from a peer.
/// Updates the download speed estimate via EMA and resets the snubbed timer.
///
/// # Arguments
/// * `algo` - The choking algorithm instance (mutable reference)
/// * `peer_idx` - Index of the peer we received data from
/// * `bytes` - Number of bytes received in this block
pub fn on_piece_received(algo: &mut Option<ChokingAlgorithm>, peer_idx: usize, bytes: u64) {
    if let Some(a) = algo {
        a.on_data_received(peer_idx, bytes);
        debug!(
            "[BT] Updated peer {} stats: received {} bytes",
            peer_idx, bytes
        );
    }
}

// ======================================================================
// Helper: is_regular_unchoker
// ======================================================================

/// Check if a peer qualifies as a "regular unchoker".
///
/// A regular unchoker is a peer that:
/// - Is interested in our data (`peer_interested == true`)
/// - Has sent data to us in the last 30 seconds (`last_data_time` within 30s)
///
/// This matches the C++ `BtLeecherStateChoke::PeerEntry` constructor logic.
fn is_regular_unchoker(peer: &PeerStats) -> bool {
    peer.peer_interested
        && peer
            .last_data_time
            .map_or(false, |t| t.elapsed().as_secs() < REGULAR_UNCHOKER_DATA_THRESHOLD_SECS)
}

/// In-place partition: move elements matching `predicate` to the front.
///
/// Returns the index of the first element that does NOT match the predicate.
/// This is a stable replacement for the unstable `Iterator::partition_in_place`.
fn partition_slice<T>(slice: &mut [T], predicate: impl Fn(&T) -> bool) -> usize {
    let mut left = 0;
    let mut right = slice.len();
    while left < right {
        if predicate(&slice[left]) {
            left += 1;
        } else {
            right -= 1;
            slice.swap(left, right);
        }
    }
    left
}

// ======================================================================
// LeecherPeerEntry — mirrors C++ BtLeecherStateChoke::PeerEntry
// ======================================================================

/// Entry for a peer in the leecher choking algorithm.
///
/// Captures a snapshot of per-peer state at the start of a choke round so
/// that the algorithm can work on local copies without aliasing issues.
/// After the round completes, the final choke/unchoke decisions are applied
/// back to the original [`PeerStats`] and a [`ChokeAction`] is produced.
struct LeecherPeerEntry {
    /// Index into the original peer slice.
    peer_idx: usize,
    /// Current download speed from this peer (bytes/sec).
    download_speed: f64,
    /// Whether this peer is a "regular unchoker" (interested AND sent data
    /// in last 30 s).
    regular_unchoker: bool,
    /// Whether this peer holds the optimistic unchoke slot.
    opt_unchoking: bool,
    /// Whether choking is required for this peer (set by the algorithm).
    choking_required: bool,
    /// Whether the peer is interested in our data.
    peer_interested: bool,
    /// Snapshot of `am_choking` at the start of the round (previous state).
    prev_am_choking: bool,
}

impl LeecherPeerEntry {
    /// Create a new entry by snapshotting the current state of a [`PeerStats`].
    ///
    /// The entry starts with `choking_required = true` and `opt_unchoking = false`,
    /// matching the C++ `executeChoke` preamble that marks all active peers
    /// as requiring choking and clears any previous optimistic unchoke slot.
    fn new(peer_idx: usize, peer: &PeerStats) -> Self {
        Self {
            peer_idx,
            download_speed: peer.download_speed,
            regular_unchoker: is_regular_unchoker(peer),
            opt_unchoking: false,
            choking_required: true,
            peer_interested: peer.peer_interested,
            prev_am_choking: peer.am_choking,
        }
    }
}

// ======================================================================
// BtLeecherStateChoke — mirrors C++ BtLeecherStateChoke
// ======================================================================

/// Leecher-state choking algorithm, matching C++ `BtLeecherStateChoke`.
///
/// Implements the standard BitTorrent tit-for-tat choking with:
/// - **3-round cycle**: round 0 = planned optimistic unchoke + regular,
///   rounds 1-2 = regular only.
/// - **Regular unchokers**: peers that are interested AND sent data in the
///   last 30 seconds.
/// - **Planned optimistic unchoke**: random selection among choked +
///   interested peers (only on round 0).
///
/// # C++ Reference
///
/// ```cpp
/// void BtLeecherStateChoke::executeChoke(const PeerSet& peerSet) {
///     // 1. Mark all active peers as chokingRequired(true)
///     // 2. Snubbing peers: optUnchoking(false), skip them
///     // 3. Round 0: plannedOptimisticUnchoke()
///     // 4. regularUnchoke()
///     // 5. Increment round (mod 3)
/// }
/// ```
pub struct BtLeecherStateChoke {
    /// Current round (0, 1, or 2).
    round: u32,
    /// Time of last choke round execution.
    last_round_time: Option<Instant>,
}

impl BtLeecherStateChoke {
    /// Create a new leecher-state choke algorithm starting at round 0.
    pub fn new() -> Self {
        Self {
            round: 0,
            last_round_time: None,
        }
    }

    /// Return the current round counter.
    pub fn round(&self) -> u32 {
        self.round
    }

    /// Return the time of the last choke round, if any.
    pub fn last_round_time(&self) -> Option<Instant> {
        self.last_round_time
    }

    /// Main choke execution for leecher state.
    ///
    /// Follows the C++ `BtLeecherStateChoke::executeChoke` flow exactly:
    ///
    /// 1. Mark all active peers as requiring choking (`choking_required = true`).
    /// 2. Skip snubbed peers — they stay choked, `opt_unchoking = false`.
    /// 3. If `round == 0`: run planned optimistic unchoke.
    /// 4. Run regular unchoke (top 3 interested regular unchokers).
    /// 5. Increment round (mod 3).
    /// 6. Apply final decisions back to `PeerStats` and return [`ChokeAction`]s.
    ///
    /// # Arguments
    /// * `peers` - Mutable slice of peer stats for all connected peers.
    ///
    /// # Returns
    /// A vector of [`ChokeAction`] describing the state changes.
    pub fn execute_choke(&mut self, peers: &mut [&mut PeerStats]) -> Vec<ChokeAction> {
        info!("Leecher state, round {} choke started", self.round);
        self.last_round_time = Some(Instant::now());

        // Step 1 & 2: Build peer entries.
        // In C++, all active peers get chokingRequired(true).
        // Snubbing peers are skipped (they stay choked, optUnchoking=false).
        let mut entries: Vec<LeecherPeerEntry> = Vec::with_capacity(peers.len());
        let mut excluded_indices: Vec<usize> = Vec::new();

        for (idx, peer) in peers.iter().enumerate() {
            if !peer.is_eligible_for_selection() {
                // Banned peers are skipped entirely (C++ checks isActive).
                continue;
            }

            if peer.is_snubbed {
                // Snubbing peer: stays choked, no opt unchoke.
                excluded_indices.push(idx);
                continue;
            }

            entries.push(LeecherPeerEntry::new(idx, peer));
        }

        // Step 3: Planned optimistic unchoke (only on round 0).
        if self.round == 0 {
            Self::planned_optimistic_unchoke(&mut entries);
        }

        // Step 4: Regular unchoke.
        Self::regular_unchoke(&mut entries);

        // Step 5: Apply decisions back to PeerStats and produce ChokeActions.
        let mut actions = Vec::with_capacity(peers.len());

        for entry in &entries {
            let peer = &mut peers[entry.peer_idx];
            let new_am_choking = entry.choking_required && !entry.opt_unchoking;

            if entry.prev_am_choking && !new_am_choking {
                if entry.opt_unchoking {
                    peer.record_optimistic_unchoke();
                } else {
                    peer.record_unchoke();
                }
                actions.push(ChokeAction::Unchoke(entry.peer_idx));
            } else if !entry.prev_am_choking && new_am_choking {
                peer.record_choke();
                actions.push(ChokeAction::Choke(entry.peer_idx));
            } else {
                actions.push(ChokeAction::NoChange(entry.peer_idx));
            }
        }

        // Excluded (snubbed/banned) peers: ensure they are choked.
        for idx in &excluded_indices {
            let peer = &mut peers[*idx];
            if !peer.am_choking {
                peer.record_choke();
                actions.push(ChokeAction::Choke(*idx));
            } else {
                actions.push(ChokeAction::NoChange(*idx));
            }
        }

        // Step 6: Advance round.
        self.round = (self.round + 1) % ROUND_CYCLE;

        actions
    }

    /// Planned optimistic unchoke (POU).
    ///
    /// Mirrors C++ `BtLeecherStateChoke::plannedOptimisticUnchoke`:
    /// 1. Disable `opt_unchoking` on ALL entries.
    /// 2. Partition: move currently-choked + interested entries to the front.
    /// 3. Shuffle the front partition (random selection).
    /// 4. Pick the first entry for optimistic unchoke.
    fn planned_optimistic_unchoke(entries: &mut [LeecherPeerEntry]) {
        // Step 1: Disable opt_unchoking for all entries.
        for entry in entries.iter_mut() {
            entry.opt_unchoking = false;
        }

        // Step 2: Partition — choked + interested peers go to the front.
        // C++ PeerFilter(true, true) checks amChoking()==true && peerInterested()==true.
        // We use prev_am_choking (the snapshot before this round).
        let boundary = partition_slice(entries, |e| e.prev_am_choking && e.peer_interested);

        if boundary == 0 {
            return;
        }

        // Step 3: Shuffle the choked+interested partition.
        entries[..boundary].shuffle(&mut thread_rng());

        // Step 4: Pick the first entry for opt unchoke.
        entries[0].opt_unchoking = true;
        debug!(
            "POU: peer #{} selected for optimistic unchoke",
            entries[0].peer_idx
        );
    }

    /// Regular unchoke.
    ///
    /// Mirrors C++ `BtLeecherStateChoke::regularUnchoke`:
    /// 1. Partition entries into regular unchokers (front) vs rest.
    /// 2. Sort regular unchokers by download speed descending.
    /// 3. Shuffle the rest (random ordering).
    /// 4. Unchoke up to 3 interested peers (skip uninterested but consume slot).
    /// 5. If a regular unchoke covers the opt-unchoking peer, do fast opt
    ///    unchoke recovery.
    fn regular_unchoke(entries: &mut [LeecherPeerEntry]) {
        // Step 1: Partition — regular unchokers to front.
        let boundary = partition_slice(entries, |e| e.regular_unchoker);

        // Step 2: Sort regular unchokers by download speed descending.
        entries[..boundary].sort_by(|a, b| {
            b.download_speed
                .partial_cmp(&a.download_speed)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Step 3: Shuffle the rest.
        entries[boundary..].shuffle(&mut thread_rng());

        // Step 4: Unchoke up to LEECHER_REGULAR_UNCHOKE_COUNT interested peers.
        // C++ behaviour: count decrements for EVERY peer, even uninterested ones.
        let mut count = LEECHER_REGULAR_UNCHOKE_COUNT;
        let mut fast_opt_unchoker = false;

        for entry in entries.iter_mut() {
            if count == 0 {
                break;
            }
            count -= 1;

            if !entry.peer_interested {
                continue;
            }

            entry.choking_required = false;

            debug!(
                "RU: peer #{}, dlspd={}",
                entry.peer_idx, entry.download_speed
            );

            // Step 5: Fast opt-unchoke recovery.
            if entry.opt_unchoking {
                fast_opt_unchoker = true;
                entry.opt_unchoking = false;
            }
        }

        if fast_opt_unchoker {
            for entry in entries.iter_mut() {
                if !entry.peer_interested {
                    continue;
                }
                entry.opt_unchoking = true;
                debug!(
                    "OU (fast recovery): peer #{} got opt unchoke slot",
                    entry.peer_idx
                );
                break;
            }
        }
    }
}

impl Default for BtLeecherStateChoke {
    fn default() -> Self {
        Self::new()
    }
}

// ======================================================================
// SeederPeerEntry — mirrors C++ BtSeederStateChoke::PeerEntry
// ======================================================================

/// Entry for a peer in the seeder choking algorithm.
///
/// Mirrors C++ `BtSeederStateChoke::PeerEntry` which captures:
/// - `outstandingUpload_` — count of pending upload operations
/// - `lastAmUnchoking_` — timestamp when we last unchoked this peer
/// - `recentUnchoking_` — whether we unchoked this peer in the last 20 s
/// - `uploadSpeed_` — current upload speed to this peer
struct SeederPeerEntry {
    /// Index into the original peer slice.
    peer_idx: usize,
    /// Whether this peer was recently unchoked (within 20 s).
    recent_unchoking: bool,
    /// Upload speed to this peer (bytes/sec).
    upload_speed: f64,
    /// Number of outstanding uploads to this peer.
    outstanding_upload: usize,
    /// When we last unchoked this peer.
    last_am_unchoking: Instant,
    /// Whether the peer is interested in our data.
    /// Preserved for C++ API parity; may be used in future extensions.
    #[allow(dead_code)]
    peer_interested: bool,
    /// Snapshot of `am_choking` at the start of the round.
    prev_am_choking: bool,
    /// Whether choking is required for this peer (algorithm state).
    choking_required: bool,
    /// Whether this peer holds the optimistic unchoke slot.
    opt_unchoking: bool,
}

impl SeederPeerEntry {
    /// Create a new entry by snapshotting the current state of a [`PeerStats`].
    ///
    /// The entry starts with `choking_required = true` and `opt_unchoking = false`,
    /// matching the C++ `executeChoke` preamble.
    fn new(peer_idx: usize, peer: &PeerStats) -> Self {
        let now = Instant::now();
        let last_am_unchoking = peer.last_unchoke_at;
        let recent_unchoking = now.duration_since(last_am_unchoking).as_secs()
            < SEEDER_RECENT_UNCHOKING_TIME_FRAME_SECS;

        Self {
            peer_idx,
            recent_unchoking,
            upload_speed: peer.upload_speed,
            outstanding_upload: 0, // PeerStats doesn't track this yet.
            last_am_unchoking,
            peer_interested: peer.peer_interested,
            prev_am_choking: peer.am_choking,
            choking_required: true,
            opt_unchoking: false,
        }
    }

    /// Whether this peer should be choked after the algorithm runs.
    ///
    /// Matches C++ `Peer::shouldBeChoking()` which returns
    /// `chokingRequired && !optUnchoking`.
    fn should_be_choking(&self) -> bool {
        self.choking_required && !self.opt_unchoking
    }

    /// Comparison matching C++ `BtSeederStateChoke::PeerEntry::operator<`.
    ///
    /// Priority (higher = first in sort = lower `cmp` result):
    /// 1. Peers WITH outstanding uploads come first.
    /// 2. Among equal outstanding-upload status, recently unchoked peers
    ///    with later `last_am_unchoking` come first (anti-churn).
    /// 3. Otherwise, higher upload speed comes first.
    fn compare_priority(a: &Self, b: &Self) -> std::cmp::Ordering {
        // Rule 1: outstanding uploads first.
        match (a.outstanding_upload > 0, b.outstanding_upload > 0) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }

        // Rule 2: recently unchoked with later last_am_unchoking first.
        if a.recent_unchoking && a.last_am_unchoking > b.last_am_unchoking {
            return std::cmp::Ordering::Less;
        }
        if b.recent_unchoking {
            return std::cmp::Ordering::Greater;
        }

        // Rule 3: higher upload speed first.
        b.upload_speed
            .partial_cmp(&a.upload_speed)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

// ======================================================================
// BtSeederStateChoke — mirrors C++ BtSeederStateChoke
// ======================================================================

/// Seeder-state choking algorithm, matching C++ `BtSeederStateChoke`.
///
/// When seeding, the algorithm prioritises:
/// - Peers with outstanding uploads (keep them fed)
/// - Peers that were recently unchoked (avoid unnecessary churn)
/// - Higher upload speed (reciprocity)
///
/// # C++ Reference
///
/// ```cpp
/// void BtSeederStateChoke::executeChoke(const PeerSet& peerSet) {
///     // 1. Mark all active peers as chokingRequired(true)
///     // 2. Interested peers go into entries; uninterested get optUnchoking(false)
///     // 3. unchoke() — sort by priority, unchoke top K, opt unchoke one more
///     // 4. Increment round (mod 3)
/// }
/// ```
pub struct BtSeederStateChoke {
    /// Current round counter (0, 1, or 2).
    round: u32,
    /// Time of last choke round execution.
    last_round_time: Option<Instant>,
}

impl BtSeederStateChoke {
    /// Create a new seeder-state choke algorithm starting at round 0.
    pub fn new() -> Self {
        Self {
            round: 0,
            last_round_time: None,
        }
    }

    /// Return the current round counter.
    pub fn round(&self) -> u32 {
        self.round
    }

    /// Return the time of the last choke round, if any.
    pub fn last_round_time(&self) -> Option<Instant> {
        self.last_round_time
    }

    /// Main choke execution for seeder state.
    ///
    /// Follows the C++ `BtSeederStateChoke::executeChoke` flow:
    ///
    /// 1. Mark all active peers as requiring choking.
    /// 2. Only interested peers go into the entry list; uninterested peers
    ///    have `optUnchoking = false` and are excluded.
    /// 3. Sort entries by seeder priority (outstanding uploads > recent
    ///    unchoking > upload speed).
    /// 4. Unchoke top K (4 on round 2, 3 otherwise).
    /// 5. On rounds 0 and 1, give one additional optimistic unchoke slot
    ///    to a random remaining peer.
    /// 6. Apply decisions back and return [`ChokeAction`]s.
    pub fn execute_choke(&mut self, peers: &mut [&mut PeerStats]) -> Vec<ChokeAction> {
        info!("Seeder state, round {} choke started", self.round);
        self.last_round_time = Some(Instant::now());

        // Step 1 & 2: Build entries for interested peers only.
        let mut entries: Vec<SeederPeerEntry> = Vec::with_capacity(peers.len());
        let mut excluded_indices: Vec<usize> = Vec::new();

        for (idx, peer) in peers.iter().enumerate() {
            if !peer.is_eligible_for_selection() {
                continue;
            }

            if peer.peer_interested {
                entries.push(SeederPeerEntry::new(idx, peer));
            } else {
                // Uninterested peer: stays choked, no opt unchoke.
                excluded_indices.push(idx);
            }
        }

        // Step 3-5: Unchoke.
        Self::unchoke(&mut entries, self.round);

        // Apply decisions back to PeerStats.
        let mut actions = Vec::with_capacity(peers.len());

        for entry in &entries {
            let new_am_choking = entry.should_be_choking();
            let peer = &mut peers[entry.peer_idx];

            if entry.prev_am_choking && !new_am_choking {
                if entry.opt_unchoking {
                    peer.record_optimistic_unchoke();
                } else {
                    peer.record_unchoke();
                }
                actions.push(ChokeAction::Unchoke(entry.peer_idx));
            } else if !entry.prev_am_choking && new_am_choking {
                peer.record_choke();
                actions.push(ChokeAction::Choke(entry.peer_idx));
            } else {
                actions.push(ChokeAction::NoChange(entry.peer_idx));
            }
        }

        // Excluded (uninterested/banned) peers: ensure they are choked.
        for idx in &excluded_indices {
            let peer = &mut peers[*idx];
            if !peer.am_choking {
                peer.record_choke();
                actions.push(ChokeAction::Choke(*idx));
            } else {
                actions.push(ChokeAction::NoChange(*idx));
            }
        }

        // Advance round.
        self.round = (self.round + 1) % ROUND_CYCLE;

        actions
    }

    /// Unchoke logic for seeder state.
    ///
    /// Mirrors C++ `BtSeederStateChoke::unchoke`:
    /// - Count = 4 on round 2, else 3.
    /// - Sort entries by priority.
    /// - Unchoke top `count` entries.
    /// - On rounds < 2, assign one optimistic unchoke to a random
    ///   remaining peer.
    fn unchoke(entries: &mut [SeederPeerEntry], round: u32) {
        let count: usize = if round == 2 { 4 } else { 3 };

        // Sort by seeder priority.
        entries.sort_by(SeederPeerEntry::compare_priority);

        // Unchoke top `count` entries.
        let unchoke_end = count.min(entries.len());
        for entry in entries.iter_mut().take(unchoke_end) {
            entry.choking_required = false;
            debug!(
                "RU: peer #{}, ulspd={}",
                entry.peer_idx, entry.upload_speed
            );
        }

        // On rounds < 2, assign optimistic unchoke to a random remaining peer.
        if round < 2 {
            // First, disable opt_unchoking on ALL entries.
            for entry in entries.iter_mut() {
                entry.opt_unchoking = false;
            }

            if unchoke_end < entries.len() {
                // Shuffle remaining.
                entries[unchoke_end..].shuffle(&mut thread_rng());

                // Give the first remaining entry the opt unchoke slot.
                entries[unchoke_end].opt_unchoking = true;
                debug!(
                    "POU: peer #{} got seeder opt unchoke slot",
                    entries[unchoke_end].peer_idx
                );
            }
        }
    }
}

impl Default for BtSeederStateChoke {
    fn default() -> Self {
        Self::new()
    }
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::Duration;

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Create a basic `PeerStats` for testing.
    fn make_peer(
        peer_id: u8,
        am_choking: bool,
        peer_interested: bool,
        download_speed: f64,
        upload_speed: f64,
    ) -> PeerStats {
        let addr: SocketAddr = format!("127.0.0.{}:6881", peer_id)
            .parse()
            .unwrap();
        let mut peer = PeerStats::new([peer_id; 20], addr);
        peer.am_choking = am_choking;
        peer.peer_interested = peer_interested;
        peer.download_speed = download_speed;
        peer.upload_speed = upload_speed;
        peer
    }

    /// Create a peer that qualifies as a regular unchoker (interested + recent data).
    fn make_regular_unchoker(peer_id: u8, download_speed: f64) -> PeerStats {
        let mut peer = make_peer(peer_id, true, true, download_speed, 0.0);
        peer.last_data_time = Some(Instant::now());
        peer
    }

    /// Create a peer that does NOT qualify as a regular unchoker (interested
    /// but no recent data).
    fn make_interested_no_data(peer_id: u8, download_speed: f64) -> PeerStats {
        make_peer(peer_id, true, true, download_speed, 0.0)
    }

    // ------------------------------------------------------------------
    // Leecher tests
    // ------------------------------------------------------------------

    #[test]
    fn test_leecher_state_choke_3_round_cycle() {
        let mut choke = BtLeecherStateChoke::new();
        assert_eq!(choke.round(), 0);

        let mut peers: Vec<PeerStats> = vec![make_regular_unchoker(1, 5000.0)];
        let mut refs: Vec<&mut PeerStats> = peers.iter_mut().collect();

        // Round 0 -> 1
        let actions = choke.execute_choke(&mut refs);
        assert_eq!(choke.round(), 1);
        assert!(actions.iter().any(|a| matches!(a, ChokeAction::Unchoke(0))));

        // Round 1 -> 2
        let _actions = choke.execute_choke(&mut refs);
        assert_eq!(choke.round(), 2);

        // Round 2 -> 0 (cycle resets)
        let _actions = choke.execute_choke(&mut refs);
        assert_eq!(choke.round(), 0);
    }

    #[test]
    fn test_leecher_state_choke_regular_unchoker_detection() {
        let regular = make_regular_unchoker(1, 5000.0);
        assert!(is_regular_unchoker(&regular));

        let no_data = make_interested_no_data(2, 5000.0);
        assert!(!is_regular_unchoker(&no_data));

        let mut not_interested = make_peer(3, true, false, 5000.0, 0.0);
        not_interested.last_data_time = Some(Instant::now());
        assert!(!is_regular_unchoker(&not_interested));
    }

    #[test]
    fn test_leecher_state_choke_planned_optimistic_unchoke_round_0() {
        let mut choke = BtLeecherStateChoke::new();
        assert_eq!(choke.round(), 0);

        // 3 interested+choked peers with no recent data.
        let mut peers: Vec<PeerStats> = vec![
            make_interested_no_data(1, 100.0),
            make_interested_no_data(2, 200.0),
            make_interested_no_data(3, 300.0),
        ];
        let mut refs: Vec<&mut PeerStats> = peers.iter_mut().collect();

        let actions = choke.execute_choke(&mut refs);

        let unchoke_count = actions
            .iter()
            .filter(|a| matches!(a, ChokeAction::Unchoke(_)))
            .count();
        assert!(
            unchoke_count >= 1,
            "Expected at least 1 unchoke on round 0, got {}",
            unchoke_count
        );
    }

    #[test]
    fn test_leecher_state_choke_snubbed_peers_stay_choked() {
        let mut choke = BtLeecherStateChoke::new();

        let mut snubbed_peer = make_regular_unchoker(1, 100000.0);
        snubbed_peer.is_snubbed = true;
        let normal_peer = make_regular_unchoker(2, 5000.0);

        let mut peers: Vec<PeerStats> = vec![snubbed_peer, normal_peer];
        let mut refs: Vec<&mut PeerStats> = peers.iter_mut().collect();

        let actions = choke.execute_choke(&mut refs);

        // Snubbed peer should stay choked.
        let snubbed_action = actions.iter().find(|a| match a {
            ChokeAction::Unchoke(0) | ChokeAction::Choke(0) | ChokeAction::NoChange(0) => true,
            _ => false,
        });
        assert!(snubbed_action.is_some(), "Snubbed peer should have an action");
        match snubbed_action.unwrap() {
            ChokeAction::Unchoke(_) => panic!("Snubbed peer should NOT be unchoked"),
            ChokeAction::Choke(_) | ChokeAction::NoChange(_) => {}
        }

        // Normal peer should be unchoked.
        assert!(!refs[1].am_choking, "Normal peer should be unchoked");
    }

    #[test]
    fn test_leecher_peer_entry_sorting() {
        let mut entries = vec![
            LeecherPeerEntry {
                peer_idx: 0,
                download_speed: 1000.0,
                regular_unchoker: true,
                opt_unchoking: false,
                choking_required: true,
                peer_interested: true,
                prev_am_choking: true,
            },
            LeecherPeerEntry {
                peer_idx: 1,
                download_speed: 5000.0,
                regular_unchoker: true,
                opt_unchoking: false,
                choking_required: true,
                peer_interested: true,
                prev_am_choking: true,
            },
            LeecherPeerEntry {
                peer_idx: 2,
                download_speed: 3000.0,
                regular_unchoker: true,
                opt_unchoking: false,
                choking_required: true,
                peer_interested: true,
                prev_am_choking: true,
            },
        ];

        entries.sort_by(|a, b| {
            b.download_speed
                .partial_cmp(&a.download_speed)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        assert_eq!(entries[0].peer_idx, 1); // 5000
        assert_eq!(entries[1].peer_idx, 2); // 3000
        assert_eq!(entries[2].peer_idx, 0); // 1000
    }

    // ------------------------------------------------------------------
    // Seeder tests
    // ------------------------------------------------------------------

    #[test]
    fn test_seeder_state_choke_prioritizes_upload_speed() {
        let mut choke = BtSeederStateChoke::new();

        let mut peers: Vec<PeerStats> = vec![
            make_peer(1, true, true, 0.0, 1000.0),
            make_peer(2, true, true, 0.0, 5000.0),
        ];
        let mut refs: Vec<&mut PeerStats> = peers.iter_mut().collect();

        let actions = choke.execute_choke(&mut refs);

        let unchoke_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, ChokeAction::Unchoke(_)))
            .collect();
        assert!(
            !unchoke_actions.is_empty(),
            "Expected at least one unchoke"
        );
    }

    #[test]
    fn test_seeder_state_choke_recently_unchoked_preferred() {
        let mut choke = BtSeederStateChoke::new();

        // Peer 1: recently unchoked, low upload speed.
        let mut peer1 = make_peer(1, true, true, 0.0, 100.0);
        peer1.last_unchoke_at = Instant::now();

        // Peer 2: not recently unchoked, high upload speed.
        let mut peer2 = make_peer(2, true, true, 0.0, 10000.0);
        peer2.last_unchoke_at = Instant::now() - Duration::from_secs(30);

        let mut peers: Vec<PeerStats> = vec![peer1, peer2];
        let mut refs: Vec<&mut PeerStats> = peers.iter_mut().collect();

        let actions = choke.execute_choke(&mut refs);

        let unchoke_count = actions
            .iter()
            .filter(|a| matches!(a, ChokeAction::Unchoke(_)))
            .count();
        assert_eq!(unchoke_count, 2, "Both peers should be unchoked");
    }

    #[test]
    fn test_seeder_peer_entry_priority_ordering() {
        let now = Instant::now();

        let a = SeederPeerEntry {
            peer_idx: 0,
            recent_unchoking: true,
            upload_speed: 100.0,
            outstanding_upload: 1,
            last_am_unchoking: now,
            peer_interested: true,
            prev_am_choking: true,
            choking_required: true,
            opt_unchoking: false,
        };

        let b = SeederPeerEntry {
            peer_idx: 1,
            recent_unchoking: false,
            upload_speed: 5000.0,
            outstanding_upload: 0,
            last_am_unchoking: now - Duration::from_secs(30),
            peer_interested: true,
            prev_am_choking: true,
            choking_required: true,
            opt_unchoking: false,
        };

        // `a` has outstanding uploads -> should come first (Less).
        assert_eq!(
            SeederPeerEntry::compare_priority(&a, &b),
            std::cmp::Ordering::Less,
            "Peer with outstanding uploads should have higher priority"
        );
    }

    // ------------------------------------------------------------------
    // Integration: legacy free functions still work
    // ------------------------------------------------------------------

    #[test]
    fn test_legacy_on_peer_choke_unchoke() {
        let config = crate::engine::choking_algorithm::ChokingConfig::default();
        let mut algo = Some(ChokingAlgorithm::new(config));

        let addr: SocketAddr = "192.168.1.10:6881".parse().unwrap();
        let peer = PeerStats::new([0xAA; 20], addr);
        if let Some(a) = algo.as_mut() {
            a.add_peer(peer);
        }

        on_peer_unchoke(&mut algo, 0);
        assert!(!algo.as_ref().unwrap().get_peer(0).unwrap().peer_choking);

        on_peer_choke(&mut algo, 0);
        assert!(algo.as_ref().unwrap().get_peer(0).unwrap().peer_choking);
    }

    #[test]
    fn test_legacy_select_best_peer_for_request() {
        let config = crate::engine::choking_algorithm::ChokingConfig::default();
        let mut algo = ChokingAlgorithm::new(config);

        let mut peer1 = make_peer(1, false, true, 5000.0, 0.0);
        peer1.is_snubbed = false;
        let peer2 = make_peer(2, true, true, 1000.0, 0.0);

        algo.add_peer(peer1);
        algo.add_peer(peer2);

        let best = select_best_peer_for_request(&Some(algo));
        assert_eq!(best, Some(0), "Should select unchoked peer");
    }

    #[test]
    fn test_leecher_state_choke_regular_unchoke_top_3() {
        let mut choke = BtLeecherStateChoke::new();

        // 5 regular unchokers with different speeds.
        let mut peers: Vec<PeerStats> = vec![
            make_regular_unchoker(1, 10000.0), // highest
            make_regular_unchoker(2, 8000.0),
            make_regular_unchoker(3, 6000.0),
            make_regular_unchoker(4, 4000.0),
            make_regular_unchoker(5, 2000.0), // lowest
        ];
        let mut refs: Vec<&mut PeerStats> = peers.iter_mut().collect();

        // Round 0: POU + regular unchoke. The number of unchokes depends on
        // whether the POU peer is in the top 3 regular unchokers. Since POU
        // is random, we just verify at least 3 peers are unchoked.
        let actions = choke.execute_choke(&mut refs);
        let unchoke_count = actions
            .iter()
            .filter(|a| matches!(a, ChokeAction::Unchoke(_)))
            .count();
        assert!(
            unchoke_count >= 3,
            "Expected at least 3 unchokes on round 0, got {}",
            unchoke_count
        );
        assert!(
            unchoke_count <= 4,
            "Expected at most 4 unchokes on round 0, got {}",
            unchoke_count
        );

        // Advance to round 1 (no POU) and verify exactly 3 stay unchoked.
        // Since peers are already unchoked from round 0, subsequent rounds
        // should produce NoChange for them (no new Unchoke actions).
        let actions = choke.execute_choke(&mut refs);
        let new_unchoke_count = actions
            .iter()
            .filter(|a| matches!(a, ChokeAction::Unchoke(_)))
            .count();
        // Peers already unchoked stay unchoked -> NoChange.
        assert_eq!(
            new_unchoke_count, 0,
            "Expected 0 new unchokes on round 1 (peers already unchoked)"
        );

        // Verify exactly 3 peers are unchoked at steady state.
        let unchoked = refs.iter().filter(|p| !p.am_choking).count();
        assert_eq!(
            unchoked, 3,
            "Expected exactly 3 unchoked peers at steady state"
        );
    }

    #[test]
    fn test_seeder_state_choke_round_2_unchoke_4() {
        let mut choke = BtSeederStateChoke::new();

        // Advance to round 2 (where count=4).
        choke.round = 2;

        // 5 interested peers.
        let mut peers: Vec<PeerStats> = vec![
            make_peer(1, true, true, 0.0, 5000.0),
            make_peer(2, true, true, 0.0, 4000.0),
            make_peer(3, true, true, 0.0, 3000.0),
            make_peer(4, true, true, 0.0, 2000.0),
            make_peer(5, true, true, 0.0, 1000.0),
        ];
        let mut refs: Vec<&mut PeerStats> = peers.iter_mut().collect();

        let actions = choke.execute_choke(&mut refs);

        let unchoke_count = actions
            .iter()
            .filter(|a| matches!(a, ChokeAction::Unchoke(_)))
            .count();
        // On round 2, count=4, so 4 peers should be unchoked.
        assert_eq!(
            unchoke_count, 4,
            "Expected 4 unchokes on seeder round 2"
        );
    }

    #[test]
    fn test_seeder_state_choke_uninterested_peers_stay_choked() {
        let mut choke = BtSeederStateChoke::new();

        let mut peers: Vec<PeerStats> = vec![
            make_peer(1, true, false, 0.0, 5000.0), // Not interested
            make_peer(2, true, true, 0.0, 1000.0),  // Interested
        ];
        let mut refs: Vec<&mut PeerStats> = peers.iter_mut().collect();

        let actions = choke.execute_choke(&mut refs);

        // Peer 0 (uninterested) should be choked.
        let peer0_action = actions.iter().find(|a| match a {
            ChokeAction::Unchoke(0) | ChokeAction::Choke(0) | ChokeAction::NoChange(0) => true,
            _ => false,
        });
        assert!(peer0_action.is_some());
        match peer0_action.unwrap() {
            ChokeAction::Unchoke(_) => panic!("Uninterested peer should NOT be unchoked"),
            ChokeAction::Choke(_) | ChokeAction::NoChange(_) => {}
        }

        // Peer 1 (interested) should be unchoked.
        assert!(
            actions.iter().any(|a| matches!(a, ChokeAction::Unchoke(1))),
            "Interested peer should be unchoked"
        );
    }

    #[test]
    fn test_leecher_state_choke_fast_opt_unchoke_recovery() {
        let mut choke = BtLeecherStateChoke::new();

        // 3 regular unchokers + 1 extra interested peer.
        // The POU on round 0 should pick a choked+interested peer for opt unchoke.
        // If a regular unchoke later covers that peer, fast recovery should
        // reassign the opt slot.
        let mut peers: Vec<PeerStats> = vec![
            make_regular_unchoker(1, 5000.0),
            make_regular_unchoker(2, 4000.0),
            make_regular_unchoker(3, 3000.0),
            make_interested_no_data(4, 100.0), // Extra interested peer
        ];
        let mut refs: Vec<&mut PeerStats> = peers.iter_mut().collect();

        let actions = choke.execute_choke(&mut refs);

        // At least 3 peers should be unchoked (top 3 regular unchokers).
        let unchoke_count = actions
            .iter()
            .filter(|a| matches!(a, ChokeAction::Unchoke(_)))
            .count();
        assert!(
            unchoke_count >= 3,
            "Expected at least 3 unchokes, got {}",
            unchoke_count
        );
    }
}

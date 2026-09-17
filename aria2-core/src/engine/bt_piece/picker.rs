//! Piece picker module
//!
//! Implements piece selection strategies for BitTorrent downloads.
//! Based on BEP 0019 (WebSeed) and various piece picking algorithms.

use crate::segment::Bitfield;

mod selection;
#[cfg(test)]
mod tests;
mod types;

pub use types::{
    PickedPiece, PieceInfo, PiecePickStrategy, PiecePickerConfig, PiecePriorityMode,
    PieceSelectionStrategy,
};

/// Default remaining-piece count at or below which end-game tracking
/// activates. Mirrors aria2's `BT_ENDGAME_THRESHOLD`.
pub const DEFAULT_ENDGAME_THRESHOLD: usize = 20;

/// Piece picker — selects the next piece to download based on the
/// configured strategy, peer bitfield frequency data, and priority mode.
///
/// Maintains internal state for end-game candidate tracking and
/// per-piece frequency counters used by the rarest-first algorithm.
///
/// # Complexity
///
/// Sequential orders (`Forward` / `Backward`) and unrestricted rarest-first
/// picks are amortised **O(1)** thanks to monotone cursors: every piece before
/// the cursor is known to be completed or unavailable. The rarest order is
/// rebuilt in O(n log n) when frequencies change. Peer-restricted selection
/// remains O(n), because a different peer bitfield can make any earlier piece
/// unavailable.
pub struct PiecePicker {
    /// Total number of pieces in the torrent
    num_pieces: u32,
    /// Active selection strategy
    strategy: PieceSelectionStrategy,
    /// Active priority mode
    priority_mode: PiecePriorityMode,
    /// Per-piece availability frequency (from peer bitfields)
    frequencies: Vec<u32>,
    /// Piece indexes sorted by availability, then by index for stable ties.
    rarest_order: Vec<u32>,
    /// Cursor into `rarest_order` for unrestricted normal-mode selection.
    rarest_cursor: usize,
    /// Per-piece completion tracking (true = piece verified and written)
    completed: Bitfield,
    /// Per-piece selection filter (true = piece is requested by this task).
    /// Every selection strategy consults this same fact.
    allowed: Bitfield,
    /// Per-piece in-progress tracking (true = piece is being downloaded)
    in_progress: Bitfield,
    /// Per-piece priority (0 = default, higher = more important)
    priorities: Vec<u8>,
    /// Explicitly prioritized pieces, in the order in which they are tried.
    /// This models aria2's `PriorityPieceSelector` wrapper.
    priority_pieces: Vec<u32>,
    /// Indices of pieces that are candidates for end-game mode
    endgame_candidates: Vec<usize>,
    /// Number of selected pieces marked completed.
    completed_allowed_count: usize,
    /// Number of pieces selected by the current filter.
    allowed_count: usize,
    /// Forward scan cursor: every piece with index `< head_cursor` is
    /// completed or in progress. Only ever moves forward (or is reset
    /// backwards by [`PiecePicker::reopen`]).
    head_cursor: usize,
    /// Backward scan cursor stored as `index + 1`, so `0` means "exhausted".
    tail_cursor: usize,
    /// Remaining-piece count at or below which end-game candidates are tracked
    endgame_threshold: usize,
    /// xorshift64* state for the `Random` / `Geometric` orders
    rng_state: u64,
}

impl PiecePicker {
    /// Create a new picker for a torrent with `num_pieces` pieces.
    pub fn new(num_pieces: u32) -> Self {
        let n = num_pieces as usize;
        Self {
            num_pieces,
            strategy: PieceSelectionStrategy::RarestFirst,
            priority_mode: PiecePriorityMode::RarestFirst,
            frequencies: vec![0; n],
            rarest_order: (0..num_pieces).collect(),
            rarest_cursor: 0,
            completed: Bitfield::new(n),
            allowed: Bitfield::all_set(n),
            in_progress: Bitfield::new(n),
            priorities: vec![0; n],
            priority_pieces: Vec::new(),
            endgame_candidates: Vec::new(),
            completed_allowed_count: 0,
            allowed_count: n,
            head_cursor: 0,
            tail_cursor: n,
            endgame_threshold: DEFAULT_ENDGAME_THRESHOLD,
            rng_state: Self::seed(),
        }
    }

    /// Derive a non-zero RNG seed from the standard library's randomised
    /// hasher, avoiding an external `rand` dependency in the protocol crate.
    fn seed() -> u64 {
        use std::hash::{BuildHasher, Hasher};
        let s = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        s | 1
    }

    /// Advance the xorshift64* generator and return the next value.
    fn next_rand(&mut self) -> u64 {
        let mut x = self.rng_state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng_state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Set the base selection strategy.
    pub fn set_strategy(&mut self, strategy: PieceSelectionStrategy) {
        self.strategy = strategy;
    }

    /// Set the piece priority mode.
    pub fn set_priority_mode(&mut self, mode: PiecePriorityMode) {
        self.priority_mode = mode;
    }

    /// Install a prioritized piece sequence used before the normal selector.
    pub fn set_priority_pieces(&mut self, mut pieces: Vec<u32>) {
        pieces.retain(|&piece| piece < self.num_pieces);
        pieces.sort_unstable();
        pieces.dedup();
        for index in (1..pieces.len()).rev() {
            let swap = (self.next_rand() % (index as u64 + 1)) as usize;
            pieces.swap(index, swap);
        }
        self.priority_pieces = pieces;
    }

    /// Return the currently installed explicit priority sequence.
    pub fn priority_pieces(&self) -> &[u32] {
        &self.priority_pieces
    }

    /// Restrict selection and selective completion to the given piece
    /// indexes.
    ///
    /// The completion bitfield remains global: unselected pieces that were
    /// already present can still be persisted and reported, but they do not
    /// contribute to selective-download completion or future selection.
    pub fn set_allowed_pieces(&mut self, pieces: &[u32]) {
        self.allowed.clear_all();
        for &piece in pieces {
            self.allowed.set(piece as usize);
        }
        self.allowed_count = (0..self.num_pieces as usize)
            .filter(|&i| self.allowed.test(i))
            .count();
        self.completed_allowed_count = (0..self.num_pieces as usize)
            .filter(|&i| self.completed.test(i) && self.allowed.test(i))
            .count();
        self.head_cursor = 0;
        self.tail_cursor = self.num_pieces as usize;
        self.rarest_cursor = 0;
        self.refresh_endgame_candidates();
    }

    /// Whether a piece belongs to the current selective-download set.
    pub fn is_allowed(&self, index: u32) -> bool {
        self.allowed.test(index as usize)
    }

    /// Number of pieces selected by the current filter.
    pub fn allowed_count(&self) -> usize {
        self.allowed_count
    }

    /// Override the remaining-piece count at which end-game mode activates.
    pub fn set_endgame_threshold(&mut self, threshold: usize) {
        self.endgame_threshold = threshold;
        self.refresh_endgame_candidates();
    }

    /// Set the explicit priority of a piece (used by the `Priority` strategy).
    pub fn set_priority(&mut self, index: u32, priority: u8) {
        let i = index as usize;
        if i < self.num_pieces as usize {
            self.priorities[i] = priority;
        }
    }

    /// Pick the next piece index using the configured strategy.
    ///
    /// `bitfield` is the peer's have-bitfield (MSB-first), `nbits` is the
    /// number of valid bits (typically `num_pieces`). Returns `None` when the
    /// peer has nothing we still need.
    pub fn select(&mut self, bitfield: &[u8], nbits: usize) -> Option<u32> {
        self.pick_internal(Some(bitfield), nbits, false)
    }

    /// Pick the next piece without a peer restriction.
    ///
    /// In end-game mode (remaining pieces at or below the threshold) pieces
    /// already in flight become eligible again, so they can be requested from
    /// several peers at once.
    pub fn pick_next(&mut self) -> Option<u32> {
        let allow_in_progress = self.endgame_active();
        let n = self.num_pieces as usize;
        self.pick_internal(None, n, allow_in_progress)
    }

    /// Pick the next piece without peer restrictions or end-game duplicates.
    ///
    /// The BT scheduler uses this when its own end-game threshold says normal
    /// selection is still active. Keeping the flag explicit avoids coupling
    /// the protocol picker to a caller-owned threshold.
    pub fn pick_next_without_endgame(&mut self) -> Option<u32> {
        let n = self.num_pieces as usize;
        self.pick_internal(None, n, false)
    }

    /// Whether end-game mode is currently active.
    pub fn endgame_active(&self) -> bool {
        let remaining = self.remaining_count();
        remaining > 0 && remaining <= self.endgame_threshold
    }

    /// Recompute the end-game candidate list (incomplete pieces).
    ///
    /// O(n), but only runs once end-game mode is active, i.e. at most
    /// `endgame_threshold` times per download.
    fn refresh_endgame_candidates(&mut self) {
        self.endgame_candidates.clear();
        if !self.endgame_active() {
            return;
        }
        for i in 0..self.num_pieces as usize {
            if self.allowed.test(i) && !self.completed.test(i) {
                self.endgame_candidates.push(i);
            }
        }
    }

    /// Return the list of piece indices that are end-game candidates.
    pub fn endgame_candidates(&self) -> &[usize] {
        &self.endgame_candidates
    }

    /// Update per-piece frequency data from a peer frequency slice.
    pub fn set_frequencies_from_peers(&mut self, freqs: &[usize]) {
        self.frequencies.fill(0);
        let len = freqs.len().min(self.frequencies.len());
        for (dst, src) in self.frequencies.iter_mut().zip(freqs.iter()).take(len) {
            *dst = *src as u32;
        }
        self.rarest_order.sort_unstable_by_key(|&index| {
            let index = index as usize;
            (self.frequencies[index], index)
        });
        self.rarest_cursor = 0;
    }

    /// Iterator over all pieces, yielding [`PieceInfo`] for each.
    pub fn pieces_iter(&self) -> impl Iterator<Item = PieceInfo> + '_ {
        let n = self.num_pieces as usize;
        (0..n).map(move |i| PieceInfo {
            index: i as u32,
            frequency: self.frequencies[i],
            is_completed: self.completed.test(i),
            completed: self.completed.test(i),
            in_progress: self.in_progress.test(i),
            priority: self.priorities[i],
        })
    }

    /// Return info about a specific piece, or `None` if out of range.
    pub fn get_piece_info(&self, index: u32) -> Option<PieceInfo> {
        let i = index as usize;
        if i >= self.num_pieces as usize {
            return None;
        }
        Some(PieceInfo {
            index,
            frequency: self.frequencies[i],
            is_completed: self.completed.test(i),
            completed: self.completed.test(i),
            in_progress: self.in_progress.test(i),
            priority: self.priorities[i],
        })
    }

    /// Return the current priority mode.
    pub fn priority_mode(&self) -> PiecePriorityMode {
        self.priority_mode
    }

    /// Number of pieces not yet completed. O(1).
    pub fn remaining_count(&self) -> usize {
        self.allowed_count()
            .saturating_sub(self.completed_allowed_count)
    }

    /// Mark a piece as completed.
    ///
    /// Idempotent: marking an already-completed piece is a no-op. Also clears
    /// the in-progress flag, since a completed piece is no longer in flight.
    ///
    /// # Panics
    /// Panics if `index` is out of range in debug builds.
    pub fn mark_completed(&mut self, index: u32) {
        let i = index as usize;
        debug_assert!(
            i < self.num_pieces as usize,
            "mark_completed: index out of range"
        );
        if i >= self.num_pieces as usize {
            return;
        }
        if !self.completed.test(i) {
            self.completed.set(i);
            if self.allowed.test(i) {
                self.completed_allowed_count += 1;
            }
        }
        self.in_progress.clear(i);
        self.refresh_endgame_candidates();
    }

    /// Mark a piece as being downloaded (or release it back to the pool).
    ///
    /// Releasing a piece (`in_progress = false`) rewinds the sequential
    /// cursors so the piece is picked up again by later scans.
    ///
    /// # Panics
    /// Panics if `index` is out of range in debug builds.
    pub fn mark_in_progress(&mut self, index: u32, in_progress: bool) {
        let i = index as usize;
        debug_assert!(
            i < self.num_pieces as usize,
            "mark_in_progress: index out of range"
        );
        if i >= self.num_pieces as usize {
            return;
        }
        if in_progress {
            self.in_progress.set(i);
        } else {
            self.in_progress.clear(i);
        }
        if !in_progress {
            self.reopen(i);
        }
    }

    /// Whether a piece is currently being downloaded.
    pub fn is_in_progress(&self, index: u32) -> bool {
        let i = index as usize;
        i < self.num_pieces as usize && self.in_progress.test(i)
    }

    /// Whether a piece has been completed and verified.
    pub fn is_completed(&self, index: u32) -> bool {
        let i = index as usize;
        i < self.num_pieces as usize && self.completed.test(i)
    }

    /// Export completed pieces as a bitfield byte vector (MSB-first).
    pub fn export_bitfield(&self) -> Vec<u8> {
        self.completed.as_bytes().to_vec()
    }

    /// Check if all pieces are completed. O(1).
    pub fn is_complete(&self) -> bool {
        self.completed_allowed_count == self.allowed_count()
    }
}

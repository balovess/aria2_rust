//! Piece picker module
//!
//! Implements piece selection strategies for BitTorrent downloads.
//! Based on BEP 0019 (WebSeed) and various piece picking algorithms.

/// Piece selection strategy — determines the algorithm used to pick the next
/// piece to request from peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceSelectionStrategy {
    /// Select pieces sequentially (good for streaming)
    Sequential,
    /// Select rarest pieces first (default BitTorrent strategy)
    RarestFirst,
    /// Select pieces randomly
    Random,
    /// Select pieces to form the longest contiguous sequence
    LongestSequence,
    /// Priority-based selection (higher priority pieces first)
    Priority,
    /// Geometric distribution (prefer earlier pieces)
    Geometric,
}

/// Piece priority mode — controls how pieces are prioritised within the
/// picker, independent of the base selection strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiecePriorityMode {
    /// Prioritise pieces from the beginning of the file (head)
    SequentialHead,
    /// Prioritise pieces from the end of the file (tail)
    SequentialTail,
    /// Default rarest-first priority (no special head/tail bias)
    RarestFirst,
}

/// Legacy alias kept for backward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiecePickStrategy {
    /// Select pieces sequentially (good for streaming)
    Sequential,
    /// Select rarest pieces first (default BitTorrent strategy)
    RarestFirst,
    /// Select pieces randomly
    Random,
    /// Select pieces to form the longest contiguous sequence
    LongestSequence,
    /// Priority-based selection (higher priority pieces first)
    Priority,
    /// Geometric distribution (prefer earlier pieces)
    Geometric,
}

/// Information about a single piece within the picker.
///
/// Returned by [`PiecePicker::get_piece_info`].
#[derive(Debug, Clone)]
pub struct PieceInfo {
    /// Zero-based piece index
    pub index: u32,
    /// Number of peers that have this piece (availability frequency)
    pub frequency: u32,
    /// Whether this piece has been fully downloaded and verified
    pub is_completed: bool,
    /// Whether this piece has been fully downloaded and verified (alias)
    pub completed: bool,
    /// Whether this piece is currently being downloaded
    pub in_progress: bool,
    /// Priority level (0 = default, higher = more important)
    pub priority: u8,
}

/// Default remaining-piece count at or below which end-game tracking
/// activates. Mirrors aria2's `BT_ENDGAME_THRESHOLD`.
pub const DEFAULT_ENDGAME_THRESHOLD: usize = 20;

/// Scan order resolved from the (`strategy`, `priority_mode`) pair.
///
/// The legacy `priority_mode` API can still request a global head/tail scan;
/// the aria2-compatible file-boundary option is handled separately by
/// `set_priority_pieces` before this order is consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanOrder {
    /// Lowest index first (streaming / sequential writes)
    Forward,
    /// Highest index first
    Backward,
    /// Lowest availability frequency first, ties broken by lowest index
    Rarest,
    /// Uniform random among eligible pieces
    Random,
    /// Start of the longest contiguous run of eligible pieces
    LongestRun,
    /// Highest explicit priority first, ties broken by lowest index
    Priority,
    /// Geometric bias towards earlier eligible pieces
    Geometric,
}

/// Piece picker — selects the next piece to download based on the
/// configured strategy, peer bitfield frequency data, and priority mode.
///
/// Maintains internal state for end-game candidate tracking and
/// per-piece frequency counters used by the rarest-first algorithm.
///
/// # Complexity
///
/// Sequential orders (`Forward` / `Backward`) are amortised **O(1)** thanks to
/// monotone cursors: every piece below `head_cursor` (resp. above
/// `tail_cursor`) is known to be completed or already in flight, so repeated
/// picks never rescan the prefix. The remaining orders are O(n) per pick,
/// which matches aria2's C++ `RarestPieceSelector` behaviour.
pub struct PiecePicker {
    /// Total number of pieces in the torrent
    num_pieces: u32,
    /// Active selection strategy
    strategy: PieceSelectionStrategy,
    /// Active priority mode
    priority_mode: PiecePriorityMode,
    /// Per-piece availability frequency (from peer bitfields)
    frequencies: Vec<u32>,
    /// Per-piece completion tracking (true = piece verified and written)
    completed: Vec<bool>,
    /// Per-piece selection filter (true = piece is requested by this task).
    /// Every selection strategy consults this same fact.
    allowed: Vec<bool>,
    /// Per-piece in-progress tracking (true = piece is being downloaded)
    in_progress: Vec<bool>,
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
            completed: vec![false; n],
            allowed: vec![true; n],
            in_progress: vec![false; n],
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
        self.allowed.fill(false);
        for &piece in pieces {
            if let Some(allowed) = self.allowed.get_mut(piece as usize) {
                *allowed = true;
            }
        }
        self.allowed_count = self.allowed.iter().filter(|allowed| **allowed).count();
        self.completed_allowed_count = self
            .completed
            .iter()
            .zip(&self.allowed)
            .filter(|(completed, allowed)| **completed && **allowed)
            .count();
        self.head_cursor = 0;
        self.tail_cursor = self.num_pieces as usize;
        self.refresh_endgame_candidates();
    }

    /// Whether a piece belongs to the current selective-download set.
    pub fn is_allowed(&self, index: u32) -> bool {
        self.allowed.get(index as usize).copied().unwrap_or(false)
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

    /// Resolve the effective scan order from strategy + priority mode.
    fn scan_order(&self) -> ScanOrder {
        match self.priority_mode {
            PiecePriorityMode::SequentialHead => ScanOrder::Forward,
            PiecePriorityMode::SequentialTail => ScanOrder::Backward,
            PiecePriorityMode::RarestFirst => match self.strategy {
                PieceSelectionStrategy::Sequential => ScanOrder::Forward,
                PieceSelectionStrategy::RarestFirst => ScanOrder::Rarest,
                PieceSelectionStrategy::Random => ScanOrder::Random,
                PieceSelectionStrategy::LongestSequence => ScanOrder::LongestRun,
                PieceSelectionStrategy::Priority => ScanOrder::Priority,
                PieceSelectionStrategy::Geometric => ScanOrder::Geometric,
            },
        }
    }

    /// A piece is *available* when it is neither completed nor already
    /// being downloaded by another request.
    #[inline]
    fn is_available(&self, i: usize) -> bool {
        self.allowed[i] && !self.completed[i] && !self.in_progress[i]
    }

    /// Test bit `i` of an MSB-first bitfield. `None` means "peer has everything".
    #[inline]
    fn peer_has(bitfield: Option<&[u8]>, i: usize) -> bool {
        match bitfield {
            None => true,
            Some(bf) => {
                let byte = i / 8;
                let bit = 7 - (i % 8);
                byte < bf.len() && (bf[byte] & (1 << bit)) != 0
            }
        }
    }

    /// Move `head_cursor` up to the first globally available piece.
    fn advance_head(&mut self) {
        let n = self.num_pieces as usize;
        while self.head_cursor < n {
            let i = self.head_cursor;
            if self.is_available(i) {
                break;
            }
            self.head_cursor = i + 1;
        }
    }

    /// Move `tail_cursor` down to the last globally available piece.
    fn advance_tail(&mut self) {
        while self.tail_cursor > 0 {
            let i = self.tail_cursor - 1;
            if self.is_available(i) {
                break;
            }
            self.tail_cursor = i;
        }
    }

    /// A piece became available again — pull the cursors back so it is
    /// not skipped by subsequent sequential scans.
    fn reopen(&mut self, i: usize) {
        if i < self.head_cursor {
            self.head_cursor = i;
        }
        if i + 1 > self.tail_cursor {
            self.tail_cursor = i + 1;
        }
    }

    /// Core selection routine shared by [`Self::select`] and [`Self::pick_next`].
    ///
    /// `bitfield` restricts the candidates to pieces the peer advertises;
    /// `allow_in_progress` is set in end-game mode, where duplicate requests
    /// for in-flight pieces are intentional.
    fn pick_internal(
        &mut self,
        bitfield: Option<&[u8]>,
        nbits: usize,
        allow_in_progress: bool,
    ) -> Option<u32> {
        let n = (self.num_pieces as usize).min(nbits);
        if n == 0 {
            return None;
        }
        // `PriorityPieceSelector` in aria2_original tries its explicit list
        // first, but still respects the peer bitfield and completion state.
        for &piece in &self.priority_pieces {
            let index = piece as usize;
            if index < n
                && self.allowed[index]
                && !self.completed[index]
                && (allow_in_progress || !self.in_progress[index])
                && Self::peer_has(bitfield, index)
            {
                return Some(piece);
            }
        }

        let order = self.scan_order();

        // Cursor fast paths — only valid when in-progress pieces are excluded,
        // because the cursor invariant counts them as unavailable.
        if !allow_in_progress {
            match order {
                ScanOrder::Forward => {
                    self.advance_head();
                    for i in self.head_cursor..n {
                        if self.is_available(i) && Self::peer_has(bitfield, i) {
                            return Some(i as u32);
                        }
                    }
                    return None;
                }
                ScanOrder::Backward => {
                    self.advance_tail();
                    let mut i = self.tail_cursor.min(n);
                    while i > 0 {
                        i -= 1;
                        if self.is_available(i) && Self::peer_has(bitfield, i) {
                            return Some(i as u32);
                        }
                    }
                    return None;
                }
                _ => {}
            }
        }

        let usable = |p: &Self, i: usize| -> bool {
            p.allowed[i]
                && !p.completed[i]
                && (allow_in_progress || !p.in_progress[i])
                && Self::peer_has(bitfield, i)
        };

        match order {
            ScanOrder::Forward => (0..n).find(|&i| usable(self, i)).map(|i| i as u32),
            ScanOrder::Backward => (0..n).rev().find(|&i| usable(self, i)).map(|i| i as u32),
            ScanOrder::Rarest => {
                let mut best: Option<(u32, usize)> = None;
                for i in 0..n {
                    if usable(self, i) {
                        let f = self.frequencies[i];
                        if best.is_none_or(|(bf, _)| f < bf) {
                            best = Some((f, i));
                        }
                    }
                }
                best.map(|(_, i)| i as u32)
            }
            ScanOrder::Priority => {
                let mut best: Option<(u8, usize)> = None;
                for i in 0..n {
                    if usable(self, i) {
                        let p = self.priorities[i];
                        if best.is_none_or(|(bp, _)| p > bp) {
                            best = Some((p, i));
                        }
                    }
                }
                best.map(|(_, i)| i as u32)
            }
            ScanOrder::LongestRun => {
                let (mut best_start, mut best_len) = (None, 0usize);
                let (mut cur_start, mut cur_len) = (None, 0usize);
                for i in 0..n {
                    if usable(self, i) {
                        if cur_start.is_none() {
                            cur_start = Some(i);
                            cur_len = 0;
                        }
                        cur_len += 1;
                        if cur_len > best_len {
                            best_len = cur_len;
                            best_start = cur_start;
                        }
                    } else {
                        cur_start = None;
                        cur_len = 0;
                    }
                }
                best_start.map(|i| i as u32)
            }
            ScanOrder::Random => {
                // Reservoir sampling: one pass, uniform over eligible pieces.
                let mut chosen: Option<usize> = None;
                let mut seen: u64 = 0;
                for i in 0..n {
                    if usable(self, i) {
                        seen += 1;
                        if self.next_rand().is_multiple_of(seen) {
                            chosen = Some(i);
                        }
                    }
                }
                chosen.map(|i| i as u32)
            }
            ScanOrder::Geometric => {
                let candidates = (0..n).filter(|&i| usable(self, i)).count();
                if candidates == 0 {
                    return None;
                }
                // P(k-th candidate) = 2^-(k+1): strong bias towards the head.
                let r = self.next_rand();
                let k = (r.trailing_zeros() as usize).min(candidates - 1);
                (0..n).filter(|&i| usable(self, i)).nth(k).map(|i| i as u32)
            }
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
            if self.allowed[i] && !self.completed[i] {
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
        let len = freqs.len().min(self.frequencies.len());
        for (dst, src) in self.frequencies.iter_mut().zip(freqs.iter()).take(len) {
            *dst = *src as u32;
        }
    }

    /// Iterator over all pieces, yielding [`PieceInfo`] for each.
    pub fn pieces_iter(&self) -> impl Iterator<Item = PieceInfo> + '_ {
        let n = self.num_pieces as usize;
        (0..n).map(move |i| PieceInfo {
            index: i as u32,
            frequency: self.frequencies[i],
            is_completed: self.completed[i],
            completed: self.completed[i],
            in_progress: self.in_progress[i],
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
            is_completed: self.completed[i],
            completed: self.completed[i],
            in_progress: self.in_progress[i],
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
        if !self.completed[i] {
            self.completed[i] = true;
            if self.allowed[i] {
                self.completed_allowed_count += 1;
            }
        }
        self.in_progress[i] = false;
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
        self.in_progress[i] = in_progress;
        if !in_progress {
            self.reopen(i);
        }
    }

    /// Whether a piece is currently being downloaded.
    pub fn is_in_progress(&self, index: u32) -> bool {
        let i = index as usize;
        i < self.num_pieces as usize && self.in_progress[i]
    }

    /// Whether a piece has been completed and verified.
    pub fn is_completed(&self, index: u32) -> bool {
        let i = index as usize;
        i < self.num_pieces as usize && self.completed[i]
    }

    /// Export completed pieces as a bitfield byte vector (MSB-first).
    pub fn export_bitfield(&self) -> Vec<u8> {
        let n = self.num_pieces as usize;
        let bf_len = n.div_ceil(8);
        let mut bitfield = vec![0u8; bf_len];
        for (i, &done) in self.completed.iter().enumerate() {
            if done {
                let byte_idx = i / 8;
                let bit_idx = 7 - (i % 8);
                bitfield[byte_idx] |= 1 << bit_idx;
            }
        }
        bitfield
    }

    /// Check if all pieces are completed. O(1).
    pub fn is_complete(&self) -> bool {
        self.completed_allowed_count == self.allowed_count()
    }
}

/// Piece picker configuration
#[derive(Debug, Clone)]
pub struct PiecePickerConfig {
    /// Selection strategy
    pub strategy: PiecePickStrategy,
    /// Number of pieces to request ahead
    pub request_queue_size: usize,
    /// Whether to prioritize end-game mode
    pub end_game_threshold: f64,
}

impl Default for PiecePickerConfig {
    fn default() -> Self {
        Self {
            strategy: PiecePickStrategy::RarestFirst,
            request_queue_size: 16,
            end_game_threshold: 0.95,
        }
    }
}

/// Result of a piece pick operation
#[derive(Debug, Clone)]
pub struct PickedPiece {
    /// Index of the picked piece
    pub index: usize,
    /// Priority of the piece (higher = more important)
    pub priority: u8,
    /// Whether this piece is in end-game mode
    pub is_end_game: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_piece_selection_strategy_variants() {
        assert_ne!(
            PieceSelectionStrategy::Sequential,
            PieceSelectionStrategy::RarestFirst
        );
        assert_ne!(
            PieceSelectionStrategy::Random,
            PieceSelectionStrategy::Geometric
        );
    }

    #[test]
    fn test_piece_priority_mode_variants() {
        assert_ne!(
            PiecePriorityMode::SequentialHead,
            PiecePriorityMode::SequentialTail
        );
        assert_ne!(
            PiecePriorityMode::SequentialTail,
            PiecePriorityMode::RarestFirst
        );
    }

    #[test]
    fn test_piece_picker_new() {
        let picker = PiecePicker::new(100);
        assert!(picker.endgame_candidates().is_empty());
        assert_eq!(picker.remaining_count(), 100);
        assert!(!picker.is_complete());
    }

    #[test]
    fn test_allowed_piece_filter_controls_selection_and_completion() {
        let mut picker = PiecePicker::new(4);
        picker.set_endgame_threshold(0);
        picker.set_allowed_pieces(&[1, 3]);

        assert_eq!(picker.allowed_count(), 2);
        assert_eq!(picker.remaining_count(), 2);
        assert!(!picker.is_allowed(0));
        assert!(picker.is_allowed(1));
        assert_eq!(picker.pick_next(), Some(1));

        picker.mark_completed(1);
        assert_eq!(picker.remaining_count(), 1);
        assert!(!picker.is_complete());
        assert_eq!(picker.pick_next(), Some(3));

        picker.mark_completed(3);
        assert!(picker.is_complete());
        assert_eq!(picker.remaining_count(), 0);
        assert_eq!(picker.pick_next(), None);
    }

    #[test]
    fn test_allowed_piece_filter_keeps_endgame_candidates_selective() {
        let mut picker = PiecePicker::new(5);
        picker.set_allowed_pieces(&[2, 4]);

        assert_eq!(picker.endgame_candidates(), &[2, 4]);
        picker.mark_completed(0);
        assert_eq!(picker.endgame_candidates(), &[2, 4]);
        picker.mark_completed(2);
        assert_eq!(picker.endgame_candidates(), &[4]);
    }

    #[test]
    fn test_piece_picker_new_u32() {
        let picker = PiecePicker::new(50u32);
        assert_eq!(picker.remaining_count(), 50);
    }

    #[test]
    fn test_piece_picker_set_strategy() {
        let mut picker = PiecePicker::new(100);
        picker.set_strategy(PieceSelectionStrategy::Sequential);
        // Verify it compiles and runs without panic
        let _ = picker.select(&[0xFF; 13], 100);
    }

    #[test]
    fn test_piece_picker_set_priority_mode() {
        let mut picker = PiecePicker::new(100);
        picker.set_priority_mode(PiecePriorityMode::SequentialHead);
        assert_eq!(picker.priority_mode(), PiecePriorityMode::SequentialHead);
    }

    #[test]
    fn test_explicit_priority_pieces_precede_base_selector() {
        let mut picker = PiecePicker::new(5);
        picker.set_strategy(PieceSelectionStrategy::RarestFirst);
        picker.set_frequencies_from_peers(&[0, 0, 0, 9, 0]);
        picker.set_priority_pieces(vec![3, 1, 3]);

        assert_eq!(picker.priority_pieces().len(), 2);
        assert!(picker.priority_pieces().contains(&1));
        assert!(picker.priority_pieces().contains(&3));

        let first = picker
            .pick_next()
            .expect("priority piece should be selected");
        assert!(first == 1 || first == 3);
        picker.mark_completed(first);
        let second = picker
            .pick_next()
            .expect("second priority piece should follow");
        assert!(second == 1 || second == 3);
        assert_ne!(first, second);
    }

    #[test]
    fn test_explicit_priority_pieces_respect_peer_bitfield() {
        let mut picker = PiecePicker::new(4);
        picker.set_priority_pieces(vec![2, 1]);

        // The peer has piece 1 but not piece 2 (MSB-first bitfield).
        assert_eq!(picker.select(&[0b0100_0000], 4), Some(1));
    }

    #[test]
    fn test_piece_pick_strategy_variants() {
        assert_ne!(
            PiecePickStrategy::Sequential,
            PiecePickStrategy::RarestFirst
        );
        assert_ne!(PiecePickStrategy::Random, PiecePickStrategy::Geometric);
    }

    #[test]
    fn test_piece_picker_config_default() {
        let config = PiecePickerConfig::default();
        assert_eq!(config.strategy, PiecePickStrategy::RarestFirst);
        assert_eq!(config.request_queue_size, 16);
        assert!(config.end_game_threshold > 0.9);
    }

    #[test]
    fn test_picked_piece() {
        let picked = PickedPiece {
            index: 42,
            priority: 5,
            is_end_game: false,
        };
        assert_eq!(picked.index, 42);
        assert_eq!(picked.priority, 5);
        assert!(!picked.is_end_game);
    }

    #[test]
    fn test_mark_completed_and_is_complete() {
        let mut picker = PiecePicker::new(3u32);
        assert!(!picker.is_complete());
        assert_eq!(picker.remaining_count(), 3);

        picker.mark_completed(0);
        assert!(!picker.is_complete());
        assert_eq!(picker.remaining_count(), 2);

        picker.mark_completed(1);
        picker.mark_completed(2);
        assert!(picker.is_complete());
        assert_eq!(picker.remaining_count(), 0);
    }

    #[test]
    fn test_export_bitfield() {
        let mut picker = PiecePicker::new(8u32);
        picker.mark_completed(0);
        picker.mark_completed(7);
        let bf = picker.export_bitfield();
        assert_eq!(bf.len(), 1);
        // Bit 0 (MSB) and bit 7 (LSB) set => 10000001 = 0x81
        assert_eq!(bf[0], 0x81);
    }

    #[test]
    fn test_get_piece_info() {
        let mut picker = PiecePicker::new(10u32);
        picker.mark_completed(3);
        picker.set_frequencies_from_peers(&[0, 2, 0, 0, 5, 0, 0, 0, 0, 0]);

        let info = picker.get_piece_info(3).unwrap();
        assert!(info.is_completed);
        assert_eq!(info.index, 3);
        assert_eq!(info.frequency, 0);

        let info4 = picker.get_piece_info(4).unwrap();
        assert!(!info4.is_completed);
        assert_eq!(info4.frequency, 5);

        assert!(picker.get_piece_info(100).is_none());
    }

    #[test]
    fn test_pieces_iter() {
        let picker = PiecePicker::new(5u32);
        let indices: Vec<u32> = picker.pieces_iter().map(|p| p.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_pieces_iter_empty() {
        let picker = PiecePicker::new(0u32);
        let indices: Vec<u32> = picker.pieces_iter().map(|p| p.index).collect();
        assert!(indices.is_empty());
    }

    #[test]
    fn test_export_bitfield_partial() {
        let mut picker = PiecePicker::new(16u32);
        // Set pieces 0, 3, 7
        picker.mark_completed(0);
        picker.mark_completed(3);
        picker.mark_completed(7);
        let bf = picker.export_bitfield();
        // MSB-first: piece0=bit7(0x80), piece3=bit4(0x10), piece7=bit0(0x01)
        // 0x80|0x10|0x01 = 0x91
        assert_eq!(bf.len(), 2);
        assert_eq!(bf[0], 0x91);
        assert_eq!(bf[1], 0x00);
    }

    #[test]
    fn test_is_complete_zero_pieces() {
        let picker = PiecePicker::new(0u32);
        // Zero pieces: vacuously true
        assert!(picker.is_complete());
        assert_eq!(picker.remaining_count(), 0);
    }

    #[test]
    fn test_set_frequencies_from_peers() {
        let mut picker = PiecePicker::new(4u32);
        let freqs: Vec<usize> = vec![3, 1, 5, 2];
        picker.set_frequencies_from_peers(&freqs);

        let info = picker.get_piece_info(2).unwrap();
        assert_eq!(info.frequency, 5);
    }

    // ── Selection behaviour ──────────────────────────────────────────────

    #[test]
    fn test_sequential_pick_advances_in_order() {
        let mut picker = PiecePicker::new(5u32);
        picker.set_strategy(PieceSelectionStrategy::Sequential);

        for expected in 0..5u32 {
            assert_eq!(picker.pick_next(), Some(expected));
            picker.mark_completed(expected);
        }
        assert_eq!(picker.pick_next(), None);
        assert!(picker.is_complete());
    }

    #[test]
    fn test_sequential_skips_in_progress() {
        let mut picker = PiecePicker::new(4u32);
        picker.set_strategy(PieceSelectionStrategy::Sequential);
        // Disable end-game, which would otherwise re-offer in-flight pieces
        // (a 4-piece torrent is below the default threshold).
        picker.set_endgame_threshold(0);

        picker.mark_in_progress(0, true);
        assert_eq!(picker.pick_next(), Some(1));

        // Releasing piece 0 rewinds the cursor.
        picker.mark_in_progress(0, false);
        assert_eq!(picker.pick_next(), Some(0));
    }

    #[test]
    fn test_tail_mode_picks_from_the_end() {
        let mut picker = PiecePicker::new(6u32);
        picker.set_priority_mode(PiecePriorityMode::SequentialTail);

        assert_eq!(picker.pick_next(), Some(5));
        picker.mark_completed(5);
        assert_eq!(picker.pick_next(), Some(4));
    }

    #[test]
    fn test_head_mode_picks_from_the_start() {
        let mut picker = PiecePicker::new(6u32);
        picker.set_priority_mode(PiecePriorityMode::SequentialHead);

        assert_eq!(picker.pick_next(), Some(0));
        picker.mark_completed(0);
        assert_eq!(picker.pick_next(), Some(1));
    }

    #[test]
    fn test_rarest_first_prefers_lowest_frequency() {
        let mut picker = PiecePicker::new(4u32);
        picker.set_strategy(PieceSelectionStrategy::RarestFirst);
        picker.set_frequencies_from_peers(&[9, 4, 1, 7]);

        assert_eq!(picker.pick_next(), Some(2));
        picker.mark_completed(2);
        assert_eq!(picker.pick_next(), Some(1));
    }

    #[test]
    fn test_priority_strategy_prefers_highest_priority() {
        let mut picker = PiecePicker::new(4u32);
        picker.set_strategy(PieceSelectionStrategy::Priority);
        picker.set_priority(3, 9);
        picker.set_priority(1, 5);

        assert_eq!(picker.pick_next(), Some(3));
        picker.mark_completed(3);
        assert_eq!(picker.pick_next(), Some(1));
    }

    #[test]
    fn test_longest_sequence_picks_run_start() {
        let mut picker = PiecePicker::new(8u32);
        picker.set_strategy(PieceSelectionStrategy::LongestSequence);
        // Leave 0 available (run length 1) and 3..=7 available (run length 5).
        picker.mark_completed(1);
        picker.mark_completed(2);

        assert_eq!(picker.pick_next(), Some(3));
    }

    #[test]
    fn test_random_strategy_returns_available_piece() {
        let mut picker = PiecePicker::new(16u32);
        picker.set_strategy(PieceSelectionStrategy::Random);
        for i in 0..15u32 {
            picker.mark_completed(i);
        }
        // Only piece 15 is left, so the random pick is deterministic.
        assert_eq!(picker.pick_next(), Some(15));
    }

    #[test]
    fn test_geometric_strategy_returns_available_piece() {
        let mut picker = PiecePicker::new(32u32);
        picker.set_strategy(PieceSelectionStrategy::Geometric);
        for _ in 0..50 {
            let picked = picker.pick_next().expect("piece must be available");
            assert!(picked < 32);
            assert!(!picker.is_completed(picked));
        }
    }

    #[test]
    fn test_select_respects_peer_bitfield() {
        let mut picker = PiecePicker::new(8u32);
        picker.set_strategy(PieceSelectionStrategy::Sequential);

        // Peer only has piece 5 (MSB-first: bit index 5 => 0b0000_0100).
        let bf = [0b0000_0100u8];
        assert_eq!(picker.select(&bf, 8), Some(5));

        picker.mark_completed(5);
        assert_eq!(picker.select(&bf, 8), None);
    }

    #[test]
    fn test_select_with_zero_nbits_returns_none() {
        let mut picker = PiecePicker::new(8u32);
        assert_eq!(picker.select(&[0xFF], 0), None);
    }

    #[test]
    fn test_select_ignores_bits_beyond_bitfield_length() {
        let mut picker = PiecePicker::new(16u32);
        picker.set_strategy(PieceSelectionStrategy::Sequential);
        // Short bitfield: only the first byte is present.
        let bf = [0x00u8];
        assert_eq!(picker.select(&bf, 16), None);
    }

    #[test]
    fn test_endgame_allows_duplicate_requests() {
        let mut picker = PiecePicker::new(4u32);
        picker.set_strategy(PieceSelectionStrategy::Sequential);
        picker.set_endgame_threshold(4);

        picker.mark_in_progress(0, true);
        picker.mark_in_progress(1, true);
        picker.mark_in_progress(2, true);
        picker.mark_in_progress(3, true);

        // All pieces are in flight, but end-game mode re-offers them.
        assert!(picker.endgame_active());
        assert_eq!(picker.pick_next(), Some(0));
    }

    #[test]
    fn test_endgame_candidates_populated_near_completion() {
        let mut picker = PiecePicker::new(4u32);
        picker.set_endgame_threshold(2);

        picker.mark_completed(0);
        assert!(picker.endgame_candidates().is_empty(), "remaining=3 > 2");

        picker.mark_completed(1);
        assert_eq!(picker.endgame_candidates(), &[2, 3]);

        picker.mark_completed(2);
        picker.mark_completed(3);
        assert!(picker.endgame_candidates().is_empty(), "download finished");
    }

    #[test]
    fn test_mark_completed_is_idempotent() {
        let mut picker = PiecePicker::new(3u32);
        picker.mark_completed(1);
        picker.mark_completed(1);
        picker.mark_completed(1);
        assert_eq!(picker.remaining_count(), 2);
        assert!(!picker.is_complete());
    }

    #[test]
    fn test_pick_next_on_empty_torrent() {
        let mut picker = PiecePicker::new(0u32);
        assert_eq!(picker.pick_next(), None);
        assert!(!picker.endgame_active());
    }

    #[test]
    fn test_mark_in_progress_flag_roundtrip() {
        let mut picker = PiecePicker::new(2u32);
        assert!(!picker.is_in_progress(0));
        picker.mark_in_progress(0, true);
        assert!(picker.is_in_progress(0));
        assert!(picker.get_piece_info(0).unwrap().in_progress);
        picker.mark_in_progress(0, false);
        assert!(!picker.is_in_progress(0));
    }

    #[test]
    fn test_mark_completed_clears_in_progress() {
        let mut picker = PiecePicker::new(2u32);
        picker.mark_in_progress(1, true);
        picker.mark_completed(1);
        assert!(!picker.is_in_progress(1));
        assert!(picker.is_completed(1));
    }
}

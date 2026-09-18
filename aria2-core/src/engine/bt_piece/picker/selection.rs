use super::{PiecePicker, PiecePriorityMode, PieceSelectionStrategy};

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

impl PiecePicker {
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
        self.allowed.test(i) && !self.completed.test(i) && !self.in_progress.test(i)
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
    pub(super) fn reopen(&mut self, i: usize) {
        if i < self.head_cursor {
            self.head_cursor = i;
        }
        if i + 1 > self.tail_cursor {
            self.tail_cursor = i + 1;
        }
        // The sorted rarity order is independent from the piece index, so a
        // reopened piece may live anywhere before the cursor.
        self.rarest_cursor = 0;
    }

    /// Core selection routine shared by [`Self::select`] and [`Self::pick_next`].
    ///
    /// `bitfield` restricts the candidates to pieces the peer advertises;
    /// `allow_in_progress` is set in end-game mode, where duplicate requests
    /// for in-flight pieces are intentional.
    pub(super) fn pick_internal(
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
                && self.allowed.test(index)
                && !self.completed.test(index)
                && (allow_in_progress || !self.in_progress.test(index))
                && Self::peer_has(bitfield, index)
            {
                return Some(piece);
            }
        }

        let order = self.scan_order();

        // The BT download scheduler asks for an unrestricted piece. In that
        // case the sorted rarity order can advance once per piece instead of
        // rescanning every completed piece on every selection.
        if bitfield.is_none() && !allow_in_progress && matches!(order, ScanOrder::Rarest) {
            while self.rarest_cursor < self.rarest_order.len() {
                let i = self.rarest_order[self.rarest_cursor] as usize;
                self.rarest_cursor += 1;
                if i < n && self.is_available(i) {
                    return Some(i as u32);
                }
            }
            return None;
        }

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
            p.allowed.test(i)
                && !p.completed.test(i)
                && (allow_in_progress || !p.in_progress.test(i))
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
}

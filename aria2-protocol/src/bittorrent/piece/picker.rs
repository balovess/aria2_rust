use std::collections::{HashMap, HashSet};

/// Priority mode for piece selection strategy.
///
/// Controls which piece the picker selects next when multiple pieces are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PiecePriorityMode {
    /// Default: select the rarest available piece (best for swarm health)
    #[default]
    RarestFirst,
    /// Download lowest-index incomplete piece first (sequential from start)
    /// Useful for streaming/media playback where you need the beginning first
    SequentialHead,
    /// Download highest-index incomplete piece first (sequential from end)
    /// Useful for verifying file integrity from the end
    SequentialTail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PieceSelectionStrategy {
    #[default]
    RarestFirst,
    Sequential,
    Random,
}

#[derive(Debug, Clone)]
pub struct PieceInfo {
    pub index: u32,
    pub priority: i32,
    pub frequency: usize,
    pub completed: bool,
    pub in_progress: bool,
}

impl PieceInfo {
    pub fn new(index: u32) -> Self {
        Self {
            index,
            priority: 0,
            frequency: 0,
            completed: false,
            in_progress: false,
        }
    }
}

pub struct PiecePicker {
    total_pieces: u32,
    pieces: Vec<PieceInfo>,
    strategy: PieceSelectionStrategy,
    /// Priority mode for piece selection (RarestFirst / SequentialHead / SequentialTail)
    priority_mode: PiecePriorityMode,
    peer_availability: HashMap<u32, HashSet<u32>>,
    #[allow(dead_code)] // Seed for deterministic piece selection; must remain to support future reproducible/deterministic piece picking (e.g., for testing or reproducible downloads)
    rng_seed: u64,
    /// Cursor for sequential piece selection (O(1) optimization)
    /// Points to the next piece index to check for sequential selection
    sequential_cursor: u32,
    /// Cursor for SequentialHead mode (lowest index first)
    sequential_head_cursor: u32,
    /// Cursor for SequentialTail mode (highest index first)
    sequential_tail_cursor: u32,
}

impl PiecePicker {
    pub fn new(num_pieces: u32) -> Self {
        let pieces = (0..num_pieces).map(PieceInfo::new).collect();
        Self {
            total_pieces: num_pieces,
            pieces,
            strategy: PieceSelectionStrategy::RarestFirst,
            priority_mode: PiecePriorityMode::RarestFirst,
            peer_availability: HashMap::new(),
            rng_seed: 42,
            sequential_cursor: 0,
            sequential_head_cursor: 0,
            sequential_tail_cursor: if num_pieces > 0 { num_pieces - 1 } else { 0 },
        }
    }

    pub fn set_strategy(&mut self, strategy: PieceSelectionStrategy) {
        self.strategy = strategy;
    }

    /// Set the priority mode for piece selection.
    ///
    /// This controls whether pieces are selected in rarest-first order (default),
    /// sequential from head (lowest index first), or sequential from tail
    /// (highest index first).
    pub fn set_priority_mode(&mut self, mode: PiecePriorityMode) {
        self.priority_mode = mode;
    }

    /// Get the current priority mode.
    pub fn priority_mode(&self) -> PiecePriorityMode {
        self.priority_mode
    }

    pub fn set_piece_priority(&mut self, index: u32, priority: i32) {
        if (index as usize) < self.pieces.len() {
            self.pieces[index as usize].priority = priority;
        }
    }

    pub fn mark_completed(&mut self, index: u32) {
        if (index as usize) < self.pieces.len() {
            self.pieces[index as usize].completed = true;
            self.pieces[index as usize].in_progress = false;
            // Update cursors when a piece is completed
            self.update_sequential_cursors();
        }
    }

    /// Update sequential cursors to point to the next available piece
    /// This ensures O(1) selection by maintaining cursor positions
    fn update_sequential_cursors(&mut self) {
        // Update sequential_cursor (forward direction)
        while self.sequential_cursor < self.total_pieces {
            let piece = &self.pieces[self.sequential_cursor as usize];
            if !piece.completed && !piece.in_progress {
                break;
            }
            self.sequential_cursor += 1;
        }

        // Update sequential_head_cursor (forward direction)
        while self.sequential_head_cursor < self.total_pieces {
            let piece = &self.pieces[self.sequential_head_cursor as usize];
            if !piece.completed && !piece.in_progress {
                break;
            }
            self.sequential_head_cursor += 1;
        }

        // Update sequential_tail_cursor (backward direction)
        while self.sequential_tail_cursor > 0 {
            let piece = &self.pieces[self.sequential_tail_cursor as usize];
            if !piece.completed && !piece.in_progress {
                break;
            }
            self.sequential_tail_cursor -= 1;
        }
        // Check the last position (index 0)
        if self.sequential_tail_cursor == 0 {
            let piece = &self.pieces[0];
            if piece.completed || piece.in_progress {
                // No more available pieces from tail
                self.sequential_tail_cursor = self.total_pieces;
            }
        }
    }

    pub fn mark_in_progress(&mut self, index: u32, in_progress: bool) {
        if (index as usize) < self.pieces.len() {
            self.pieces[index as usize].in_progress = in_progress;
            // Update cursors when in_progress status changes
            self.update_sequential_cursors();
        }
    }

    pub fn add_peer_piece(&mut self, peer_id: u32, piece_index: u32) {
        self.peer_availability
            .entry(peer_id)
            .or_default()
            .insert(piece_index);
        self.update_frequencies();
    }

    pub fn remove_peer(&mut self, peer_id: u32) {
        self.peer_availability.remove(&peer_id);
        self.update_frequencies();
    }

    fn update_frequencies(&mut self) {
        for piece in &mut self.pieces {
            piece.frequency = 0;
        }
        for piece_set in self.peer_availability.values() {
            for &idx in piece_set {
                if (idx as usize) < self.pieces.len() {
                    self.pieces[idx as usize].frequency += 1;
                }
            }
        }
    }

    pub fn pick_next(&mut self) -> Option<u32> {
        match self.priority_mode {
            PiecePriorityMode::RarestFirst => self.pick_by_strategy(),
            PiecePriorityMode::SequentialHead => self.pick_sequential_head(),
            PiecePriorityMode::SequentialTail => self.pick_sequential_tail(),
        }
    }

    /// Internal: pick based on PieceSelectionStrategy (used when in RarestFirst priority mode)
    fn pick_by_strategy(&mut self) -> Option<u32> {
        match self.strategy {
            PieceSelectionStrategy::RarestFirst => self.pick_rarest_first(),
            PieceSelectionStrategy::Sequential => self.pick_sequential(),
            PieceSelectionStrategy::Random => self.pick_random(),
        }
    }

    pub fn select(&self, peer_bitfield: &[u8], nbits: usize) -> Option<u32> {
        if nbits == 0 || peer_bitfield.is_empty() {
            return None;
        }
        let max_pieces = std::cmp::min(nbits, self.pieces.len());
        match self.priority_mode {
            PiecePriorityMode::RarestFirst => match self.strategy {
                PieceSelectionStrategy::RarestFirst => {
                    self.select_rarest_with_bitfield(peer_bitfield, max_pieces)
                }
                PieceSelectionStrategy::Sequential => {
                    self.select_sequential_with_bitfield(peer_bitfield, max_pieces)
                }
                PieceSelectionStrategy::Random => {
                    self.select_random_with_bitfield(peer_bitfield, max_pieces)
                }
            },
            PiecePriorityMode::SequentialHead => {
                self.select_sequential_head(peer_bitfield, max_pieces)
            }
            PiecePriorityMode::SequentialTail => {
                self.select_sequential_tail(peer_bitfield, max_pieces)
            }
        }
    }

    /// Select the lowest-index incomplete piece (sequential from head).
    /// Respects bitfield to only pick pieces the peer actually has.
    fn select_sequential_head(&self, bitfield: &[u8], max_pieces: usize) -> Option<u32> {
        // O(1) optimization: start from cursor position instead of beginning
        let start_pos = self.sequential_head_cursor as usize;
        
        // First, check from cursor to end
        for i in start_pos..max_pieces {
            let piece = &self.pieces[i];
            if piece.completed || piece.in_progress {
                continue;
            }
            if Self::bitfield_has_piece(bitfield, i) {
                return Some(i as u32);
            }
        }
        
        // If not found after cursor, check from beginning to cursor
        for i in 0..start_pos {
            let piece = &self.pieces[i];
            if piece.completed || piece.in_progress {
                continue;
            }
            if Self::bitfield_has_piece(bitfield, i) {
                return Some(i as u32);
            }
        }
        
        None
    }

    /// Select the highest-index incomplete piece (sequential from tail).
    /// Respects bitfield to only pick pieces the peer actually has.
    fn select_sequential_tail(&self, bitfield: &[u8], max_pieces: usize) -> Option<u32> {
        // O(1) optimization: start from cursor position instead of end
        let start_pos = if self.sequential_tail_cursor >= max_pieces as u32 {
            max_pieces.saturating_sub(1)
        } else {
            self.sequential_tail_cursor as usize
        };
        
        // First, check from cursor down to 0
        for i in (0..=start_pos).rev() {
            let piece = &self.pieces[i];
            if piece.completed || piece.in_progress {
                continue;
            }
            if Self::bitfield_has_piece(bitfield, i) {
                return Some(i as u32);
            }
        }
        
        // If not found before cursor, check from end to cursor
        if start_pos + 1 < max_pieces {
            for i in ((start_pos + 1)..max_pieces).rev() {
                let piece = &self.pieces[i];
                if piece.completed || piece.in_progress {
                    continue;
                }
                if Self::bitfield_has_piece(bitfield, i) {
                    return Some(i as u32);
                }
            }
        }
        
        None
    }

    /// Check if a bitfield has a specific piece index set (MSB-first ordering).
    fn bitfield_has_piece(bitfield: &[u8], piece_index: usize) -> bool {
        let byte_idx = piece_index / 8;
        let bit_idx = 7 - (piece_index % 8);
        if byte_idx >= bitfield.len() {
            return false;
        }
        (bitfield[byte_idx] & (1 << bit_idx)) != 0
    }

    fn select_rarest_with_bitfield(&self, bitfield: &[u8], max_pieces: usize) -> Option<u32> {
        let mut best: Option<(usize, &PieceInfo)> = None;
        for (i, piece) in self.pieces.iter().enumerate().take(max_pieces) {
            if piece.completed || piece.in_progress {
                continue;
            }
            let byte_idx = i / 8;
            let bit_idx = 7 - (i % 8);
            if byte_idx >= bitfield.len() {
                continue;
            }
            if (bitfield[byte_idx] & (1 << bit_idx)) == 0 {
                continue;
            }
            match &best {
                None => best = Some((i, piece)),
                Some((_, prev)) => {
                    if piece.frequency < prev.frequency
                        || (piece.frequency == prev.frequency && piece.priority > prev.priority)
                    {
                        best = Some((i, piece));
                    }
                }
            }
        }
        best.map(|(idx, _)| idx as u32)
    }

    fn select_sequential_with_bitfield(&self, bitfield: &[u8], max_pieces: usize) -> Option<u32> {
        // O(1) optimization: start from cursor position instead of beginning
        let start_pos = self.sequential_cursor as usize;
        
        // First, check from cursor to end
        for i in start_pos..max_pieces {
            let piece = &self.pieces[i];
            if piece.completed || piece.in_progress {
                continue;
            }
            let byte_idx = i / 8;
            let bit_idx = 7 - (i % 8);
            if byte_idx < bitfield.len() && (bitfield[byte_idx] & (1 << bit_idx)) != 0 {
                return Some(piece.index);
            }
        }
        
        // If not found after cursor, check from beginning to cursor (in case cursor was reset)
        for i in 0..start_pos {
            let piece = &self.pieces[i];
            if piece.completed || piece.in_progress {
                continue;
            }
            let byte_idx = i / 8;
            let bit_idx = 7 - (i % 8);
            if byte_idx < bitfield.len() && (bitfield[byte_idx] & (1 << bit_idx)) != 0 {
                return Some(piece.index);
            }
        }
        
        None
    }

    fn select_random_with_bitfield(&self, bitfield: &[u8], max_pieces: usize) -> Option<u32> {
        use rand::Rng;
        let available: Vec<u32> = self
            .pieces
            .iter()
            .enumerate()
            .take(max_pieces)
            .filter_map(|(i, piece)| {
                if piece.completed || piece.in_progress {
                    return None;
                }
                let byte_idx = i / 8;
                let bit_idx = 7 - (i % 8);
                if byte_idx >= bitfield.len() {
                    return None;
                }
                if (bitfield[byte_idx] & (1 << bit_idx)) == 0 {
                    return None;
                }
                Some(piece.index)
            })
            .collect();
        if available.is_empty() {
            return None;
        }
        let mut rng = rand::thread_rng();
        Some(available[rng.gen_range(0..available.len())])
    }

    fn pick_rarest_first(&mut self) -> Option<u32> {
        let mut candidates: Vec<&PieceInfo> = self
            .pieces
            .iter()
            .filter(|p| !p.completed && !p.in_progress && p.frequency > 0)
            .collect();

        candidates.sort_by(|a, b| {
            a.frequency
                .cmp(&b.frequency)
                .then(b.priority.cmp(&a.priority))
        });

        candidates.first().map(|p| p.index)
    }

    fn pick_sequential(&self) -> Option<u32> {
        // O(1) optimization: use cursor to directly access the next available piece
        if self.sequential_cursor >= self.total_pieces {
            return None;
        }
        let piece = &self.pieces[self.sequential_cursor as usize];
        if !piece.completed && !piece.in_progress {
            Some(piece.index)
        } else {
            // Fallback: cursor might be out of sync, do a linear search
            self.pieces
                .iter()
                .find(|p| !p.completed && !p.in_progress)
                .map(|p| p.index)
        }
    }

    fn pick_random(&self) -> Option<u32> {
        use rand::Rng;
        let available: Vec<u32> = self
            .pieces
            .iter()
            .filter(|p| !p.completed && !p.in_progress && p.frequency > 0)
            .map(|p| p.index)
            .collect();

        if available.is_empty() {
            return None;
        }
        let mut rng = rand::thread_rng();
        Some(available[rng.gen_range(0..available.len())])
    }

    /// Pick the lowest-index incomplete piece (sequential from head, no bitfield filter).
    fn pick_sequential_head(&self) -> Option<u32> {
        // O(1) optimization: use cursor to directly access the next available piece
        if self.sequential_head_cursor >= self.total_pieces {
            return None;
        }
        let piece = &self.pieces[self.sequential_head_cursor as usize];
        if !piece.completed && !piece.in_progress {
            Some(piece.index)
        } else {
            // Fallback: cursor might be out of sync, do a linear search
            self.pieces
                .iter()
                .filter(|p| !p.completed && !p.in_progress)
                .map(|p| p.index)
                .min()
        }
    }

    /// Pick the highest-index incomplete piece (sequential from tail, no bitfield filter).
    fn pick_sequential_tail(&self) -> Option<u32> {
        // O(1) optimization: use cursor to directly access the next available piece
        if self.sequential_tail_cursor >= self.total_pieces {
            return None;
        }
        let piece = &self.pieces[self.sequential_tail_cursor as usize];
        if !piece.completed && !piece.in_progress {
            Some(piece.index)
        } else {
            // Fallback: cursor might be out of sync, do a linear search
            self.pieces
                .iter()
                .filter(|p| !p.completed && !p.in_progress)
                .map(|p| p.index)
                .max()
        }
    }

    pub fn endgame_candidates(&self) -> Vec<u32> {
        let incomplete_count = self.pieces.iter().filter(|p| !p.completed).count();
        if incomplete_count > 5 {
            return vec![];
        }

        self.pieces
            .iter()
            .filter(|p| !p.completed)
            .map(|p| p.index)
            .collect()
    }

    pub fn completed_count(&self) -> u32 {
        self.pieces.iter().filter(|p| p.completed).count() as u32
    }

    pub fn remaining_count(&self) -> u32 {
        self.total_pieces - self.completed_count()
    }

    pub fn is_complete(&self) -> bool {
        self.completed_count() == self.total_pieces
    }

    pub fn progress_percent(&self) -> f64 {
        if self.total_pieces == 0 {
            return 100.0;
        }
        self.completed_count() as f64 / self.total_pieces as f64 * 100.0
    }

    pub fn set_frequencies_from_peers(&mut self, peer_counts: &[usize]) {
        for (i, piece) in self.pieces.iter_mut().enumerate() {
            if i < peer_counts.len() {
                piece.frequency = peer_counts[i];
            } else {
                piece.frequency = 0;
            }
        }
    }

    pub fn get_piece_info(&self, index: u32) -> Option<&PieceInfo> {
        self.pieces.get(index as usize)
    }

    /// Get mutable reference to piece info by index
    ///
    /// Used for modifying piece properties like priority when receiving
    /// Suggest messages (BEP 6).
    pub fn get_piece_info_mut(&mut self, index: u32) -> Option<&mut PieceInfo> {
        self.pieces.get_mut(index as usize)
    }

    /// Iterate over all pieces
    ///
    /// Returns an iterator over all PieceInfo entries for filtering/sorting operations.
    pub fn pieces_iter(&self) -> impl Iterator<Item = &PieceInfo> {
        self.pieces.iter()
    }
    
    /// Export completed pieces as a compact bitfield (MSB-first ordering)
    ///
    /// Returns `Vec<u8>` where each byte represents 8 pieces.
    /// Bit ordering is MSB-first: piece 0 is bit 7 of byte 0, piece 7 is bit 0 of byte 0.
    /// This matches the BitTorrent protocol bitfield format.
    ///
    /// Used for session persistence to save progress state.
    pub fn export_bitfield(&self) -> Vec<u8> {
        if self.total_pieces == 0 {
            return vec![];
        }
        
        let num_bytes = (self.total_pieces as usize).div_ceil(8);
        let mut bitfield = vec![0u8; num_bytes];
        
        for (i, piece) in self.pieces.iter().enumerate() {
            if piece.completed {
                let byte_idx = i / 8;
                let bit_idx = 7 - (i % 8);  // MSB-first ordering
                if byte_idx < bitfield.len() {
                    bitfield[byte_idx] |= 1 << bit_idx;
                }
            }
        }
        bitfield
    }
    
    /// Import bitfield to mark pieces as completed
    ///
    /// Parses a compact bitfield (MSB-first ordering) and marks
    /// corresponding pieces as completed.
    ///
    /// Used for session restoration to recover progress state.
    pub fn import_bitfield(&mut self, bitfield: &[u8]) {
        for (i, piece) in self.pieces.iter_mut().enumerate() {
            let byte_idx = i / 8;
            let bit_idx = 7 - (i % 8);  // MSB-first ordering
            if byte_idx < bitfield.len() && (bitfield[byte_idx] & (1 << bit_idx)) != 0 {
                piece.completed = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_picker_creation() {
        let picker = PiecePicker::new(10);
        assert_eq!(picker.total_pieces, 10);
        assert_eq!(picker.remaining_count(), 10);
        assert!(!picker.is_complete());
    }

    #[test]
    fn test_rarest_first_selection() {
        let mut picker = PiecePicker::new(5);

        picker.add_peer_piece(1, 0);
        picker.add_peer_piece(2, 0);
        picker.add_peer_piece(1, 1);
        picker.add_peer_piece(3, 4);

        let picked = picker.pick_next();
        assert!(picked.is_some());
        let idx = picked.unwrap();
        assert!(idx == 1 || idx == 4);
        assert_eq!(picker.pieces[idx as usize].frequency, 1);
    }

    #[test]
    fn test_sequential_selection() {
        let mut picker = PiecePicker::new(5);
        picker.set_strategy(PieceSelectionStrategy::Sequential);
        picker.mark_in_progress(0, true);

        assert_eq!(picker.pick_next(), Some(1));
    }

    #[test]
    fn test_marking_completion() {
        let mut picker = PiecePicker::new(3);
        picker.mark_completed(0);
        picker.mark_completed(1);
        assert_eq!(picker.completed_count(), 2);
        assert_eq!(picker.remaining_count(), 1);
        let progress = picker.progress_percent();
        assert!(progress > 66.0 && progress < 67.0);
    }

    #[test]
    fn test_endgame_mode() {
        let mut picker = PiecePicker::new(3);
        picker.mark_completed(0);
        assert_eq!(picker.endgame_candidates().len(), 2);
    }

    #[test]
    fn test_all_done() {
        let mut picker = PiecePicker::new(2);
        picker.mark_completed(0);
        picker.mark_completed(1);
        assert!(picker.is_complete());
        assert_eq!(picker.pick_next(), None);
    }

    fn make_bitfield(pieces: usize, indices: &[usize]) -> Vec<u8> {
        let len = pieces.div_ceil(8);
        let mut bf = vec![0u8; len];
        for &idx in indices {
            if idx < pieces {
                bf[idx / 8] |= 1 << (7 - (idx % 8));
            }
        }
        bf
    }

    #[test]
    fn test_select_with_bitfield_basic() {
        let mut picker = PiecePicker::new(8);
        picker.set_frequencies_from_peers(&[5, 1, 3, 2, 4, 6, 1, 0]);

        let bf = make_bitfield(8, &[0, 1, 2, 3, 4, 5, 6]);
        let selected = picker.select(&bf, 8);
        assert_eq!(
            selected,
            Some(1),
            "should pick piece with frequency=1 in bitfield"
        );
    }

    #[test]
    fn test_select_rarest_prefers_lowest_frequency() {
        let mut picker = PiecePicker::new(8);
        picker.set_frequencies_from_peers(&[10, 1, 5, 1, 8, 3, 7, 2]);

        let bf = make_bitfield(8, &[0, 1, 2, 3, 4, 5, 6, 7]);
        let selected = picker.select(&bf, 8);
        assert_eq!(
            selected,
            Some(1),
            "piece 1 has lowest freq=1 and lower index than piece 3"
        );
    }

    #[test]
    fn test_select_empty_bitfield_returns_none() {
        let picker = PiecePicker::new(8);
        let bf = vec![0u8; 1];
        assert_eq!(picker.select(&bf, 8), None);
    }

    #[test]
    fn test_select_all_completed_returns_none() {
        let mut picker = PiecePicker::new(4);
        for i in 0..4 {
            picker.mark_completed(i);
        }
        let bf = make_bitfield(4, &[0, 1, 2, 3]);
        assert_eq!(picker.select(&bf, 4), None);
    }

    #[test]
    fn test_set_frequencies_updates_correctly() {
        let mut picker = PiecePicker::new(4);
        picker.set_frequencies_from_peers(&[3, 7, 1, 5]);
        assert_eq!(picker.get_piece_info(0).unwrap().frequency, 3);
        assert_eq!(picker.get_piece_info(1).unwrap().frequency, 7);
        assert_eq!(picker.get_piece_info(2).unwrap().frequency, 1);
        assert_eq!(picker.get_piece_info(3).unwrap().frequency, 5);
    }

    #[test]
    fn test_rarest_ignores_in_progress_pieces() {
        let mut picker = PiecePicker::new(6);
        picker.set_frequencies_from_peers(&[1, 1, 1, 1, 1, 100]);
        picker.mark_in_progress(5, true);

        let bf = make_bitfield(6, &[0, 1, 2, 3, 4, 5]);
        let selected = picker.select(&bf, 6);
        assert_ne!(selected, Some(5), "in-progress piece should be skipped");
        assert!(selected.is_some(), "should find another piece");
    }

    #[test]
    fn test_rarest_respects_priority() {
        let mut picker = PiecePicker::new(4);
        picker.set_frequencies_from_peers(&[1, 1, 1, 1]);
        picker.set_piece_priority(2, 99);

        let bf = make_bitfield(4, &[0, 1, 2, 3]);
        let selected = picker.select(&bf, 4);
        assert_eq!(
            selected,
            Some(2),
            "highest priority piece should win when freq tied"
        );
    }

    #[test]
    fn test_sequential_always_picks_lowest_available() {
        let mut picker = PiecePicker::new(8);
        picker.set_strategy(PieceSelectionStrategy::Sequential);
        picker.mark_in_progress(0, true);
        picker.mark_completed(1);

        let bf = make_bitfield(8, &[2, 3, 4, 5, 6, 7]);
        assert_eq!(
            picker.select(&bf, 8),
            Some(2),
            "sequential picks lowest available in bitfield"
        );
    }

    #[test]
    fn test_random_uses_bitfield_filter() {
        let mut picker = PiecePicker::new(8);
        picker.set_strategy(PieceSelectionStrategy::Random);
        picker.set_frequencies_from_peers(&[1; 8]);

        let bf = make_bitfield(8, &[3, 5, 7]);
        for _ in 0..20 {
            let sel = picker.select(&bf, 8);
            assert!(sel.is_some());
            let idx = sel.unwrap();
            assert!(
                idx == 3 || idx == 5 || idx == 7,
                "random should only pick from bitfield-available pieces, got {}",
                idx
            );
        }
    }

    #[test]
    fn test_endgame_candidates_threshold() {
        let mut picker = PiecePicker::new(3);
        picker.mark_completed(0);
        assert_eq!(
            picker.endgame_candidates().len(),
            2,
            "≤5 incomplete → all returned"
        );

        let mut picker2 = PiecePicker::new(10);
        picker2.mark_completed(0);
        assert!(
            picker2.endgame_candidates().is_empty(),
            ">5 incomplete → empty"
        );
    }

    #[test]
    fn test_select_with_zero_nbits_returns_none() {
        let picker = PiecePicker::new(8);
        assert_eq!(picker.select(&[0xFF], 0), None);
    }

    #[test]
    fn test_get_piece_info_out_of_range() {
        let picker = PiecePicker::new(4);
        assert!(picker.get_piece_info(99).is_none());
    }

    #[test]
    fn test_get_piece_info_mut_modifies_priority() {
        let mut picker = PiecePicker::new(5);

        // Check initial priority
        assert_eq!(picker.get_piece_info(2).unwrap().priority, 0);

        // Modify priority via mutable reference
        if let Some(info) = picker.get_piece_info_mut(2) {
            info.priority = 100;
        }

        // Verify change persisted
        assert_eq!(picker.get_piece_info(2).unwrap().priority, 100);
    }

    #[test]
    fn test_get_piece_info_mut_out_of_range() {
        let mut picker = PiecePicker::new(3);
        assert!(picker.get_piece_info_mut(99).is_none());
    }

    #[test]
    fn test_pieces_iter_returns_all_pieces() {
        let picker = PiecePicker::new(5);
        let pieces: Vec<_> = picker.pieces_iter().collect();

        assert_eq!(pieces.len(), 5);
        for (i, piece) in pieces.iter().enumerate() {
            assert_eq!(piece.index, i as u32);
        }
    }

    #[test]
    fn test_pieces_iter_filters_correctly() {
        let mut picker = PiecePicker::new(5);
        picker.mark_completed(0);
        picker.mark_completed(1);

        // Count incomplete pieces via iterator
        let incomplete_count = picker.pieces_iter().filter(|p| !p.completed).count();

        assert_eq!(incomplete_count, 3); // Pieces 2,3,4
    }

    // ==================== G2: PiecePriorityMode Tests ====================

    #[test]
    fn test_sequential_head_picks_lowest_index_first() {
        let mut picker = PiecePicker::new(10);
        picker.set_priority_mode(PiecePriorityMode::SequentialHead);

        // Mark some pieces as completed/in-progress
        picker.mark_completed(0);
        picker.mark_in_progress(1, true);

        // Should pick piece 2 (lowest available index)
        let picked = picker.pick_next();
        assert_eq!(
            picked,
            Some(2),
            "SequentialHead should pick lowest available index"
        );

        // After picking 2, next should be 3
        picker.mark_in_progress(2, true);
        let picked2 = picker.pick_next();
        assert_eq!(picked2, Some(3), "Should continue with next lowest");
    }

    #[test]
    fn test_sequential_tail_picks_highest_index_first() {
        let mut picker = PiecePicker::new(10);
        picker.set_priority_mode(PiecePriorityMode::SequentialTail);

        // Mark some pieces as completed/in-progress
        picker.mark_completed(9);
        picker.mark_in_progress(8, true);

        // Should pick piece 7 (highest available index)
        let picked = picker.pick_next();
        assert_eq!(
            picked,
            Some(7),
            "SequentialTail should pick highest available index"
        );

        // After picking 7, next should be 6
        picker.mark_in_progress(7, true);
        let picked2 = picker.pick_next();
        assert_eq!(picked2, Some(6), "Should continue with next highest");
    }

    #[test]
    fn test_rarest_first_default_behavior() {
        let mut picker = PiecePicker::new(5);
        // Default mode is RarestFirst
        assert_eq!(picker.priority_mode(), PiecePriorityMode::RarestFirst);

        // Set up frequencies so piece 1 is rarest
        picker.add_peer_piece(1, 0);
        picker.add_peer_piece(1, 0); // piece 0: freq 2
        picker.add_peer_piece(2, 1); // piece 1: freq 1 (rarest)
        picker.add_peer_piece(3, 2);
        picker.add_peer_piece(3, 2); // piece 2: freq 2

        let picked = picker.pick_next();
        // In RarestFirst mode via pick_by_strategy -> pick_rarest_first,
        // picks the piece with lowest frequency among those with freq > 0
        assert!(
            picked.is_some(),
            "RarestFirst should pick a piece when frequencies are set"
        );
        // Verify the picked piece has frequency > 0 (rarest-first only considers available pieces)
        let picked_idx = picked.unwrap();
        assert!(
            picker.get_piece_info(picked_idx).unwrap().frequency > 0,
            "Picked piece should have frequency > 0"
        );
    }

    #[test]
    fn test_mode_switch_mid_download() {
        let mut picker = PiecePicker::new(8);

        // Start in SequentialHead mode
        picker.set_priority_mode(PiecePriorityMode::SequentialHead);
        picker.mark_completed(0);
        assert_eq!(picker.pick_next(), Some(1));

        // Switch to SequentialTail mid-download
        picker.set_priority_mode(PiecePriorityMode::SequentialTail);
        picker.mark_completed(7);
        // Should now pick highest remaining (which is 6)
        assert_eq!(
            picker.pick_next(),
            Some(6),
            "After switching to SequentialTail, should pick highest index"
        );

        // Switch back to RarestFirst
        picker.set_priority_mode(PiecePriorityMode::RarestFirst);
        // Should pick based on frequency again (all have freq 0 except what we add)
        picker.add_peer_piece(10, 3);
        picker.add_peer_piece(10, 3);
        picker.add_peer_piece(10, 5);
        // Piece 3 has freq 2, piece 5 has freq 1 -> piece 5 is rarer
        // Verify that a piece with frequency > 0 is picked (rarest-first behavior)
        let picked = picker.pick_next();
        assert!(
            picked.is_some(),
            "RarestFirst mode should pick a piece with available frequency data"
        );
        let picked_idx = picked.unwrap();
        assert!(
            picker.get_piece_info(picked_idx).unwrap().frequency > 0,
            "Picked piece in RarestFirst mode should have freq > 0"
        );
    }

    #[test]
    fn test_priority_mode_setter_getter() {
        let mut picker = PiecePicker::new(5);

        assert_eq!(picker.priority_mode(), PiecePriorityMode::RarestFirst);

        picker.set_priority_mode(PiecePriorityMode::SequentialHead);
        assert_eq!(picker.priority_mode(), PiecePriorityMode::SequentialHead);

        picker.set_priority_mode(PiecePriorityMode::SequentialTail);
        assert_eq!(picker.priority_mode(), PiecePriorityMode::SequentialTail);
    }

    #[test]
    fn test_select_sequential_head_with_bitfield() {
        let mut picker = PiecePicker::new(8);
        picker.set_priority_mode(PiecePriorityMode::SequentialHead);
        picker.mark_completed(0);
        picker.mark_in_progress(1, true);

        // Bitfield has pieces 2-7 available
        let bf = make_bitfield(8, &[2, 3, 4, 5, 6, 7]);
        let selected = picker.select(&bf, 8);
        assert_eq!(
            selected,
            Some(2),
            "SequentialHead with bitfield should pick lowest available in both"
        );
    }

    #[test]
    fn test_select_sequential_tail_with_bitfield() {
        let mut picker = PiecePicker::new(8);
        picker.set_priority_mode(PiecePriorityMode::SequentialTail);
        picker.mark_completed(7);
        picker.mark_in_progress(6, true);

        // Bitfield has pieces 0-5 available
        let bf = make_bitfield(8, &[0, 1, 2, 3, 4, 5]);
        let selected = picker.select(&bf, 8);
        assert_eq!(
            selected,
            Some(5),
            "SequentialTail with bitfield should pick highest available in both"
        );
    }

    // ==================== Performance Tests ====================

    #[test]
    fn test_sequential_performance_with_large_torrent() {
        // Test with a large number of pieces to verify O(1) performance
        let num_pieces = 10000;
        let mut picker = PiecePicker::new(num_pieces);
        picker.set_strategy(PieceSelectionStrategy::Sequential);

        // Mark first 5000 pieces as completed
        for i in 0..5000 {
            picker.mark_completed(i);
        }

        // The next pick should be piece 5000 (O(1) with cursor)
        let start = std::time::Instant::now();
        let picked = picker.pick_next();
        let duration = start.elapsed();

        assert_eq!(picked, Some(5000), "Should pick piece 5000 after completing 0-4999");
        
        // With O(1) optimization, this should be very fast (< 1 microsecond)
        // Even with 10000 pieces, cursor-based lookup is constant time
        println!("Sequential pick time for 10000 pieces: {:?}", duration);
        assert!(duration.as_micros() < 100, "O(1) pick should be very fast");
    }

    #[test]
    fn test_sequential_head_performance_with_large_torrent() {
        let num_pieces = 10000;
        let mut picker = PiecePicker::new(num_pieces);
        picker.set_priority_mode(PiecePriorityMode::SequentialHead);

        // Mark first 5000 pieces as completed
        for i in 0..5000 {
            picker.mark_completed(i);
        }

        let start = std::time::Instant::now();
        let picked = picker.pick_next();
        let duration = start.elapsed();

        assert_eq!(picked, Some(5000), "SequentialHead should pick piece 5000");
        println!("SequentialHead pick time for 10000 pieces: {:?}", duration);
        assert!(duration.as_micros() < 100, "O(1) pick should be very fast");
    }

    #[test]
    fn test_sequential_tail_performance_with_large_torrent() {
        let num_pieces = 10000;
        let mut picker = PiecePicker::new(num_pieces);
        picker.set_priority_mode(PiecePriorityMode::SequentialTail);

        // Mark last 5000 pieces as completed
        for i in 5000..10000 {
            picker.mark_completed(i);
        }

        let start = std::time::Instant::now();
        let picked = picker.pick_next();
        let duration = start.elapsed();

        assert_eq!(picked, Some(4999), "SequentialTail should pick piece 4999");
        println!("SequentialTail pick time for 10000 pieces: {:?}", duration);
        assert!(duration.as_micros() < 100, "O(1) pick should be very fast");
    }

    #[test]
    fn test_cursor_updates_correctly_on_completion() {
        let mut picker = PiecePicker::new(10);
        picker.set_strategy(PieceSelectionStrategy::Sequential);

        // Initially should pick piece 0
        assert_eq!(picker.pick_next(), Some(0));

        // Mark piece 0 as completed, cursor should move to 1
        picker.mark_completed(0);
        assert_eq!(picker.pick_next(), Some(1));

        // Mark piece 2 as completed (skip piece 1), cursor should stay at 1
        picker.mark_completed(2);
        assert_eq!(picker.pick_next(), Some(1));

        // Mark piece 1 as completed, cursor should move to 3 (since 2 is also completed)
        picker.mark_completed(1);
        assert_eq!(picker.pick_next(), Some(3));
    }

    #[test]
    fn test_cursor_handles_in_progress_correctly() {
        let mut picker = PiecePicker::new(10);
        picker.set_strategy(PieceSelectionStrategy::Sequential);

        // Mark piece 0 as in_progress, cursor should move to 1
        picker.mark_in_progress(0, true);
        assert_eq!(picker.pick_next(), Some(1));

        // Mark piece 1 as completed, cursor should move to 2
        picker.mark_completed(1);
        assert_eq!(picker.pick_next(), Some(2));

        // Mark piece 0 as not in_progress, cursor should still be at 2
        // (because we've already passed piece 0)
        picker.mark_in_progress(0, false);
        assert_eq!(picker.pick_next(), Some(2));
    }

    #[test]
    fn test_no_pieces_skipped_in_sequential_mode() {
        let mut picker = PiecePicker::new(100);
        picker.set_strategy(PieceSelectionStrategy::Sequential);

        // Randomly mark some pieces as completed
        let completed_indices = vec![5, 10, 15, 20, 25, 30, 35, 40, 45, 50];
        for &i in &completed_indices {
            picker.mark_completed(i);
        }

        // Pick all remaining pieces and verify none are skipped
        let mut picked_pieces = Vec::new();
        while let Some(piece) = picker.pick_next() {
            picked_pieces.push(piece);
            picker.mark_completed(piece);
        }

        // Verify all pieces were picked
        assert_eq!(picked_pieces.len(), 90, "Should have picked 90 remaining pieces");
        
        // Verify no completed pieces were picked
        for &completed in &completed_indices {
            assert!(!picked_pieces.contains(&completed), "Completed piece {} should not be picked", completed);
        }

        // Verify pieces are in order (sequential)
        for i in 1..picked_pieces.len() {
            assert!(picked_pieces[i] > picked_pieces[i-1], "Pieces should be picked in order");
        }
    }

    #[test]
    fn test_sequential_head_no_pieces_skipped() {
        let mut picker = PiecePicker::new(100);
        picker.set_priority_mode(PiecePriorityMode::SequentialHead);

        // Mark some pieces as completed
        for i in (0..100).step_by(3) {
            picker.mark_completed(i);
        }

        // Pick all remaining pieces
        let mut picked_pieces = Vec::new();
        while let Some(piece) = picker.pick_next() {
            picked_pieces.push(piece);
            picker.mark_completed(piece);
        }

        // Count completed pieces: 0, 3, 6, ..., 99 = 34 pieces (0 to 99 inclusive, step 3)
        // Remaining: 100 - 34 = 66 pieces
        assert_eq!(picked_pieces.len(), 66, "Should have picked 66 remaining pieces");
        
        // Verify pieces are in ascending order
        for i in 1..picked_pieces.len() {
            assert!(picked_pieces[i] > picked_pieces[i-1], "Pieces should be in ascending order");
        }
    }

    #[test]
    fn test_sequential_tail_no_pieces_skipped() {
        let mut picker = PiecePicker::new(100);
        picker.set_priority_mode(PiecePriorityMode::SequentialTail);

        // Mark some pieces as completed
        for i in (0..100).step_by(3) {
            picker.mark_completed(i);
        }

        // Pick all remaining pieces
        let mut picked_pieces = Vec::new();
        while let Some(piece) = picker.pick_next() {
            picked_pieces.push(piece);
            picker.mark_completed(piece);
        }

        // Count completed pieces: 0, 3, 6, ..., 99 = 34 pieces
        // Remaining: 100 - 34 = 66 pieces
        assert_eq!(picked_pieces.len(), 66, "Should have picked 66 remaining pieces");
        
        // Verify pieces are in descending order
        for i in 1..picked_pieces.len() {
            assert!(picked_pieces[i] < picked_pieces[i-1], "Pieces should be in descending order");
        }
    }
    
    // ==================== Export/Import Bitfield Tests ====================
    
    #[test]
    fn test_export_bitfield_empty() {
        let picker = PiecePicker::new(10);
        let bitfield = picker.export_bitfield();
        
        // All pieces incomplete -> all zeros
        assert_eq!(bitfield.len(), 2, "10 pieces should need 2 bytes");
        assert_eq!(bitfield, vec![0x00, 0x00], "Empty bitfield should be all zeros");
    }
    
    #[test]
    fn test_export_bitfield_all_complete() {
        let mut picker = PiecePicker::new(8);
        for i in 0..8 {
            picker.mark_completed(i);
        }
        
        let bitfield = picker.export_bitfield();
        assert_eq!(bitfield.len(), 1, "8 pieces should need 1 byte");
        assert_eq!(bitfield, vec![0xFF], "All complete should be 0xFF");
    }
    
    #[test]
    fn test_export_bitfield_partial() {
        let mut picker = PiecePicker::new(16);
        // Mark pieces 0, 3, 7, 10, 15 as complete
        picker.mark_completed(0);
        picker.mark_completed(3);
        picker.mark_completed(7);
        picker.mark_completed(10);
        picker.mark_completed(15);
        
        let bitfield = picker.export_bitfield();
        assert_eq!(bitfield.len(), 2, "16 pieces should need 2 bytes");
        
        // Byte 0: pieces 0-7
        // piece 0 -> bit 7 (MSB) -> 0x80
        // piece 3 -> bit 4 -> 0x10
        // piece 7 -> bit 0 -> 0x01
        // Byte 0 = 0x80 | 0x10 | 0x01 = 0x91
        assert_eq!(bitfield[0], 0x91, "Byte 0 should have pieces 0,3,7 set");
        
        // Byte 1: pieces 8-15
        // piece 10 -> bit 5 (in byte 1, piece 10 is index 2, bit 7-2=5) -> 0x20
        // piece 15 -> bit 0 -> 0x01
        // Byte 1 = 0x20 | 0x01 = 0x21
        assert_eq!(bitfield[1], 0x21, "Byte 1 should have pieces 10,15 set");
    }
    
    #[test]
    fn test_import_bitfield() {
        let mut picker = PiecePicker::new(8);
        
        // Import bitfield with pieces 0, 2, 5, 7 complete
        // 0x80 | 0x20 | 0x04 | 0x01 = 0xA5
        picker.import_bitfield(&[0xA5]);
        
        assert!(picker.get_piece_info(0).unwrap().completed, "Piece 0 should be complete");
        assert!(picker.get_piece_info(2).unwrap().completed, "Piece 2 should be complete");
        assert!(picker.get_piece_info(5).unwrap().completed, "Piece 5 should be complete");
        assert!(picker.get_piece_info(7).unwrap().completed, "Piece 7 should be complete");
        
        assert!(!picker.get_piece_info(1).unwrap().completed, "Piece 1 should not be complete");
        assert!(!picker.get_piece_info(3).unwrap().completed, "Piece 3 should not be complete");
        assert!(!picker.get_piece_info(4).unwrap().completed, "Piece 4 should not be complete");
        assert!(!picker.get_piece_info(6).unwrap().completed, "Piece 6 should not be complete");
        
        assert_eq!(picker.completed_count(), 4, "Should have 4 completed pieces");
    }
    
    #[test]
    fn test_export_import_roundtrip() {
        let mut picker = PiecePicker::new(24);
        
        // Mark various pieces as complete
        for i in [0, 5, 10, 15, 20, 23] {
            picker.mark_completed(i);
        }
        
        let exported = picker.export_bitfield();
        
        // Create new picker and import
        let mut picker2 = PiecePicker::new(24);
        picker2.import_bitfield(&exported);
        
        // Verify same pieces are complete
        for i in 0..24 {
            let p1 = picker.get_piece_info(i).unwrap().completed;
            let p2 = picker2.get_piece_info(i).unwrap().completed;
            assert_eq!(p1, p2, "Piece {} completion should match after roundtrip", i);
        }
        
        assert_eq!(picker.completed_count(), picker2.completed_count());
    }
    
    #[test]
    fn test_export_bitfield_zero_pieces() {
        let picker = PiecePicker::new(0);
        let bitfield = picker.export_bitfield();
        assert!(bitfield.is_empty(), "Zero pieces should produce empty bitfield");
    }
}

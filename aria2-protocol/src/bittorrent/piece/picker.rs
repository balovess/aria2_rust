use std::collections::{HashMap, HashSet};

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
    peer_availability: HashMap<u32, HashSet<u32>>,
    #[allow(dead_code)]
    rng_seed: u64,
}

impl PiecePicker {
    pub fn new(num_pieces: u32) -> Self {
        let pieces = (0..num_pieces).map(PieceInfo::new).collect();
        Self {
            total_pieces: num_pieces,
            pieces,
            strategy: PieceSelectionStrategy::RarestFirst,
            peer_availability: HashMap::new(),
            rng_seed: 42,
        }
    }

    pub fn set_strategy(&mut self, strategy: PieceSelectionStrategy) {
        self.strategy = strategy;
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
        }
    }

    pub fn mark_in_progress(&mut self, index: u32, in_progress: bool) {
        if (index as usize) < self.pieces.len() {
            self.pieces[index as usize].in_progress = in_progress;
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
        match self.strategy {
            PieceSelectionStrategy::RarestFirst => {
                self.select_rarest_with_bitfield(peer_bitfield, max_pieces)
            }
            PieceSelectionStrategy::Sequential => {
                self.select_sequential_with_bitfield(peer_bitfield, max_pieces)
            }
            PieceSelectionStrategy::Random => {
                self.select_random_with_bitfield(peer_bitfield, max_pieces)
            }
        }
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
        for (i, piece) in self.pieces.iter().enumerate().take(max_pieces) {
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
        self.pieces
            .iter()
            .find(|p| !p.completed && !p.in_progress)
            .map(|p| p.index)
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
        let len = (pieces + 7) / 8;
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
}

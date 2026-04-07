use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceSelectionStrategy {
    RarestFirst,
    Sequential,
    Random,
}

impl Default for PieceSelectionStrategy {
    fn default() -> Self { PieceSelectionStrategy::RarestFirst }
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
        self.peer_availability.entry(peer_id)
            .or_insert_with(HashSet::new)
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
        for (_, piece_set) in &self.peer_availability {
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

    fn pick_rarest_first(&mut self) -> Option<u32> {
        let mut candidates: Vec<&PieceInfo> = self.pieces.iter()
            .filter(|p| !p.completed && !p.in_progress && p.frequency > 0)
            .collect();

        candidates.sort_by(|a, b| {
            a.frequency.cmp(&b.frequency)
                .then(b.priority.cmp(&a.priority))
        });

        candidates.first().map(|p| p.index)
    }

    fn pick_sequential(&self) -> Option<u32> {
        self.pieces.iter()
            .find(|p| !p.completed && !p.in_progress)
            .map(|p| p.index)
    }

    fn pick_random(&self) -> Option<u32> {
        use rand::Rng;
        let available: Vec<u32> = self.pieces.iter()
            .filter(|p| !p.completed && !p.in_progress && p.frequency > 0)
            .map(|p| p.index)
            .collect();

        if available.is_empty() { return None; }
        let mut rng = rand::thread_rng();
        Some(available[rng.gen_range(0..available.len())])
    }

    pub fn endgame_candidates(&self) -> Vec<u32> {
        let incomplete_count = self.pieces.iter().filter(|p| !p.completed).count();
        if incomplete_count > 5 { return vec![]; }

        self.pieces.iter()
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
        if self.total_pieces == 0 { return 100.0; }
        self.completed_count() as f64 / self.total_pieces as f64 * 100.0
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
}

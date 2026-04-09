use std::collections::HashMap;
use std::time::Instant;

pub struct PeerBitfieldEntry {
    pub peer_id: String,
    pub have_pieces: Vec<bool>,
    pub raw_bitfield: Vec<u8>,
    pub last_updated: Instant,
}

pub struct PeerBitfieldTracker {
    total_pieces: u32,
    peers: HashMap<String, PeerBitfieldEntry>,
    piece_peer_count: Vec<usize>,
}

pub struct PeerTrackerStats {
    pub peer_count: usize,
    pub tracked_pieces: u32,
    pub avg_pieces_per_peer: f64,
    pub rarest_piece_count: usize,
    pub is_endgame: bool,
}

impl PeerBitfieldTracker {
    pub fn new(total_pieces: u32) -> Self {
        Self {
            total_pieces,
            peers: HashMap::new(),
            piece_peer_count: vec![0usize; total_pieces as usize],
        }
    }

    pub fn update_peer_bitfield(&mut self, peer_id: &str, bitfield: &[u8]) {
        if let Some(existing) = self.peers.get_mut(peer_id) {
            for (i, had) in existing.have_pieces.iter().enumerate() {
                if *had && i < self.piece_peer_count.len() {
                    self.piece_peer_count[i] = self.piece_peer_count[i].saturating_sub(1);
                }
            }
            let have = expand_bitfield(bitfield, self.total_pieces);
            for (i, has) in have.iter().enumerate() {
                if *has && i < self.piece_peer_count.len() {
                    self.piece_peer_count[i] += 1;
                }
            }
            existing.have_pieces = have;
            existing.raw_bitfield = bitfield.to_vec();
            existing.last_updated = Instant::now();
        } else {
            let have = expand_bitfield(bitfield, self.total_pieces);
            for (i, has) in have.iter().enumerate() {
                if *has && i < self.piece_peer_count.len() {
                    self.piece_peer_count[i] += 1;
                }
            }
            self.peers.insert(peer_id.to_string(), PeerBitfieldEntry {
                peer_id: peer_id.to_string(),
                have_pieces: have,
                raw_bitfield: bitfield.to_vec(),
                last_updated: Instant::now(),
            });
        }
    }

    pub fn remove_peer(&mut self, peer_id: &str) {
        if let Some(entry) = self.peers.remove(peer_id) {
            for (i, had) in entry.have_pieces.iter().enumerate() {
                if *had && i < self.piece_peer_count.len() {
                    self.piece_peer_count[i] = self.piece_peer_count[i].saturating_sub(1);
                }
            }
        }
    }

    pub fn peers_having_piece(&self, piece_index: u32) -> Vec<String> {
        let idx = piece_index as usize;
        self.peers.iter()
            .filter(|(_, e)| idx < e.have_pieces.len() && e.have_pieces[idx])
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn peer_has_piece(&self, peer_id: &str, piece_index: u32) -> bool {
        let idx = piece_index as usize;
        self.peers.get(peer_id)
            .map(|e| idx < e.have_pieces.len() && e.have_pieces[idx])
            .unwrap_or(false)
    }

    pub fn piece_frequencies(&self) -> Vec<usize> {
        self.piece_peer_count.clone()
    }

    pub fn should_enter_endgame(&self, threshold: u32, completed: &[bool]) -> bool {
        let missing: usize = completed.iter()
            .take(self.total_pieces as usize)
            .filter(|c| !**c)
            .count();
        missing > 0 && missing as u32 <= threshold
    }

    pub fn missing_pieces(&self, completed: &[bool]) -> Vec<u32> {
        completed.iter()
            .enumerate()
            .take(self.total_pieces as usize)
            .filter(|(_, c)| !**c)
            .map(|(i, _)| i as u32)
            .collect()
    }

    pub fn stats(&self, completed: Option<&[bool]>) -> PeerTrackerStats {
        let total_have: usize = self.piece_peer_count.iter().sum();
        let avg = if self.peers.is_empty() { 0.0 } else { total_have as f64 / self.peers.len() as f64 };
        let rarest = self.piece_peer_count.iter().filter(|&&c| c == 1).count();

        let is_endgame = completed.map_or(false, |c| self.should_enter_endgame(20, c));

        PeerTrackerStats {
            peer_count: self.peers.len(),
            tracked_pieces: self.total_pieces,
            avg_pieces_per_peer: avg,
            rarest_piece_count: rarest,
            is_endgame,
        }
    }

    pub fn get_peer_bitfield_raw(&self, peer_id: &str) -> Option<&[u8]> {
        self.peers.get(peer_id).map(|e| e.raw_bitfield.as_slice())
    }

    pub fn get_peer_bitfield_or_empty(&self, peer_id: &str) -> Vec<u8> {
        self.get_peer_bitfield_raw(peer_id).map(|b| b.to_vec()).unwrap_or_default()
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }
}

fn expand_bitfield(bitfield: &[u8], total_pieces: u32) -> Vec<bool> {
    let mut result = vec![false; total_pieces as usize];
    for (i, &byte) in bitfield.iter().enumerate() {
        for bit in 0..8u32 {
            let piece_idx = (i as u32) * 8 + (7 - bit);
            if piece_idx >= total_pieces { continue; }
            if byte & (1 << bit) != 0 {
                result[piece_idx as usize] = true;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bf(pieces: u32, indices: &[u32]) -> Vec<u8> {
        let len = ((pieces as usize) + 7) / 8;
        let mut bf = vec![0u8; len];
        for &idx in indices {
            if idx < pieces {
                bf[(idx / 8) as usize] |= 1 << (7 - (idx % 8));
            }
        }
        bf
    }

    #[test]
    fn test_create_and_update_bitfield() {
        let mut tracker = PeerBitfieldTracker::new(10);
        assert_eq!(tracker.peer_count(), 0);

        let bf = make_bf(10, &[0, 2, 5, 7]);
        tracker.update_peer_bitfield("peer_a", &bf);

        assert_eq!(tracker.peer_count(), 1);
        assert!(tracker.peer_has_piece("peer_a", 0));
        assert!(tracker.peer_has_piece("peer_a", 2));
        assert!(!tracker.peer_has_piece("peer_a", 1));
        assert!(!tracker.peer_has_piece("peer_a", 9));

        let freqs = tracker.piece_frequencies();
        assert_eq!(freqs[0], 1);
        assert_eq!(freqs[2], 1);
        assert_eq!(freqs[1], 0);
    }

    #[test]
    fn test_remove_peer_updates_counts() {
        let mut tracker = PeerBitfieldTracker::new(6);
        let bf = make_bf(6, &[0, 1, 2]);
        tracker.update_peer_bitfield("p1", &bf);

        let bf2 = make_bf(6, &[2, 3, 4]);
        tracker.update_peer_bitfield("p2", &bf2);

        assert_eq!(tracker.piece_frequencies()[2], 2, "piece 2 should be owned by 2 peers");

        tracker.remove_peer("p1");
        assert_eq!(tracker.peer_count(), 1);
        assert_eq!(tracker.piece_frequencies()[2], 1, "after remove p1, piece 2 count drops to 1");
        assert_eq!(tracker.piece_frequencies()[0], 0, "after remove p1, piece 0 count drops to 0");
    }

    #[test]
    fn test_peers_having_piece_correct() {
        let mut tracker = PeerBitfieldTracker::new(8);
        tracker.update_peer_bitfield("a", &make_bf(8, &[0, 3, 5]));
        tracker.update_peer_bitfield("b", &make_bf(8, &[3, 5, 7]));
        tracker.update_peer_bitfield("c", &make_bf(8, &[1, 3]));

        let owners = tracker.peers_having_piece(3);
        assert_eq!(owners.len(), 3, "piece 3 owned by all 3 peers");

        let owners0 = tracker.peers_having_piece(0);
        assert_eq!(owners0, vec!["a".to_string()], "only peer a has piece 0");

        let owners6 = tracker.peers_having_piece(6);
        assert!(owners6.is_empty(), "no one has piece 6");
    }

    #[test]
    fn test_piece_frequencies_distribution() {
        let mut tracker = PeerBitfieldTracker::new(5);
        tracker.update_peer_bitfield("p1", &make_bf(5, &[0, 1, 2, 3, 4]));
        tracker.update_peer_bitfield("p2", &make_bf(5, &[0, 2, 4]));

        let freqs = tracker.piece_frequencies();
        assert_eq!(freqs, vec![2, 1, 2, 1, 2]);

        let stats = tracker.stats(None);
        assert_eq!(stats.peer_count, 2);
        assert!((stats.avg_pieces_per_peer - 4.0).abs() < 0.01);
        assert_eq!(stats.rarest_piece_count, 2, "pieces with freq=1 are rarest");
    }

    #[test]
    fn test_should_enter_endgame_threshold() {
        let tracker = PeerBitfieldTracker::new(100);
        let completed_all_false = vec![false; 100];

        assert!(!tracker.should_enter_endgame(20, &completed_all_false), "100 missing > 20 threshold");

        let mut mostly_done = vec![true; 95];
        mostly_done.resize(100, false);
        assert!(tracker.should_enter_endgame(20, &mostly_done), "5 missing ≤ 20 → endgame");
    }

    #[test]
    fn test_missing_pieces_excludes_completed() {
        let tracker = PeerBitfieldTracker::new(8);
        let completed = vec![true, false, true, false, true, false, true, false];

        let missing = tracker.missing_pieces(&completed);
        assert_eq!(missing, vec![1, 3, 5, 7]);
    }

    #[test]
    fn test_stats_reasonable_values() {
        let mut tracker = PeerBitfieldTracker::new(20);
        tracker.update_peer_bitfield("x", &make_bf(20, &[0, 5, 10, 15]));

        let stats = tracker.stats(None);
        assert_eq!(stats.peer_count, 1);
        assert_eq!(stats.tracked_pieces, 20);
        assert_eq!(stats.rarest_piece_count, 4);
    }

    #[test]
    fn test_empty_tracker_no_crash() {
        let tracker = PeerBitfieldTracker::new(50);
        assert_eq!(tracker.peer_count(), 0);
        assert_eq!(tracker.missing_pieces(&vec![false; 50]).len(), 50);
        assert!(tracker.peers_having_piece(0).is_empty());
        assert!(!tracker.peer_has_piece("nonexistent", 0));
        assert_eq!(tracker.get_peer_bitfield_raw("nope"), None);
        assert!(tracker.get_peer_bitfield_or_empty("nope").is_empty());

        let stats = tracker.stats(Some(&vec![false; 50]));
        assert_eq!(stats.peer_count, 0);
        assert!(!stats.is_endgame, "50 missing > 20 threshold");
    }

    #[test]
    fn test_reupdate_peer_replaces_old_data() {
        let mut tracker = PeerBitfieldTracker::new(6);
        tracker.update_peer_bitfield("p", &make_bf(6, &[0, 1, 2]));

        assert_eq!(tracker.piece_frequencies()[0], 1);

        tracker.update_peer_bitfield("p", &make_bf(6, &[3, 4, 5]));

        assert_eq!(tracker.piece_frequencies()[0], 0, "old piece 0 no longer counted");
        assert_eq!(tracker.piece_frequencies()[3], 1, "new piece 3 now counted");
        assert_eq!(tracker.peer_count(), 1, "still only 1 peer");
    }
}

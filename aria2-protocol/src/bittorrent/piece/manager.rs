use sha1::Digest;

pub struct PieceManager {
    num_pieces: u32,
    piece_length: u32,
    total_size: u64,
    completed: Vec<bool>,
    downloaded_bytes_per_piece: Vec<u64>,
    piece_hashes: Vec<[u8; 20]>,
}

impl PieceManager {
    pub fn new(
        num_pieces: u32,
        piece_length: u32,
        total_size: u64,
        hashes: Vec<[u8; 20]>,
    ) -> Self {
        assert_eq!(num_pieces as usize, hashes.len());
        Self {
            num_pieces,
            piece_length,
            total_size,
            completed: vec![false; num_pieces as usize],
            downloaded_bytes_per_piece: vec![0; num_pieces as usize],
            piece_hashes: hashes,
        }
    }

    pub fn piece_size(&self, index: u32) -> u32 {
        if index >= self.num_pieces - 1 {
            let remainder = (self.total_size % self.piece_length as u64) as u64;
            if remainder > 0 { remainder as u32 } else { self.piece_length }
        } else {
            self.piece_length
        }
    }

    pub fn is_completed(&self, index: u32) -> bool {
        (index as usize) < self.completed.len() && self.completed[index as usize]
    }

    pub fn mark_piece_downloaded(&mut self, index: u32, bytes: u64) {
        if (index as usize) < self.downloaded_bytes_per_piece.len() {
            self.downloaded_bytes_per_piece[index as usize] += bytes;
        }
    }

    pub fn mark_piece_complete(&mut self, index: u32) {
        if (index as usize) < self.completed.len() {
            self.completed[index as usize] = true;
        }
    }

    pub fn verify_piece_hash(&self, index: u32, data: &[u8]) -> bool {
        use sha1::Sha1;
        if (index as usize) >= self.piece_hashes.len() { return false; }
        let hash = Sha1::digest(data);
        hash.as_slice() == &self.piece_hashes[index as usize]
    }

    pub fn completed_pieces(&self) -> u32 {
        self.completed.iter().filter(|&&c| c).count() as u32
    }

    pub fn total_progress(&self) -> f64 {
        if self.total_size == 0 { return 100.0; }
        let downloaded: u64 = self.downloaded_bytes_per_piece.iter().sum();
        downloaded as f64 / self.total_size as f64 * 100.0
    }

    pub fn num_pieces(&self) -> u32 { self.num_pieces }
    pub fn piece_length(&self) -> u32 { self.piece_length }
    pub fn total_size(&self) -> u64 { self.total_size }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manager_creation() {
        let hashes: Vec<[u8; 20]> = (0..3).map(|_| {
            let h = [0u8; 20]; h
        }).collect();
        let mgr = PieceManager::new(3, 512, 1024, hashes);
        assert_eq!(mgr.num_pieces(), 3);
        assert_eq!(mgr.piece_length(), 512);
        assert_eq!(mgr.total_size(), 1024);
    }

    #[test]
    fn test_last_piece_size() {
        let hashes: Vec<[u8; 20]> = (0..3).map(|_| {
            let h = [0u8; 20]; h
        }).collect();
        let mgr = PieceManager::new(3, 512, 1100, hashes);
        assert_eq!(mgr.piece_size(0), 512);
        assert_eq!(mgr.piece_size(1), 512);
        assert_eq!(mgr.piece_size(2), 76);
    }

    #[test]
    fn test_mark_and_verify() {
        let hashes: Vec<[u8; 20]> = (0..2).map(|_| {
            let h = [0u8; 20]; h
        }).collect();
        let mut mgr = PieceManager::new(2, 100, 150, hashes);
        assert!(!mgr.is_completed(0));

        mgr.mark_piece_downloaded(0, 50);
        assert!(!mgr.is_completed(0));

        mgr.mark_piece_complete(0);
        assert!(mgr.is_completed(0));
        assert_eq!(mgr.completed_pieces(), 1);

        mgr.mark_piece_complete(1);
        assert_eq!(mgr.completed_pieces(), 2);
    }

    #[test]
    fn test_total_progress() {
        let hashes: Vec<[u8; 20]> = (0..4).map(|_| {
            let h = [0u8; 20]; h
        }).collect();
        let mut mgr = PieceManager::new(4, 256, 800, hashes);
        assert_eq!(mgr.total_progress(), 0.0);

        mgr.mark_piece_downloaded(0, 256);
        mgr.mark_piece_downloaded(1, 256);
        assert!((mgr.total_progress() - 64.0).abs() < 0.01);
    }
}

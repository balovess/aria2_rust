//! BEP 9 metadata piece collection.

/// Collect BEP 9 metadata pieces and assemble them in wire order.
pub struct MetadataCollector {
    total_size: u64,
    collected: Vec<Option<Vec<u8>>>,
    piece_size: u32,
}

impl MetadataCollector {
    pub fn new(total_size: u64, piece_size: u32) -> Self {
        let num_pieces = total_size.div_ceil(piece_size as u64) as usize;
        Self {
            total_size,
            collected: vec![None; num_pieces],
            piece_size,
        }
    }

    pub fn add_piece(&mut self, piece_idx: u32, data: &[u8]) -> bool {
        let idx = piece_idx as usize;
        if idx >= self.collected.len() || self.collected[idx].is_some() {
            return false;
        }
        let offset = u64::from(piece_idx) * u64::from(self.piece_size);
        let expected_len = self
            .total_size
            .saturating_sub(offset)
            .min(u64::from(self.piece_size));
        if data.len() as u64 != expected_len {
            return false;
        }
        self.collected[idx] = Some(data.to_vec());
        true
    }

    pub fn is_complete(&self) -> bool {
        self.collected.iter().all(Option::is_some)
    }

    pub fn assemble(&self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let mut result = Vec::with_capacity(self.total_size as usize);
        for piece in &self.collected {
            result.extend(piece.as_ref().expect("complete metadata has every piece"));
        }
        Some(result)
    }

    pub fn into_bytes(self) -> Option<Vec<u8>> {
        if self.collected.iter().any(Option::is_none) {
            return None;
        }
        let mut result = Vec::with_capacity(self.total_size as usize);
        for piece in self.collected {
            result.extend(piece.expect("complete metadata has every piece"));
        }
        Some(result)
    }

    pub fn progress(&self) -> f64 {
        let done = self
            .collected
            .iter()
            .filter(|piece| piece.is_some())
            .count();
        if self.collected.is_empty() {
            0.0
        } else {
            done as f64 / self.collected.len() as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MetadataCollector;

    #[test]
    fn rejects_wrong_piece_lengths() {
        let mut collector = MetadataCollector::new(3, 2);
        assert!(!collector.add_piece(0, b"x"));
        assert!(collector.add_piece(0, b"ab"));
        assert!(collector.add_piece(1, b"c"));
        assert_eq!(collector.assemble(), Some(b"abc".to_vec()));
    }

    #[test]
    fn reports_empty_collection_as_complete() {
        let collector = MetadataCollector::new(0, 16 * 1024);
        assert!(collector.is_complete());
        assert_eq!(collector.progress(), 0.0);
        assert_eq!(collector.into_bytes(), Some(Vec::new()));
    }
}

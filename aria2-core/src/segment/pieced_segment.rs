use super::bitfield::Bitfield;
use crate::error::Result;

pub struct PiecedSegment {
    piece_index: usize,
    piece_length: u64,
    bitfield: Bitfield,
}

impl PiecedSegment {
    pub fn new(piece_index: usize, piece_length: u64, num_blocks: usize) -> Self {
        PiecedSegment {
            piece_index,
            piece_length,
            bitfield: Bitfield::new(num_blocks),
        }
    }

    pub fn piece_index(&self) -> usize {
        self.piece_index
    }

    pub fn piece_length(&self) -> u64 {
        self.piece_length
    }

    pub fn num_blocks(&self) -> usize {
        self.bitfield.len()
    }

    pub fn is_completed(&self) -> bool {
        self.bitfield.is_all_set()
    }

    pub fn mark_block_completed(&mut self, block_index: usize) -> Result<()> {
        self.bitfield.set(block_index)?;
        Ok(())
    }

    pub fn mark_block_incomplete(&mut self, block_index: usize) -> Result<()> {
        self.bitfield.unset(block_index)?;
        Ok(())
    }

    pub fn is_block_completed(&self, block_index: usize) -> bool {
        self.bitfield.get(block_index)
    }

    pub fn get_next_missing_block(&self) -> Option<usize> {
        self.bitfield.find_first_unset()
    }

    pub fn get_completed_blocks(&self) -> Vec<usize> {
        let mut completed = Vec::new();
        for i in 0..self.bitfield.len() {
            if self.bitfield.get(i) {
                completed.push(i);
            }
        }
        completed
    }

    pub fn get_missing_blocks(&self) -> Vec<usize> {
        let mut missing = Vec::new();
        for i in 0..self.bitfield.len() {
            if !self.bitfield.get(i) {
                missing.push(i);
            }
        }
        missing
    }

    pub fn completed_blocks_count(&self) -> usize {
        self.bitfield.count_set_bits()
    }

    pub fn missing_blocks_count(&self) -> usize {
        self.bitfield.count_unset_bits()
    }

    pub fn progress(&self) -> f64 {
        let total = self.bitfield.len();
        if total == 0 {
            0.0
        } else {
            (self.completed_blocks_count() as f64 / total as f64) * 100.0
        }
    }
}

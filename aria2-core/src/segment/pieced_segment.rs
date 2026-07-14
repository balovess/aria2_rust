use crate::error::{Aria2Error, Result};
use aria2_protocol::bittorrent::piece::bitfield::Bitfield;

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
        self.bitfield.set(block_index).ok_or_else(|| {
            Aria2Error::DownloadFailed(format!(
                "Block index out of range: {} >= {}",
                block_index,
                self.bitfield.len()
            ))
        })
    }

    pub fn mark_block_incomplete(&mut self, block_index: usize) -> Result<()> {
        self.bitfield.clear(block_index).ok_or_else(|| {
            Aria2Error::DownloadFailed(format!(
                "Block index out of range: {} >= {}",
                block_index,
                self.bitfield.len()
            ))
        })
    }

    pub fn is_block_completed(&self, block_index: usize) -> bool {
        self.bitfield.test(block_index)
    }

    pub fn get_next_missing_block(&self) -> Option<usize> {
        self.bitfield.find_first_clear()
    }

    pub fn get_completed_blocks(&self) -> Vec<usize> {
        self.bitfield.iter_set().collect()
    }

    pub fn get_missing_blocks(&self) -> Vec<usize> {
        self.bitfield.iter_clear().collect()
    }

    pub fn completed_blocks_count(&self) -> usize {
        self.bitfield.count_set()
    }

    pub fn missing_blocks_count(&self) -> usize {
        self.bitfield.count_clear()
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

//! Trait implementations for Piece.

use super::bitfield::BlockBitfield;
use super::piece_impl::{DEFAULT_BLOCK_LENGTH, Piece};

impl Clone for Piece {
    fn clone(&self) -> Self {
        Piece {
            identity: self.identity,
            completed: self.completed.clone(),
            in_use: self.in_use.clone(),
            users: self.users.clone(),
            hash_type: self.hash_type.clone(),
            hash_state: None, // Hash state is not cloned; it will be re-initialized if needed
            next_begin: 0,
            index: self.index,
            length: self.length,
            block_length: self.block_length,
            used_by_segment: self.used_by_segment,
        }
    }
}

impl PartialEq for Piece {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl Eq for Piece {}

impl PartialOrd for Piece {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Piece {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.index.cmp(&other.index)
    }
}

impl std::fmt::Debug for Piece {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Piece")
            .field("index", &self.index)
            .field("length", &self.length)
            .field("block_length", &self.block_length)
            .field("num_blocks", &self.count_blocks())
            .field("completed_blocks", &self.count_completed_blocks())
            .field("missing_blocks", &self.count_missing_blocks())
            .field("users", &self.users.len())
            .field("used_by_segment", &self.used_by_segment)
            .field("hash_type", &self.hash_type)
            .field("hash_calculated", &self.is_hash_calculated())
            .finish()
    }
}

impl std::fmt::Display for Piece {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "piece: index={}, length={}", self.index, self.length)
    }
}

impl Default for Piece {
    fn default() -> Self {
        Piece {
            identity: 0,
            completed: BlockBitfield::new(0),
            in_use: BlockBitfield::new(0),
            users: Vec::new(),
            hash_type: None,
            hash_state: None,
            next_begin: 0,
            index: 0,
            length: 0,
            block_length: DEFAULT_BLOCK_LENGTH,
            used_by_segment: false,
        }
    }
}

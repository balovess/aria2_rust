//! Self-contained bitfield for block-level tracking.
//!
//! Uses MSB-first bit ordering (bit 0 is the MSB of byte 0), matching C++ aria2.
//! This module provides `BlockBitfield`, an internal data structure used by `Piece`
//! to track completed and in-use blocks.

/// A simple bitfield for tracking block completion/in-use status.
///
/// Uses MSB-first bit ordering (bit 0 is the MSB of byte 0), matching C++ aria2.
#[derive(Clone, Debug)]
pub(crate) struct BlockBitfield {
    pub(crate) data: Vec<u8>,
    pub(crate) num_bits: usize,
}

impl BlockBitfield {
    pub(crate) fn new(num_bits: usize) -> Self {
        let num_bytes = num_bits.div_ceil(8);
        BlockBitfield {
            data: vec![0u8; num_bytes],
            num_bits,
        }
    }

    pub(crate) fn test(&self, index: usize) -> bool {
        if index >= self.num_bits {
            return false;
        }
        let byte = index / 8;
        let bit = 7 - (index % 8);
        (self.data[byte] & (1 << bit)) != 0
    }

    pub(crate) fn set(&mut self, index: usize) {
        if index >= self.num_bits {
            return;
        }
        let byte = index / 8;
        let bit = 7 - (index % 8);
        self.data[byte] |= 1 << bit;
    }

    pub(crate) fn unset(&mut self, index: usize) {
        if index >= self.num_bits {
            return;
        }
        let byte = index / 8;
        let bit = 7 - (index % 8);
        self.data[byte] &= !(1 << bit);
    }

    pub(crate) fn len(&self) -> usize {
        self.num_bits
    }

    pub(crate) fn count_set(&self) -> usize {
        self.data
            .iter()
            .map(|b| b.count_ones() as usize)
            .sum::<usize>()
            - if !self.num_bits.is_multiple_of(8) {
                // Count extra bits in the last byte that are beyond num_bits
                let extra = 8 - (self.num_bits % 8);
                let last = *self.data.last().unwrap_or(&0);
                let mask = (1u8 << extra) - 1;
                (last & mask).count_ones() as usize
            } else {
                0
            }
    }

    pub(crate) fn count_clear(&self) -> usize {
        self.num_bits.saturating_sub(self.count_set())
    }

    /// Set all bits.
    pub(crate) fn set_all(&mut self) {
        for byte in &mut self.data {
            *byte = 0xFF;
        }
        // Clear trailing bits beyond num_bits
        if !self.num_bits.is_multiple_of(8) {
            let extra = 8 - (self.num_bits % 8);
            if let Some(last) = self.data.last_mut() {
                *last &= !((1u8 << extra) - 1);
            }
        }
    }

    /// Clear a bit at index.
    pub(crate) fn clear(&mut self, index: usize) {
        self.unset(index);
    }

    /// Find the first clear (unset) bit, returns None if all are set.
    pub(crate) fn find_first_clear(&self) -> Option<usize> {
        (0..self.num_bits).find(|&i| !self.test(i))
    }

    /// Create from existing byte data.
    pub(crate) fn from_bytes(data: &[u8], num_bits: usize) -> Self {
        let num_bytes = num_bits.div_ceil(8);
        let mut bf = BlockBitfield {
            data: vec![0u8; num_bytes],
            num_bits,
        };
        let copy_len = std::cmp::min(data.len(), num_bytes);
        bf.data[..copy_len].copy_from_slice(&data[..copy_len]);
        bf
    }

    /// Create with all bits set.
    pub(crate) fn all_set(num_bits: usize) -> Self {
        let mut bf = BlockBitfield::new(num_bits);
        bf.set_all();
        bf
    }

    /// Returns true if all bits are set.
    pub(crate) fn is_all_set(&self) -> bool {
        self.count_set() == self.num_bits
    }

    /// Returns the raw byte slice.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Clear all bits.
    pub(crate) fn clear_all(&mut self) {
        for byte in &mut self.data {
            *byte = 0;
        }
    }
}

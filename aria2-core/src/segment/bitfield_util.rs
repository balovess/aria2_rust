//! Shared bitfield manipulation utilities (MSB-first byte ordering).
//!
//! Provides the canonical `test_bit` function used across the BitTorrent
//! subsystem for querying bit states in raw bitfield byte slices.
//!
//! # Bit layout (BitTorrent standard, MSB-first)
//!
//! ```text
//! byte[0]: bit 0 (MSB) .. bit 7 (LSB)
//! byte[1]: bit 8 (MSB) .. bit 15 (LSB)
//! ...
//! ```
//!
//! This matches the C++ aria2 `bitfield::test()` convention.

/// Test whether the bit at `index` is set in a raw bitfield of `nbits` total
/// bits, using MSB-first byte ordering (BitTorrent standard).
///
/// Returns `false` if:
/// - `index >= nbits` (logical out-of-bounds), or
/// - the bitfield byte slice is too short to contain the relevant byte.
#[inline]
pub fn test_bit(bitfield: &[u8], nbits: usize, index: usize) -> bool {
    if index >= nbits {
        return false;
    }
    let byte_index = index / 8;
    if byte_index >= bitfield.len() {
        return false;
    }
    let bit_offset = index % 8;
    (bitfield[byte_index] & (1 << (7 - bit_offset))) != 0
}

/// Clear (unset) the bit at `index` in a mutable raw bitfield of `nbits` total
/// bits, using MSB-first byte ordering.
///
/// Does nothing if `index >= nbits` or the bitfield is too short.
#[inline]
pub fn clear_bit(bitfield: &mut [u8], nbits: usize, index: usize) {
    if index >= nbits {
        return;
    }
    let byte_index = index / 8;
    if byte_index >= bitfield.len() {
        return;
    }
    let bit_offset = index % 8;
    bitfield[byte_index] &= !(1 << (7 - bit_offset));
}

/// Set the bit at `index` in a mutable raw bitfield of `nbits` total
/// bits, using MSB-first byte ordering.
///
/// Does nothing if `index >= nbits` or the bitfield is too short.
#[inline]
pub fn set_bit(bitfield: &mut [u8], nbits: usize, index: usize) {
    if index >= nbits {
        return;
    }
    let byte_index = index / 8;
    if byte_index >= bitfield.len() {
        return;
    }
    let bit_offset = index % 8;
    bitfield[byte_index] |= 1 << (7 - bit_offset);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bit_single_byte_all_set() {
        let bf = [0xFF];
        for i in 0..8 {
            assert!(test_bit(&bf, 8, i), "bit {} should be set", i);
        }
    }

    #[test]
    fn test_bit_single_byte_none_set() {
        let bf = [0x00];
        for i in 0..8 {
            assert!(!test_bit(&bf, 8, i), "bit {} should not be set", i);
        }
    }

    #[test]
    fn test_bit_msb_first_ordering() {
        // 0b11000000 = bits 0 and 1 set (MSB first)
        let bf = [0xC0];
        assert!(test_bit(&bf, 8, 0), "bit 0 (MSB) should be set");
        assert!(test_bit(&bf, 8, 1), "bit 1 should be set");
        assert!(!test_bit(&bf, 8, 2), "bit 2 should not be set");
        assert!(!test_bit(&bf, 8, 7), "bit 7 (LSB) should not be set");
    }

    #[test]
    fn test_bit_alternating_pattern() {
        // 0b10101010 = bits 0, 2, 4, 6 set
        let bf = [0xAA];
        assert!(test_bit(&bf, 8, 0));
        assert!(!test_bit(&bf, 8, 1));
        assert!(test_bit(&bf, 8, 2));
        assert!(!test_bit(&bf, 8, 3));
        assert!(test_bit(&bf, 8, 4));
        assert!(!test_bit(&bf, 8, 5));
        assert!(test_bit(&bf, 8, 6));
        assert!(!test_bit(&bf, 8, 7));
    }

    #[test]
    fn test_bit_multi_byte() {
        let bf = [0xFF, 0xFF];
        for i in 0..16 {
            assert!(test_bit(&bf, 16, i), "bit {} should be set", i);
        }
    }

    #[test]
    fn test_bit_cross_byte_boundary() {
        // First byte: 0b00000001 (only bit 7 set)
        // Second byte: 0b10000000 (only bit 8 set)
        let bf = [0x01, 0x80];
        assert!(!test_bit(&bf, 16, 0));
        assert!(!test_bit(&bf, 16, 6));
        assert!(test_bit(&bf, 16, 7), "bit 7 should be set");
        assert!(test_bit(&bf, 16, 8), "bit 8 should be set");
        assert!(!test_bit(&bf, 16, 9));
    }

    #[test]
    fn test_bit_index_exceeds_nbits() {
        let bf = [0xFF];
        // nbits = 4, so index 4 should return false even though byte exists
        assert!(!test_bit(&bf, 4, 4));
        assert!(!test_bit(&bf, 4, 5));
        // index < nbits should work
        assert!(test_bit(&bf, 4, 3));
    }

    #[test]
    fn test_bit_byte_out_of_range() {
        let bf = [0xFF]; // only 1 byte
        assert!(!test_bit(&bf, 16, 8), "byte index 1 out of range");
    }

    #[test]
    fn test_bit_empty_bitfield() {
        let bf: [u8; 0] = [];
        assert!(!test_bit(&bf, 0, 0));
        assert!(!test_bit(&bf, 8, 0));
    }

    #[test]
    fn test_bit_single_bit_patterns() {
        for bit_pos in 0..8 {
            let mut bf = [0u8; 1];
            let bit_in_byte = 7 - bit_pos;
            bf[0] = 1 << bit_in_byte;
            assert!(test_bit(&bf, 8, bit_pos), "bit {} should be set", bit_pos);
            for other in 0..8 {
                if other != bit_pos {
                    assert!(
                        !test_bit(&bf, 8, other),
                        "bit {} should not be set (only {} is)",
                        other,
                        bit_pos
                    );
                }
            }
        }
    }

    #[test]
    fn test_bit_out_of_bounds_index() {
        let bf: &[u8] = &[0xFF];
        assert!(!test_bit(bf, 8, 8)); // index == nbits
        assert!(!test_bit(bf, 8, 100));
    }

    #[test]
    fn test_bit_short_bitfield() {
        // bitfield slice too short for the index
        let bf: &[u8] = &[0xFF]; // only 1 byte
        assert!(!test_bit(bf, 16, 8)); // would need byte 1, which is missing
    }
}

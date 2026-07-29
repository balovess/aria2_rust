//! Bit manipulation helpers (MSB-first ordering, matching C++ aria2).

/// Set bit at `index` in `bitfield` (MSB-first: bit 0 is the MSB of byte 0).
#[inline]
pub(crate) fn bf_set(bitfield: &mut [u8], index: usize) {
    let byte = index / 8;
    let bit = 7 - (index % 8);
    if byte < bitfield.len() {
        bitfield[byte] |= 1 << bit;
    }
}

/// Clear bit at `index` in `bitfield` (MSB-first).
#[inline]
pub(crate) fn bf_unset(bitfield: &mut [u8], index: usize) {
    let byte = index / 8;
    let bit = 7 - (index % 8);
    if byte < bitfield.len() {
        bitfield[byte] &= !(1 << bit);
    }
}

/// Count set bits in a bitfield up to `num_bits` bits.
pub(crate) fn bf_count_set(bitfield: &[u8], num_bits: usize) -> usize {
    if num_bits == 0 {
        return 0;
    }
    let full_bytes = num_bits / 8;
    let remaining_bits = num_bits % 8;
    let mut count: usize = bitfield[..full_bytes]
        .iter()
        .map(|b| b.count_ones() as usize)
        .sum();
    if remaining_bits > 0 && full_bytes < bitfield.len() {
        let last_byte = bitfield[full_bytes];
        let mask = !((1u8 << (8 - remaining_bits)) - 1);
        count += (last_byte & mask).count_ones() as usize;
    }
    count
}

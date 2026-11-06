//! Compressed bitfield storage for BitTorrent piece tracking.
//!
//! Uses 1 bit per piece instead of 1 byte (`Vec<bool>`), providing 8x memory reduction.
//! This is critical for large torrents with thousands of pieces.

/// A memory-efficient bitfield using 1 bit per element.
///
/// # Memory Efficiency
///
/// For a torrent with 10,000 pieces:
/// - `Vec<bool>`: 10,000 bytes
/// - `Bitfield`: 1,250 bytes (8x reduction)
///
/// # Examples
///
/// ```
/// use aria2_protocol::bittorrent::piece::bitfield::Bitfield;
///
/// let mut bf = Bitfield::new(100);
/// bf.set(5).unwrap();
/// assert!(bf.test(5));
/// assert!(!bf.test(6));
/// assert_eq!(bf.count_set(), 1);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitfield {
    /// Storage for bits, 8 bits per byte
    bits: Vec<u8>,
    /// Total number of bits
    num_bits: usize,
}

impl Bitfield {
    /// Create a new bitfield with all bits unset (false).
    ///
    /// # Arguments
    ///
    /// * `num_bits` - Number of bits to store
    ///
    /// # Examples
    ///
    /// ```
    /// use aria2_protocol::bittorrent::piece::bitfield::Bitfield;
    ///
    /// let bf = Bitfield::new(100);
    /// assert_eq!(bf.len(), 100);
    /// assert!(bf.is_all_clear());
    /// ```
    pub fn new(num_bits: usize) -> Self {
        let num_bytes = num_bits.div_ceil(8);
        Bitfield {
            bits: vec![0u8; num_bytes],
            num_bits,
        }
    }

    /// Create a bitfield with all bits set (true).
    ///
    /// # Examples
    ///
    /// ```
    /// use aria2_protocol::bittorrent::piece::bitfield::Bitfield;
    ///
    /// let bf = Bitfield::all_set(50);
    /// assert!(bf.is_all_set());
    /// assert_eq!(bf.count_set(), 50);
    /// ```
    pub fn all_set(num_bits: usize) -> Self {
        let num_bytes = num_bits.div_ceil(8);
        let mut bits = vec![0xFFu8; num_bytes];

        // Clear unused bits in the last byte
        let remaining_bits = num_bits % 8;
        if remaining_bits > 0 && num_bytes > 0 {
            let mask = ((1u8 << remaining_bits) - 1) << (8 - remaining_bits);
            bits[num_bytes - 1] = mask;
        }

        Bitfield { bits, num_bits }
    }

    /// Create a bitfield from raw bytes (e.g., from a peer's bitfield message).
    ///
    /// # Arguments
    ///
    /// * `data` - Raw byte data
    /// * `num_bits` - Number of valid bits
    ///
    /// # Examples
    ///
    /// ```
    /// use aria2_protocol::bittorrent::piece::bitfield::Bitfield;
    ///
    /// // Bitfield with bits 0 and 7 set: 0b10000001 = 0x81
    /// let bf = Bitfield::from_bytes(&[0x81], 8);
    /// assert!(bf.test(0));
    /// assert!(bf.test(7));
    /// ```
    pub fn from_bytes(data: &[u8], num_bits: usize) -> Self {
        let num_bytes = num_bits.div_ceil(8);
        let mut bits = vec![0u8; num_bytes];

        let copy_len = std::cmp::min(data.len(), num_bytes);
        if copy_len > 0 {
            bits[..copy_len].copy_from_slice(&data[..copy_len]);
        }

        // Clear unused bits in the last byte
        let remaining_bits = num_bits % 8;
        if remaining_bits > 0 && num_bytes > 0 {
            let mask = ((1u8 << remaining_bits) - 1) << (8 - remaining_bits);
            bits[num_bytes - 1] &= mask;
        }

        Bitfield { bits, num_bits }
    }

    /// Get the raw byte representation.
    ///
    /// This is useful for sending bitfield messages to peers.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    /// Get the total number of bits.
    pub fn len(&self) -> usize {
        self.num_bits
    }

    /// Check if the bitfield is empty.
    pub fn is_empty(&self) -> bool {
        self.num_bits == 0
    }

    /// Set a bit to true.
    ///
    /// # Errors
    ///
    /// Returns `None` if index is out of bounds.
    ///
    /// # Examples
    ///
    /// ```
    /// use aria2_protocol::bittorrent::piece::bitfield::Bitfield;
    ///
    /// let mut bf = Bitfield::new(10);
    /// assert!(bf.set(5).is_some());
    /// assert!(bf.set(100).is_none()); // Out of bounds
    /// ```
    pub fn set(&mut self, index: usize) -> Option<()> {
        if index >= self.num_bits {
            return None;
        }

        let byte_index = index / 8;
        let bit_offset = index % 8;
        self.bits[byte_index] |= 1 << (7 - bit_offset);
        Some(())
    }

    /// Clear a bit (set to false).
    ///
    /// # Errors
    ///
    /// Returns `None` if index is out of bounds.
    pub fn clear(&mut self, index: usize) -> Option<()> {
        if index >= self.num_bits {
            return None;
        }

        let byte_index = index / 8;
        let bit_offset = index % 8;
        self.bits[byte_index] &= !(1 << (7 - bit_offset));
        Some(())
    }

    /// Test if a bit is set.
    ///
    /// Returns `false` for out-of-bounds indices.
    ///
    /// # Examples
    ///
    /// ```
    /// use aria2_protocol::bittorrent::piece::bitfield::Bitfield;
    ///
    /// let mut bf = Bitfield::new(10);
    /// bf.set(3).unwrap();
    /// assert!(bf.test(3));
    /// assert!(!bf.test(4));
    /// assert!(!bf.test(100)); // Out of bounds returns false
    /// ```
    pub fn test(&self, index: usize) -> bool {
        if index >= self.num_bits {
            return false;
        }

        let byte_index = index / 8;
        let bit_offset = index % 8;
        (self.bits[byte_index] & (1 << (7 - bit_offset))) != 0
    }

    /// Count the number of set bits.
    ///
    /// # Performance
    ///
    /// Uses `count_ones()` intrinsic for fast bit counting.
    pub fn count_set(&self) -> usize {
        let mut count = 0;

        // Count full bytes
        let full_bytes = self.num_bits / 8;
        for i in 0..full_bytes {
            count += self.bits[i].count_ones() as usize;
        }

        // Count remaining bits in the last partial byte
        let remaining_bits = self.num_bits % 8;
        if remaining_bits > 0 && full_bytes < self.bits.len() {
            let last_byte = self.bits[full_bytes];
            for bit in 0..remaining_bits {
                if last_byte & (1 << (7 - bit)) != 0 {
                    count += 1;
                }
            }
        }

        count
    }

    /// Count the number of clear bits.
    pub fn count_clear(&self) -> usize {
        self.num_bits - self.count_set()
    }

    /// Check if all bits are set.
    pub fn is_all_set(&self) -> bool {
        self.count_set() == self.num_bits
    }

    /// Check if all bits are clear.
    pub fn is_all_clear(&self) -> bool {
        self.bits.iter().all(|&b| b == 0)
    }

    /// Find the first set bit.
    ///
    /// Returns `None` if no bits are set.
    pub fn find_first_set(&self) -> Option<usize> {
        for (byte_index, &byte) in self.bits.iter().enumerate() {
            if byte != 0 {
                for bit_offset in 0..8 {
                    let index = byte_index * 8 + bit_offset;
                    if index < self.num_bits && (byte & (1 << (7 - bit_offset))) != 0 {
                        return Some(index);
                    }
                }
            }
        }
        None
    }

    /// Find the first clear bit.
    ///
    /// Returns `None` if all bits are set.
    pub fn find_first_clear(&self) -> Option<usize> {
        for (byte_index, &byte) in self.bits.iter().enumerate() {
            // Check if this byte has any clear bits (considering num_bits)
            let start_bit = byte_index * 8;
            let end_bit = std::cmp::min(start_bit + 8, self.num_bits);

            for bit_offset in 0..(end_bit - start_bit) {
                let index = start_bit + bit_offset;
                if (byte & (1 << (7 - bit_offset))) == 0 {
                    return Some(index);
                }
            }
        }
        None
    }

    /// Find the next set bit after the given index.
    ///
    /// Returns `None` if no more set bits exist.
    pub fn find_next_set(&self, after: usize) -> Option<usize> {
        let start = after + 1;
        if start >= self.num_bits {
            return None;
        }

        let byte_index = start / 8;
        let bit_offset = start % 8;

        // Check remaining bits in the current byte
        let byte = self.bits[byte_index];
        for bit in bit_offset..8 {
            let index = byte_index * 8 + bit;
            if index < self.num_bits && (byte & (1 << (7 - bit))) != 0 {
                return Some(index);
            }
        }

        // Check subsequent bytes
        for (bi, &b) in self.bits.iter().enumerate().skip(byte_index + 1) {
            if b != 0 {
                for bit_offset in 0..8 {
                    let index = bi * 8 + bit_offset;
                    if index < self.num_bits && (b & (1 << (7 - bit_offset))) != 0 {
                        return Some(index);
                    }
                }
            }
        }

        None
    }

    /// Get an iterator over all set bit indices.
    pub fn iter_set(&self) -> SetBitIter<'_> {
        SetBitIter::new(self)
    }

    /// Get an iterator over all clear bit indices.
    pub fn iter_clear(&self) -> ClearBitIter<'_> {
        ClearBitIter::new(self)
    }

    /// Calculate memory usage in bytes.
    ///
    /// This is the actual storage size, not including struct overhead.
    pub fn memory_usage(&self) -> usize {
        self.bits.len()
    }

    /// Calculate memory usage if using `Vec<bool>` for the same number of bits.
    ///
    /// This is useful for comparing memory savings.
    pub fn vec_bool_memory_usage(&self) -> usize {
        self.num_bits
    }

    /// Calculate memory savings ratio compared to `Vec<bool>`.
    ///
    /// Returns the ratio of `Vec<bool>` memory to Bitfield memory.
    /// A value of 8.0 means Bitfield uses 8x less memory.
    pub fn memory_savings_ratio(&self) -> f64 {
        if self.bits.is_empty() {
            return 1.0;
        }
        self.num_bits as f64 / self.bits.len() as f64
    }

    /// Set all bits to true.
    pub fn set_all(&mut self) {
        self.bits.fill(0xFF);

        // Clear unused bits in the last byte
        let remaining_bits = self.num_bits % 8;
        if remaining_bits > 0 && !self.bits.is_empty() {
            let mask = ((1u8 << remaining_bits) - 1) << (8 - remaining_bits);
            let last_idx = self.bits.len() - 1;
            self.bits[last_idx] = mask;
        }
    }

    /// Clear all bits.
    pub fn clear_all(&mut self) {
        self.bits.fill(0);
    }

    /// Perform a bitwise AND with another bitfield.
    ///
    /// Both bitfields must have the same length.
    pub fn bitand_assign(&mut self, other: &Bitfield) {
        assert_eq!(self.num_bits, other.num_bits, "Bitfield lengths must match");
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            *a &= b;
        }
    }

    /// Perform a bitwise OR with another bitfield.
    ///
    /// Both bitfields must have the same length.
    pub fn bitor_assign(&mut self, other: &Bitfield) {
        assert_eq!(self.num_bits, other.num_bits, "Bitfield lengths must match");
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            *a |= b;
        }
    }

    /// Perform a bitwise XOR with another bitfield.
    ///
    /// Both bitfields must have the same length.
    pub fn bitxor_assign(&mut self, other: &Bitfield) {
        assert_eq!(self.num_bits, other.num_bits, "Bitfield lengths must match");
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            *a ^= b;
        }
    }
}

/// Iterator over set bit indices.
pub struct SetBitIter<'a> {
    bitfield: &'a Bitfield,
    current: usize,
}

impl<'a> SetBitIter<'a> {
    fn new(bitfield: &'a Bitfield) -> Self {
        SetBitIter {
            bitfield,
            current: 0,
        }
    }
}

impl<'a> Iterator for SetBitIter<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.current < self.bitfield.num_bits {
            if self.bitfield.test(self.current) {
                let result = self.current;
                self.current += 1;
                return Some(result);
            }
            self.current += 1;
        }
        None
    }
}

/// Iterator over clear bit indices.
pub struct ClearBitIter<'a> {
    bitfield: &'a Bitfield,
    current: usize,
}

impl<'a> ClearBitIter<'a> {
    fn new(bitfield: &'a Bitfield) -> Self {
        ClearBitIter {
            bitfield,
            current: 0,
        }
    }
}

impl<'a> Iterator for ClearBitIter<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.current < self.bitfield.num_bits {
            if !self.bitfield.test(self.current) {
                let result = self.current;
                self.current += 1;
                return Some(result);
            }
            self.current += 1;
        }
        None
    }
}

#[cfg(test)]
#[path = "bitfield_tests.rs"]
mod tests;

//! DHT node ID: 20-byte identifier with XOR distance operations.
//!
//! A `NodeId` wraps a `[u8; 20]` array and provides Kademlia-specific
//! operations: XOR distance computation, lexicographic comparison for
//! range checks, and bit manipulation for bucket splitting.

use std::cmp::Ordering;
use std::fmt;
use std::ops::BitXor;

use super::constants::ID_LENGTH;

/// A 20-byte DHT node identifier.
///
/// In Kademlia, node IDs are 160-bit keys. The XOR metric defines the
/// "distance" between two nodes: `d(A, B) = A XOR B`. Smaller distances
/// mean nodes are "closer" in the DHT keyspace.
///
/// # Layout
///
/// The bytes are stored in big-endian (network) order. Byte 0 is the
/// most significant, byte 19 is the least significant. This matches
/// the C++ implementation where `id_[0]` is compared first.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub [u8; ID_LENGTH]);

impl NodeId {
    /// All-zero node ID.
    pub const ZERO: NodeId = NodeId([0u8; ID_LENGTH]);

    /// All-one node ID (0xFF repeated).
    pub const MAX: NodeId = NodeId([0xFFu8; ID_LENGTH]);

    /// Create a `NodeId` from a byte slice. Panics if length != 20.
    pub fn from_slice(data: &[u8]) -> Self {
        assert_eq!(data.len(), ID_LENGTH, "NodeId must be exactly 20 bytes");
        let mut id = [0u8; ID_LENGTH];
        id.copy_from_slice(data);
        NodeId(id)
    }

    /// Generate a random node ID using the `rand` crate.
    pub fn random() -> Self {
        use rand::RngCore;
        let mut id = [0u8; ID_LENGTH];
        rand::thread_rng().fill_bytes(&mut id);
        NodeId(id)
    }

    /// Return the inner byte array by reference.
    pub fn as_bytes(&self) -> &[u8; ID_LENGTH] {
        &self.0
    }

    /// Compute XOR distance to another node ID.
    ///
    /// In Kademlia, distance = A XOR B. This is the fundamental metric
    /// for routing table placement and lookup operations.
    pub fn distance_to(&self, other: &NodeId) -> NodeId {
        let result: [u8; ID_LENGTH] = std::array::from_fn(|i| self.0[i] ^ other.0[i]);
        NodeId(result)
    }

    /// Return the prefix length (number of leading zero bits in the
    /// XOR distance between `self` and `other`).
    ///
    /// This determines which bucket a node belongs to. A prefix length
    /// of `n` means the first `n` bits of `self` and `other` are identical.
    pub fn common_prefix_len(&self, other: &NodeId) -> usize {
        let dist = self.distance_to(other);
        let mut count = 0usize;
        for &byte in &dist.0 {
            if byte == 0 {
                count += 8;
            } else {
                count += byte.leading_zeros() as usize;
                break;
            }
        }
        count
    }

    /// Get the bit value at the given position (0 = MSB of byte 0).
    ///
    /// Bit index 0 is the most significant bit of byte 0.
    /// Bit index 7 is the least significant bit of byte 0.
    /// Bit index 8 is the most significant bit of byte 1, etc.
    pub fn get_bit(&self, index: usize) -> bool {
        let byte_idx = index / 8;
        let bit_idx = 7 - (index % 8);
        if byte_idx >= ID_LENGTH {
            return false;
        }
        (self.0[byte_idx] >> bit_idx) & 1 == 1
    }

    /// Flip the bit at the given position.
    ///
    /// Used by bucket splitting to compute the midpoint of a range.
    /// C++: `bitfield::flipBit()`
    pub fn flip_bit(&mut self, index: usize) {
        let byte_idx = index / 8;
        let bit_idx = 7 - (index % 8);
        if byte_idx < ID_LENGTH {
            self.0[byte_idx] ^= 1 << bit_idx;
        }
    }

    /// Lexicographic comparison: returns true if `self` >= `min` and `self` <= `max`.
    ///
    /// This matches the C++ `DHTBucket::isInRange` which uses
    /// `std::lexicographical_compare` for range checks.
    pub fn is_in_range(&self, min: &NodeId, max: &NodeId) -> bool {
        self >= min && self <= max
    }

    /// Return the hex representation of this node ID.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl BitXor for NodeId {
    type Output = NodeId;

    fn bitxor(self, rhs: NodeId) -> NodeId {
        self.distance_to(&rhs)
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.to_hex())
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl Ord for NodeId {
    fn cmp(&self, other: &Self) -> Ordering {
        // Lexicographic comparison matching C++ memcmp behavior
        for i in 0..ID_LENGTH {
            match self.0[i].cmp(&other.0[i]) {
                Ordering::Equal => continue,
                other => return other,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for NodeId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl AsRef<[u8; ID_LENGTH]> for NodeId {
    fn as_ref(&self) -> &[u8; ID_LENGTH] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_id() {
        assert_eq!(NodeId::ZERO.0, [0u8; ID_LENGTH]);
    }

    #[test]
    fn max_id() {
        assert_eq!(NodeId::MAX.0, [0xFFu8; ID_LENGTH]);
    }

    #[test]
    fn from_slice() {
        let data = [0xABu8; ID_LENGTH];
        let id = NodeId::from_slice(&data);
        assert_eq!(id.0, [0xABu8; ID_LENGTH]);
    }

    #[test]
    #[should_panic]
    fn from_slice_wrong_length() {
        let data = [0u8; 10];
        let _ = NodeId::from_slice(&data);
    }

    #[test]
    fn xor_distance() {
        let a = NodeId::from_slice(&[0xFF; ID_LENGTH]);
        let b = NodeId::ZERO;
        let dist = a.distance_to(&b);
        assert_eq!(dist.0, [0xFF; ID_LENGTH]);

        let c = NodeId::ZERO;
        let d = NodeId::ZERO;
        assert_eq!(c.distance_to(&d).0, [0; ID_LENGTH]);
    }

    #[test]
    fn common_prefix_len_identical() {
        let a = NodeId::from_slice(&[0xFF; ID_LENGTH]);
        assert_eq!(a.common_prefix_len(&a), 160);
    }

    #[test]
    fn common_prefix_len_diff_first_bit() {
        // 0x80 = 1000_0000, 0x00 = 0000_0000 -> differ at bit 0
        let mut a_data = [0u8; ID_LENGTH];
        a_data[0] = 0x80;
        let a = NodeId(a_data);
        let b = NodeId::ZERO;
        assert_eq!(a.common_prefix_len(&b), 0);
    }

    #[test]
    fn common_prefix_len_diff_second_bit() {
        // 0x40 = 0100_0000, 0x00 = 0000_0000 -> differ at bit 1
        let mut a_data = [0u8; ID_LENGTH];
        a_data[0] = 0x40;
        let a = NodeId(a_data);
        let b = NodeId::ZERO;
        assert_eq!(a.common_prefix_len(&b), 1);
    }

    #[test]
    fn get_bit() {
        let mut data = [0u8; ID_LENGTH];
        data[0] = 0b1000_0000; // bit 0 is set
        data[1] = 0b0000_0001; // bit 15 is set
        let id = NodeId(data);
        assert!(id.get_bit(0));
        assert!(!id.get_bit(1));
        assert!(!id.get_bit(7));
        assert!(id.get_bit(15));
    }

    #[test]
    fn flip_bit() {
        let mut id = NodeId::ZERO;
        id.flip_bit(0);
        assert_eq!(id.0[0], 0x80);
        id.flip_bit(0);
        assert_eq!(id.0[0], 0x00);
        id.flip_bit(7);
        assert_eq!(id.0[0], 0x01);
    }

    #[test]
    fn range_check() {
        let min = NodeId::ZERO;
        let max = NodeId::MAX;
        let mid = NodeId::from_slice(&[0x80; ID_LENGTH]);
        assert!(mid.is_in_range(&min, &max));
        assert!(NodeId::ZERO.is_in_range(&min, &max));
        assert!(NodeId::MAX.is_in_range(&min, &max));
    }

    #[test]
    fn ordering() {
        let mut a_data = [0u8; ID_LENGTH];
        a_data[0] = 0x01;
        let a = NodeId(a_data);
        let b = NodeId::ZERO;
        assert!(a > b);
        assert!(b < a);
    }

    #[test]
    fn display_hex() {
        let id = NodeId::from_slice(&[0xAB; ID_LENGTH]);
        assert_eq!(id.to_hex(), "ab".repeat(ID_LENGTH));
    }

    #[test]
    fn random_produces_valid_id() {
        let id = NodeId::random();
        // Extremely unlikely to be all zeros
        assert_ne!(id.0, [0u8; ID_LENGTH]);
    }
}

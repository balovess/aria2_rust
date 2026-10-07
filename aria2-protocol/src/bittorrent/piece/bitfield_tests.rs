use super::*;

#[test]
fn test_new_bitfield() {
    let bf = Bitfield::new(100);
    assert_eq!(bf.len(), 100);
    assert!(bf.is_all_clear());
    assert!(!bf.is_all_set());
    assert_eq!(bf.count_set(), 0);
    assert_eq!(bf.count_clear(), 100);
}

#[test]
fn test_all_set() {
    let bf = Bitfield::all_set(50);
    assert_eq!(bf.len(), 50);
    assert!(bf.is_all_set());
    assert!(!bf.is_all_clear());
    assert_eq!(bf.count_set(), 50);
    assert_eq!(bf.count_clear(), 0);
}

#[test]
fn test_set_and_test() {
    let mut bf = Bitfield::new(10);

    assert!(!bf.test(5));
    bf.set(5).unwrap();
    assert!(bf.test(5));
    assert!(!bf.test(4));
    assert!(!bf.test(6));

    // Test out of bounds
    assert!(bf.set(100).is_none());
    assert!(!bf.test(100));
}

#[test]
fn test_clear() {
    let mut bf = Bitfield::new(10);
    bf.set(3).unwrap();
    assert!(bf.test(3));

    bf.clear(3).unwrap();
    assert!(!bf.test(3));

    // Test out of bounds
    assert!(bf.clear(100).is_none());
}

#[test]
fn test_count_bits() {
    let mut bf = Bitfield::new(100);

    bf.set(0).unwrap();
    bf.set(10).unwrap();
    bf.set(50).unwrap();
    bf.set(99).unwrap();

    assert_eq!(bf.count_set(), 4);
    assert_eq!(bf.count_clear(), 96);
}

#[test]
fn test_from_bytes() {
    // Test with bit 0 and bit 7 set: 0b10000001 = 0x81
    let bf = Bitfield::from_bytes(&[0x81], 8);
    assert!(bf.test(0));
    assert!(!bf.test(1));
    assert!(!bf.test(6));
    assert!(bf.test(7));

    // Test with multiple bytes
    let bf2 = Bitfield::from_bytes(&[0xFF, 0x00], 16);
    assert!(bf2.test(0));
    assert!(bf2.test(7));
    assert!(!bf2.test(8));
    assert!(!bf2.test(15));
}

#[test]
fn test_as_bytes() {
    let mut bf = Bitfield::new(16);
    bf.set(0).unwrap();
    bf.set(7).unwrap();
    bf.set(15).unwrap();

    let bytes = bf.as_bytes();
    assert_eq!(bytes.len(), 2);
    assert_eq!(bytes[0], 0x81); // 0b10000001
    assert_eq!(bytes[1], 0x01); // 0b00000001
}

#[test]
fn test_find_first_set() {
    let mut bf = Bitfield::new(100);
    assert!(bf.find_first_set().is_none());

    bf.set(42).unwrap();
    assert_eq!(bf.find_first_set(), Some(42));

    bf.set(10).unwrap();
    assert_eq!(bf.find_first_set(), Some(10));
}

#[test]
fn test_find_first_clear() {
    let bf = Bitfield::new(100);
    assert_eq!(bf.find_first_clear(), Some(0));

    let bf2 = Bitfield::all_set(50);
    assert!(bf2.find_first_clear().is_none());
}

#[test]
fn test_find_next_set() {
    let mut bf = Bitfield::new(100);
    bf.set(5).unwrap();
    bf.set(10).unwrap();
    bf.set(20).unwrap();

    assert_eq!(bf.find_next_set(0), Some(5));
    assert_eq!(bf.find_next_set(5), Some(10));
    assert_eq!(bf.find_next_set(10), Some(20));
    assert_eq!(bf.find_next_set(20), None);
}

#[test]
fn test_iter_set() {
    let mut bf = Bitfield::new(20);
    bf.set(1).unwrap();
    bf.set(5).unwrap();
    bf.set(10).unwrap();

    let set_bits: Vec<usize> = bf.iter_set().collect();
    assert_eq!(set_bits, vec![1, 5, 10]);
}

#[test]
fn test_iter_clear() {
    let mut bf = Bitfield::new(5);
    bf.set(1).unwrap();
    bf.set(3).unwrap();

    let clear_bits: Vec<usize> = bf.iter_clear().collect();
    assert_eq!(clear_bits, vec![0, 2, 4]);
}

#[test]
fn test_memory_usage() {
    let bf = Bitfield::new(100);
    assert_eq!(bf.memory_usage(), 13); // ceil(100/8) = 13 bytes
    assert_eq!(bf.vec_bool_memory_usage(), 100); // 100 bytes for Vec<bool>

    let ratio = bf.memory_savings_ratio();
    assert!(
        ratio > 7.5 && ratio < 8.0,
        "Memory savings ratio should be close to 8x"
    );
}

#[test]
fn test_large_bitfield() {
    // Test with a large number of bits (typical for large torrents)
    let mut bf = Bitfield::new(10_000);

    // Set every 100th bit
    for i in (0..10_000).step_by(100) {
        bf.set(i).unwrap();
    }

    assert_eq!(bf.count_set(), 100);
    assert_eq!(bf.count_clear(), 9900);

    // Memory usage should be 10,000 / 8 = 1250 bytes
    assert_eq!(bf.memory_usage(), 1250);

    // Vec<bool> would use 10,000 bytes
    assert_eq!(bf.vec_bool_memory_usage(), 10_000);

    // Verify 8x memory savings
    let ratio = bf.memory_savings_ratio();
    assert!(ratio > 7.9, "Should achieve close to 8x memory savings");
}

#[test]
fn test_bitwise_operations() {
    let mut bf1 = Bitfield::new(16);
    bf1.set(0).unwrap();
    bf1.set(1).unwrap();
    bf1.set(2).unwrap();

    let mut bf2 = Bitfield::new(16);
    bf2.set(1).unwrap();
    bf2.set(2).unwrap();
    bf2.set(3).unwrap();

    // Test AND
    let mut result = bf1.clone();
    result.bitand_assign(&bf2);
    assert!(result.test(1));
    assert!(result.test(2));
    assert!(!result.test(0));
    assert!(!result.test(3));

    // Test OR
    let mut result = bf1.clone();
    result.bitor_assign(&bf2);
    assert!(result.test(0));
    assert!(result.test(1));
    assert!(result.test(2));
    assert!(result.test(3));

    // Test XOR
    let mut result = bf1.clone();
    result.bitxor_assign(&bf2);
    assert!(result.test(0));
    assert!(!result.test(1));
    assert!(!result.test(2));
    assert!(result.test(3));
}

#[test]
fn test_set_all_and_clear_all() {
    let mut bf = Bitfield::new(100);

    bf.set_all();
    assert!(bf.is_all_set());
    assert_eq!(bf.count_set(), 100);

    bf.clear_all();
    assert!(bf.is_all_clear());
    assert_eq!(bf.count_set(), 0);
}

#[test]
fn test_edge_cases() {
    // Test with 0 bits
    let bf = Bitfield::new(0);
    assert!(bf.is_empty());
    assert_eq!(bf.count_set(), 0);

    // Test with 1 bit
    let mut bf = Bitfield::new(1);
    assert!(!bf.test(0));
    bf.set(0).unwrap();
    assert!(bf.test(0));
    assert!(bf.is_all_set());

    // Test with 7 bits (less than one byte)
    let mut bf = Bitfield::new(7);
    bf.set(6).unwrap();
    assert!(bf.test(6));
    assert!(!bf.test(7)); // Out of bounds

    // Test with 8 bits (exactly one byte)
    let mut bf = Bitfield::new(8);
    bf.set(7).unwrap();
    assert!(bf.test(7));
}

#[test]
fn test_roundtrip_bytes() {
    // Create a bitfield, convert to bytes, then back to bitfield
    let mut bf1 = Bitfield::new(100);
    bf1.set(0).unwrap();
    bf1.set(50).unwrap();
    bf1.set(99).unwrap();

    let bytes = bf1.as_bytes().to_vec();
    let bf2 = Bitfield::from_bytes(&bytes, 100);

    assert_eq!(bf1, bf2);
}

#[test]
fn test_partial_byte_handling() {
    // Test with 10 bits (more than one byte but not a multiple of 8)
    let mut bf = Bitfield::new(10);
    bf.set(8).unwrap();
    bf.set(9).unwrap();

    assert!(bf.test(8));
    assert!(bf.test(9));
    assert_eq!(bf.count_set(), 2);

    // Ensure bits 10+ are not accessible
    assert!(!bf.test(10));

    // Test from_bytes with partial byte
    let bf2 = Bitfield::from_bytes(&[0x00, 0xC0], 10); // 0xC0 = 0b11000000
    assert!(!bf2.test(7));
    assert!(bf2.test(8));
    assert!(bf2.test(9));
}

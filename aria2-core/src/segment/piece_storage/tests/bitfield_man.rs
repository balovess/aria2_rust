//! Tests for BitfieldMan within the piece_storage module.

use super::super::super::bitfield_util;
use super::super::*;

// ── Basic BitfieldMan tests ─────────────────────────────────────────

#[test]
fn test_bitfield_man_new() {
    let bfman = BitfieldMan::new(1024 * 1024, 10 * 1024 * 1024);
    assert_eq!(bfman.num_pieces(), 10);
    assert_eq!(bfman.piece_length(), 1024 * 1024);
    assert!(!bfman.is_all_complete());
    assert_eq!(bfman.count_missing_pieces(), 10);
}

#[test]
fn test_bitfield_man_set_and_has_piece() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    assert!(!bfman.has_piece(0));
    bfman.set_piece(0);
    assert!(bfman.has_piece(0));
    assert!(!bfman.has_piece(1));
}

#[test]
fn test_bitfield_man_use_piece() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.set_use_piece(2);
    assert!(bfman.is_use_piece(2));
    assert!(!bfman.is_use_piece(0));
    bfman.unset_use_piece(2);
    assert!(!bfman.is_use_piece(2));
}

#[test]
fn test_bitfield_man_has_missing_piece() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    assert!(bfman.has_missing_piece());
    bfman.set_piece(0);
    bfman.set_piece(1);
    bfman.set_piece(2);
    bfman.set_piece(3);
    assert!(!bfman.has_missing_piece());
}

#[test]
fn test_bitfield_man_mark_pieces_done() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.mark_pieces_done(2048);
    assert!(bfman.has_piece(0));
    assert!(bfman.has_piece(1));
    assert!(!bfman.has_piece(2));
}

#[test]
fn test_bitfield_man_mark_all_done() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.mark_all_done();
    assert!(bfman.is_all_complete());
}

#[test]
fn test_bitfield_man_set_bitfield() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    let bitfield = vec![0xFF]; // All 8 bits set
    bfman.set_bitfield(&bitfield);
    assert!(bfman.is_all_complete());
}

#[test]
fn test_bitfield_man_zero_length() {
    let bfman = BitfieldMan::new(0, 0);
    assert_eq!(bfman.num_pieces(), 0);
    assert!(bfman.is_all_complete()); // vacuously true
}

// ── BitfieldMan bit manipulation tests ──────────────────────────────

#[test]
fn test_bitfield_man_clear_all_bit() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.set_piece(0);
    bfman.set_piece(1);
    assert_eq!(bfman.count_missing_pieces(), 2);
    bfman.clear_all_bit();
    assert_eq!(bfman.count_missing_pieces(), 4);
    assert!(!bfman.has_piece(0));
}

#[test]
fn test_bitfield_man_set_all_bit() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.set_all_bit();
    assert!(bfman.is_all_complete());
    assert_eq!(bfman.get_completed_length(), 4096);
}

#[test]
fn test_bitfield_man_clear_all_use_bit() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.set_use_piece(0);
    bfman.set_use_piece(2);
    assert!(bfman.is_use_piece(0));
    bfman.clear_all_use_bit();
    assert!(!bfman.is_use_piece(0));
    assert!(!bfman.is_use_piece(2));
}

#[test]
fn test_bitfield_man_set_bit_range() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.set_bit_range(1, 2); // pieces 1 and 2
    assert!(!bfman.has_piece(0));
    assert!(bfman.has_piece(1));
    assert!(bfman.has_piece(2));
    assert!(!bfman.has_piece(3));
}

#[test]
fn test_bitfield_man_unset_bit_range() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.set_all_bit();
    bfman.unset_bit_range(1, 2); // clear pieces 1 and 2
    assert!(bfman.has_piece(0));
    assert!(!bfman.has_piece(1));
    assert!(!bfman.has_piece(2));
    assert!(bfman.has_piece(3));
}

#[test]
fn test_bitfield_man_get_last_block_length() {
    // 10 pieces, piece_length=1MB, total=10MB -> last piece is full
    let bfman = BitfieldMan::new(1024 * 1024, 10 * 1024 * 1024);
    assert_eq!(bfman.get_last_block_length(), 1024 * 1024);

    // 10 pieces, piece_length=1MB, total=10MB-512KB -> last piece is 512KB
    let bfman2 = BitfieldMan::new(1024 * 1024, 10 * 1024 * 1024 - 512 * 1024);
    assert_eq!(bfman2.get_last_block_length(), 512 * 1024);
}

#[test]
fn test_bitfield_man_get_block_length() {
    let bfman = BitfieldMan::new(1024, 3584); // 3 full + 1 of 512
    assert_eq!(bfman.get_block_length(0), 1024);
    assert_eq!(bfman.get_block_length(2), 1024);
    assert_eq!(bfman.get_block_length(3), 512); // last piece
    assert_eq!(bfman.get_block_length(4), 0); // out of range
}

#[test]
fn test_bitfield_man_get_max_index() {
    let bfman = BitfieldMan::new(1024, 4096);
    assert_eq!(bfman.get_max_index(), 3);
    let bfman2 = BitfieldMan::new(1024, 0);
    assert_eq!(bfman2.get_max_index(), 0);
}

// ── BitfieldMan filter tests ────────────────────────────────────────

#[test]
fn test_bitfield_man_disable_filter() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.enable_filter();
    assert!(bfman.is_filter_enabled());
    bfman.disable_filter();
    assert!(!bfman.is_filter_enabled());
}

#[test]
fn test_bitfield_man_clear_filter() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.add_filter(0, 2048);
    bfman.enable_filter();
    assert!(bfman.is_filter_enabled());
    bfman.clear_filter();
    assert!(!bfman.is_filter_enabled());
}

#[test]
fn test_bitfield_man_is_filter_bit_set() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.add_filter(0, 2048);
    bfman.enable_filter();
    assert!(bfman.is_filter_bit_set(0));
    assert!(bfman.is_filter_bit_set(1));
    assert!(!bfman.is_filter_bit_set(2));
    assert!(!bfman.is_filter_bit_set(3));
}

#[test]
fn test_bitfield_man_is_filtered_all_bit_set() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    // No filter: is_filtered_all_bit_set == is_all_complete
    assert!(!bfman.is_filtered_all_bit_set());
    bfman.set_piece(0);
    bfman.set_piece(1);
    bfman.set_piece(2);
    bfman.set_piece(3);
    assert!(bfman.is_filtered_all_bit_set());
}

#[test]
fn test_bitfield_man_is_filtered_all_bit_set_with_filter() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    // Filter: only pieces 0 and 1 are selected
    bfman.add_filter(0, 2048);
    bfman.enable_filter();
    // Nothing completed yet
    assert!(!bfman.is_filtered_all_bit_set());
    // Complete only the filtered pieces
    bfman.set_piece(0);
    bfman.set_piece(1);
    assert!(bfman.is_filtered_all_bit_set());
    // Non-filtered pieces can be incomplete
    assert!(!bfman.has_piece(2));
}

#[test]
fn test_bitfield_man_filtered_total_length() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    // No filter: C++ returns 0 (filterBitfield_ is null)
    assert_eq!(bfman.get_filtered_total_length(), 0);
    // Enable filter with pieces 0,1 selected
    bfman.add_filter(0, 2048);
    bfman.enable_filter();
    assert_eq!(bfman.get_filtered_total_length(), 2048);
}

#[test]
fn test_bitfield_man_filtered_completed_length() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.add_filter(0, 2048);
    bfman.enable_filter();
    bfman.set_piece(0);
    assert_eq!(bfman.get_filtered_completed_length(), 1024);
    bfman.set_piece(1);
    assert_eq!(bfman.get_filtered_completed_length(), 2048);
}

// ── Filter semantics tests (C++ enableFilter doesn't set all bits) ──

#[test]
fn test_enable_filter_does_not_set_bits() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.enable_filter();
    // Filter is enabled but no bits are set — nothing is selected
    assert!(bfman.is_filter_enabled());
    assert_eq!(bfman.get_filtered_total_length(), 0);
}

#[test]
fn test_add_filter_then_enable() {
    // C++ setupFileFilter() flow: addFilter for each file, then enableFilter
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.add_filter(0, 2048); // select pieces 0,1
    bfman.enable_filter();
    assert!(bfman.is_filter_bit_set(0));
    assert!(bfman.is_filter_bit_set(1));
    assert!(!bfman.is_filter_bit_set(2));
    assert!(!bfman.is_filter_bit_set(3));
    assert_eq!(bfman.get_filtered_total_length(), 2048);
}

#[test]
fn test_add_not_filter() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    // addNotFilter selects everything EXCEPT the specified range
    bfman.add_not_filter(1024, 1024); // exclude piece 1
    bfman.enable_filter();
    assert!(bfman.is_filter_bit_set(0)); // before range -> selected
    assert!(!bfman.is_filter_bit_set(1)); // in range -> NOT selected
    assert!(bfman.is_filter_bit_set(2)); // after range -> selected
    assert!(bfman.is_filter_bit_set(3)); // after range -> selected
}

#[test]
fn test_filter_aware_has_missing_piece() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.add_filter(0, 2048); // only pieces 0,1 are selected
    bfman.enable_filter();
    // Peer has pieces 2,3 — but those are not selected by filter
    let peer_bf: Vec<u8> = vec![0b00110000]; // bits 2,3 set (MSB-first)
    assert!(!bfman.has_missing_piece_with_bitfield(&peer_bf));
    // Peer has pieces 0,1 — those are selected by filter
    let peer_bf2: Vec<u8> = vec![0b11000000]; // bits 0,1 set
    assert!(bfman.has_missing_piece_with_bitfield(&peer_bf2));
}

#[test]
fn test_filter_aware_all_missing_indexes() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    bfman.add_filter(0, 2048); // only pieces 0,1 are selected
    bfman.enable_filter();
    // Peer has all pieces but filter limits to 0,1
    let peer_bf: Vec<u8> = vec![0b11110000];
    let missing = bfman.all_missing_indexes(&peer_bf);
    // Only pieces 0,1 should be in the result
    assert!(bitfield_util::test_bit(&missing, 4, 0));
    assert!(bitfield_util::test_bit(&missing, 4, 1));
    assert!(!bitfield_util::test_bit(&missing, 4, 2));
    assert!(!bitfield_util::test_bit(&missing, 4, 3));
}

// ── BitfieldMan missing index / first missing tests ─────────────────

#[test]
fn test_get_first_missing_index() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    // No pieces complete -> first missing is 0
    assert_eq!(bfman.get_first_missing_index(), Some(0));

    // Complete piece 0 -> first missing is 1
    bfman.set_piece(0);
    assert_eq!(bfman.get_first_missing_index(), Some(1));

    // Complete all -> no missing
    bfman.set_piece(1);
    bfman.set_piece(2);
    bfman.set_piece(3);
    assert_eq!(bfman.get_first_missing_index(), None);
}

#[test]
fn test_get_first_missing_index_with_filter() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    // Only pieces 2,3 are selected by filter
    bfman.add_filter(2048, 2048);
    bfman.enable_filter();
    // First missing with filter = piece 2 (first filter-selected piece)
    assert_eq!(bfman.get_first_missing_index(), Some(2));

    // Complete piece 2 -> first missing is 3
    bfman.set_piece(2);
    assert_eq!(bfman.get_first_missing_index(), Some(3));
}

// ── BitfieldMan piece selection tests ───────────────────────────────

#[test]
fn test_sparse_selection_midpoint() {
    // With pieces completed at both ends, sparse should select midpoint
    let mut bfman = BitfieldMan::new(1024, 8192); // 8 pieces
    let ignore = vec![0u8; 1];

    // Complete pieces 0,1,2 and 6,7 -> gap is pieces 3,4,5
    bfman.set_piece(0);
    bfman.set_piece(1);
    bfman.set_piece(2);
    bfman.set_piece(6);
    bfman.set_piece(7);
    // Longest range: [3, 6) -> size=3
    // Previous piece (2) is completed and not in-use -> return 3
    let result = bfman.get_sparse_missing_unused_index(0, &ignore);
    assert_eq!(result, Some(3));
}

#[test]
fn test_inorder_with_min_split_size() {
    let mut bfman = BitfieldMan::new(1024, 8192); // 8 pieces
    let ignore = vec![0u8; 1];

    // Complete piece 0 -> start from piece 1
    bfman.set_piece(0);

    // min_split_size=3072 (3 pieces) -> need 3 consecutive free pieces
    // Pieces 1-7 are free -> piece 1 is adjacent to completed -> return 1
    let result = bfman.get_inorder_missing_unused_index(0, 8, 3072, &ignore);
    assert_eq!(result, Some(1));
}

#[test]
fn test_sparse_with_ignore_bitfield() {
    let bfman = BitfieldMan::new(1024, 4096); // 4 pieces
    // Ignore pieces 2,3
    let ignore: Vec<u8> = vec![0b00110000]; // bits 2,3 set (MSB-first)

    // Only pieces 0,1 are available
    let result = bfman.get_sparse_missing_unused_index(0, &ignore);
    assert_eq!(result, Some(0)); // range [0,2), start=0
}

#[test]
fn test_geom_selection_with_offset() {
    let mut bfman = BitfieldMan::new(1024, 8192); // 8 pieces
    let ignore = vec![0u8; 1];

    // Complete pieces 0,1,2 -> offset_index should be 3
    bfman.set_piece(0);
    bfman.set_piece(1);
    bfman.set_piece(2);

    // Geom with offset_index=3, base=1.5
    // Window [3,4) -> piece 3 is available -> return 3
    let result = bfman.get_geom_missing_unused_index(0, &ignore, 1.5, 3);
    assert_eq!(result, Some(3));
}

// ── Range-based query tests (C++ BitfieldMan methods) ───────────────

#[test]
fn test_bit_range_set() {
    let mut bf = BitfieldMan::new(1024, 4096);
    // No pieces set -> range [0,4) should be false
    assert!(!bf.is_bit_range_set(0, 4));

    // Set pieces 0,1
    bf.set_piece(0);
    bf.set_piece(1);
    assert!(bf.is_bit_range_set(0, 2));
    assert!(!bf.is_bit_range_set(0, 3));

    // Set all pieces
    bf.set_all_bit();
    assert!(bf.is_bit_range_set(0, 4));
}

#[test]
fn test_bit_set_offset_range() {
    let mut bf = BitfieldMan::new(1024, 4096);
    // C++ returns false for zero length (no range to check)
    assert!(!bf.is_bit_set_offset_range(0, 0));

    // Set pieces 0,1 -> byte range [0, 2048) should be true
    bf.set_piece(0);
    bf.set_piece(1);
    assert!(bf.is_bit_set_offset_range(0, 2048));
    assert!(!bf.is_bit_set_offset_range(0, 4096));
}

#[test]
fn test_offset_completed_length() {
    let mut bf = BitfieldMan::new(1024, 4096);
    // No pieces -> completed length = 0
    assert_eq!(bf.get_offset_completed_length(0, 2048), 0);

    // Set pieces 0,1 -> range [0, 2048) has 2048 bytes completed
    bf.set_piece(0);
    bf.set_piece(1);
    assert_eq!(bf.get_offset_completed_length(0, 2048), 2048);
    // Range [0, 4096) has 2048 completed (pieces 0,1), 0 for piece 2
    assert_eq!(bf.get_offset_completed_length(0, 4096), 2048);
}

#[test]
fn test_missing_unused_length() {
    let mut bf = BitfieldMan::new(1024, 4096);
    // All 4 pieces missing -> 4096 bytes available from index 0
    assert_eq!(bf.get_missing_unused_length(0), 4096);
    // Starting from index 2 -> 2048 bytes available
    assert_eq!(bf.get_missing_unused_length(2), 2048);

    // Set piece 0 as in-use -> still 4096 from index 1 (piece 0 is used)
    bf.set_use_piece(0);
    assert_eq!(bf.get_missing_unused_length(1), 3072);
}

#[test]
fn test_first_n_missing_unused_indexes() {
    let mut bf = BitfieldMan::new(1024, 4096);
    // Get first 2 missing indexes -> [0, 1]
    let indexes = bf.get_first_n_missing_unused_indexes(2);
    assert_eq!(indexes, vec![0, 1]);

    // Get all 4 -> [0, 1, 2, 3]
    let indexes = bf.get_first_n_missing_unused_indexes(10);
    assert_eq!(indexes, vec![0, 1, 2, 3]);

    // Set piece 0 -> first 2 are [1, 2]
    bf.set_piece(0);
    let indexes = bf.get_first_n_missing_unused_indexes(2);
    assert_eq!(indexes, vec![1, 2]);
}

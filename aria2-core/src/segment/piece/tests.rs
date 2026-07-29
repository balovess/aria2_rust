//! Tests for the piece module.

use super::piece_impl::{Piece, DEFAULT_BLOCK_LENGTH};

// ── Construction ────────────────────────────────────────────────────

#[test]
fn test_new_default_block_length() {
    let piece = Piece::new(5, 65536);
    assert_eq!(piece.index(), 5);
    assert_eq!(piece.length(), 65536);
    assert_eq!(piece.block_length(), DEFAULT_BLOCK_LENGTH);
    assert_eq!(piece.count_blocks(), 4); // 65536 / 16384 = 4
    assert_eq!(piece.count_completed_blocks(), 0);
    assert_eq!(piece.count_missing_blocks(), 4);
    assert!(!piece.is_complete());
}

#[test]
fn test_new_custom_block_length() {
    let piece = Piece::with_block_length(0, 32768, 8192);
    assert_eq!(piece.count_blocks(), 4); // 32768 / 8192 = 4
    assert_eq!(piece.block_length(), 8192);
}

#[test]
fn test_new_non_aligned_length() {
    // 50000 bytes with 16384 block length = ceil(50000/16384) = 4 blocks
    // Last block length = 50000 - 3*16384 = 50000 - 49152 = 848
    let piece = Piece::new(0, 50000);
    assert_eq!(piece.count_blocks(), 4);
    assert_eq!(piece.block_length_at(0), 16384);
    assert_eq!(piece.block_length_at(1), 16384);
    assert_eq!(piece.block_length_at(2), 16384);
    assert_eq!(piece.block_length_at(3), 848); // last block
    assert_eq!(piece.block_length_at(4), 0); // out of range
}

#[test]
fn test_new_zero_length() {
    let piece = Piece::new(0, 0);
    assert_eq!(piece.count_blocks(), 0);
    assert!(piece.is_complete()); // vacuously true
    assert_eq!(piece.completed_length(), 0);
}

#[test]
fn test_default() {
    let piece = Piece::default();
    assert_eq!(piece.index(), 0);
    assert_eq!(piece.length(), 0);
    assert_eq!(piece.count_blocks(), 0);
}

// ── Block completion ────────────────────────────────────────────────

#[test]
fn test_complete_block() {
    let mut piece = Piece::new(0, 65536);
    assert_eq!(piece.count_completed_blocks(), 0);

    piece.complete_block(0);
    assert!(piece.has_block(0));
    assert_eq!(piece.count_completed_blocks(), 1);

    piece.complete_block(1);
    assert!(piece.has_block(1));
    assert_eq!(piece.count_completed_blocks(), 2);
}

#[test]
fn test_complete_all_blocks() {
    let mut piece = Piece::new(0, 65536);
    for i in 0..4 {
        piece.complete_block(i);
    }
    assert!(piece.is_complete());
    assert_eq!(piece.completed_length(), 65536);
}

#[test]
fn test_clear_all_blocks() {
    let mut piece = Piece::new(0, 65536);
    for i in 0..4 {
        piece.complete_block(i);
    }
    assert!(piece.is_complete());

    piece.clear_all_blocks();
    assert!(!piece.is_complete());
    assert_eq!(piece.count_completed_blocks(), 0);
}

// ── Missing unused block ────────────────────────────────────────────

#[test]
fn test_get_missing_unused_block_index() {
    let mut piece = Piece::new(0, 65536);

    // All blocks are missing and unused
    assert_eq!(piece.get_missing_unused_block_index(), Some(0));

    // Mark block 0 as in-use
    piece.set_block_in_use(0);
    assert_eq!(piece.get_missing_unused_block_index(), Some(1));

    // Mark block 1 as completed
    piece.complete_block(1);
    assert_eq!(piece.get_missing_unused_block_index(), Some(2));
}

// ── In-use tracking ─────────────────────────────────────────────────

#[test]
fn test_set_and_clear_block_in_use() {
    let mut piece = Piece::new(0, 65536);

    piece.set_block_in_use(2);
    assert!(piece.is_block_in_use(2));
    assert!(!piece.is_block_in_use(0));

    piece.clear_block_in_use(2);
    assert!(!piece.is_block_in_use(2));
}

// ── Completed length ────────────────────────────────────────────────

#[test]
fn test_completed_length_partial() {
    let mut piece = Piece::new(0, 65536);
    piece.complete_block(0);
    assert_eq!(piece.completed_length(), 16384);

    piece.complete_block(1);
    assert_eq!(piece.completed_length(), 32768);
}

#[test]
fn test_completed_length_non_aligned() {
    let mut piece = Piece::new(0, 50000);
    // Complete all but last block
    piece.complete_block(0);
    piece.complete_block(1);
    piece.complete_block(2);
    assert_eq!(piece.completed_length(), 3 * 16384); // 49152

    piece.complete_block(3); // Last block = 848 bytes
    assert_eq!(piece.completed_length(), 50000);
}

// ── User tracking ───────────────────────────────────────────────────

#[test]
fn test_add_remove_user() {
    let mut piece = Piece::new(0, 65536);

    piece.add_user(42);
    assert_eq!(piece.user_count(), 1);

    piece.add_user(100);
    assert_eq!(piece.user_count(), 2);

    piece.remove_user(42);
    assert_eq!(piece.user_count(), 1);

    piece.remove_user(100);
    assert_eq!(piece.user_count(), 0);
}

// ── Used by segment ─────────────────────────────────────────────────

#[test]
fn test_used_by_segment() {
    let mut piece = Piece::new(0, 65536);
    assert!(!piece.is_used_by_segment());

    piece.set_used_by_segment(true);
    assert!(piece.is_used_by_segment());

    piece.set_used_by_segment(false);
    assert!(!piece.is_used_by_segment());
}

// ── Hash verification ───────────────────────────────────────────────

#[test]
fn test_hash_update_and_digest() {
    let mut piece = Piece::new(0, 4);
    piece.set_hash_type("sha-1");

    assert!(piece.update_hash(0, b"test"));
    assert!(piece.is_hash_calculated());

    let digest = piece.get_digest();
    assert!(digest.is_some());
    // SHA1 of "test" = 0xa94a8fe5ccb19ba61c4c0873d391e987982fbbd3
    assert_eq!(digest.unwrap().len(), 20);
}

#[test]
fn test_hash_offset_mismatch() {
    let mut piece = Piece::new(0, 8);
    piece.set_hash_type("sha-1");

    assert!(piece.update_hash(0, b"tes"));
    assert!(!piece.update_hash(5, b"t")); // offset mismatch
    assert!(piece.update_hash(3, b"t")); // correct offset
}

#[test]
fn test_hash_no_type() {
    let mut piece = Piece::new(0, 4);
    assert!(!piece.update_hash(0, b"test"));
}

#[test]
fn test_destroy_hash_context() {
    let mut piece = Piece::new(0, 4);
    piece.set_hash_type("sha-1");
    piece.update_hash(0, b"test");
    assert!(piece.is_hash_calculated());

    piece.destroy_hash_context();
    assert!(!piece.is_hash_calculated());
}

// ── Ordering and equality ───────────────────────────────────────────

#[test]
fn test_piece_ordering() {
    let p1 = Piece::new(1, 65536);
    let p2 = Piece::new(2, 65536);
    let p3 = Piece::new(1, 32768);

    assert!(p1 < p2);
    assert!(p1 == p3); // Same index regardless of length
}

#[test]
fn test_piece_debug_format() {
    let piece = Piece::new(5, 65536);
    let debug_str = format!("{:?}", piece);
    assert!(debug_str.contains("index: 5"));
    assert!(debug_str.contains("length: 65536"));
}

#[test]
fn test_piece_display_format() {
    let piece = Piece::new(5, 65536);
    let display_str = format!("{}", piece);
    assert!(display_str.contains("index=5"));
    assert!(display_str.contains("length=65536"));
}

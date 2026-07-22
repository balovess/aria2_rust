//! Tests for the piece_storage module.

use super::*;
use super::super::bitfield_util;

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
fn test_default_piece_storage_new() {
    let storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
    assert_eq!(storage.num_pieces(), 10);
    assert!(!storage.download_finished());
    assert!(PieceStorage::has_missing_unused_piece(&storage));
}

#[test]
fn test_default_piece_storage_get_missing_piece() {
    let mut storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
    let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
    assert_eq!(piece.index(), 0);
    assert!(storage.is_piece_used(0));
    assert!(!storage.has_piece(0));
}

#[test]
fn test_default_piece_storage_complete_piece() {
    let mut storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
    let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
    let result = storage.complete_piece(&piece);
    assert!(result);
    assert!(storage.has_piece(0));
    assert!(!storage.is_piece_used(0));
}

#[test]
fn test_default_piece_storage_cancel_piece() {
    let mut storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
    let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
    let mut piece = piece;
    storage.cancel_piece(&mut piece, 1);
    assert!(!storage.is_piece_used(0));
}

#[test]
fn test_default_piece_storage_download_all() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    for _ in 0..4 {
        let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        storage.complete_piece(&piece);
    }
    assert!(storage.download_finished());
    assert_eq!(storage.get_completed_length(), 4096);
}

#[test]
fn test_default_piece_storage_end_game() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    // Complete 2 pieces, leaving 2 remaining
    for _ in 0..2 {
        let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        storage.complete_piece(&piece);
    }
    // With only 2 pieces remaining (<= END_GAME_PIECE_NUM), should enter endgame
    assert!(PieceStorage::is_end_game(&storage));
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

// ── New PieceStorage method tests ────────────────────────────────────

#[test]
fn test_get_piece_returns_in_flight_piece() {
    let mut storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
    let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
    let got = storage.get_piece(0).unwrap();
    assert_eq!(got.index(), piece.index());
}

#[test]
fn test_get_piece_returns_new_piece_for_untracked() {
    let storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
    let got = storage.get_piece(0).unwrap();
    assert_eq!(got.index(), 0);
    // Should NOT mark it as used
    assert!(!storage.is_piece_used(0));
}

#[test]
fn test_get_piece_out_of_bounds() {
    let storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
    assert!(storage.get_piece(999).is_none());
}

#[test]
fn test_get_bitfield_length() {
    let storage = DefaultPieceStorage::new(1024, 4096);
    // 4 pieces -> ceil(4/8) = 1 byte
    assert_eq!(storage.get_bitfield_length(), 1);
}

#[test]
fn test_mark_all_pieces_done() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    storage.mark_all_pieces_done();
    assert!(storage.download_finished());
    assert!(storage.has_piece(0));
    assert!(storage.has_piece(3));
}

#[test]
fn test_mark_piece_missing() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    storage.mark_all_pieces_done();
    assert!(storage.has_piece(1));
    storage.mark_piece_missing(1);
    assert!(!storage.has_piece(1));
    assert!(storage.has_piece(0)); // others still done
}

#[test]
fn test_mark_piece_missing_noop_if_already_missing() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    // Piece 0 is already missing — should be a no-op
    storage.mark_piece_missing(0);
    assert!(!storage.has_piece(0));
}

#[test]
fn test_set_end_game_piece_num() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    storage.set_end_game_piece_num(2);
    // Complete 2 pieces, leaving 2 remaining
    for _ in 0..2 {
        let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        storage.complete_piece(&piece);
    }
    // 2 remaining = end_game_piece_num, should enter endgame
    assert!(PieceStorage::is_end_game(&storage));
}

#[test]
fn test_get_piece_length() {
    let storage = DefaultPieceStorage::new(1024, 4096);
    // All pieces are 1024 bytes (total = 4 * 1024, exact multiple)
    assert_eq!(storage.get_piece_length(0), 1024);
    assert_eq!(storage.get_piece_length(3), 1024);
    assert_eq!(storage.get_piece_length(999), 0); // out of bounds
}

#[test]
fn test_get_piece_length_last_piece_shorter() {
    // 3 pieces: [1024, 1024, 512]
    let storage = DefaultPieceStorage::new(1024, 2560);
    assert_eq!(storage.get_piece_length(0), 1024);
    assert_eq!(storage.get_piece_length(1), 1024);
    assert_eq!(storage.get_piece_length(2), 512);
}

#[test]
fn test_advertise_piece_and_get_advertised() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    // C++ starts nextHaveIndex_ at 1, so first entry gets have_index=1
    storage.advertise_piece(1, 0); // CUID 1 completed piece 0 (have_index=1)
    storage.advertise_piece(2, 1); // CUID 2 completed piece 1 (have_index=2)

    // CUID 1 asks with last_have_index=0: have_index > 0 matches both
    // -> CUID 1's own entry (have_index=1, piece 0) is skipped (same CUID)
    // -> CUID 2's entry (have_index=2, piece 1) qualifies
    let (indexes, new_last) = storage.get_advertised_piece_indexes(1, 0);
    assert_eq!(indexes, vec![1]);
    assert_eq!(new_last, 3); // last have_index + 1

    // CUID 2 asks with last_have_index=0: have_index > 0 matches both
    // -> CUID 1's entry (have_index=1, piece 0) qualifies
    // -> CUID 2's own entry (have_index=2, piece 1) is skipped (same CUID)
    let (indexes2, _) = storage.get_advertised_piece_indexes(2, 0);
    assert_eq!(indexes2, vec![0]);

    // CUID 1 asks with last_have_index=2: only have_index > 2 matches
    // -> nothing new since last_have_index
    let (indexes3, _) = storage.get_advertised_piece_indexes(1, 2);
    assert!(indexes3.is_empty());
}

#[test]
fn test_get_advertised_empty() {
    let storage = DefaultPieceStorage::new(1024, 4096);
    let (indexes, new_last) = storage.get_advertised_piece_indexes(1, 0);
    assert!(indexes.is_empty());
    assert_eq!(new_last, 0);
}

#[test]
fn test_remove_advertised_piece() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    storage.advertise_piece(1, 0);
    // Remove all entries older than far future
    storage.remove_advertised_piece(u64::MAX);
    let (indexes, _) = storage.get_advertised_piece_indexes(2, 0);
    assert!(indexes.is_empty());
}

#[test]
fn test_in_flight_pieces() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
    storage.add_in_flight_piece(piece);
    assert_eq!(storage.count_in_flight_piece(), 1);
    let in_flight = storage.get_in_flight_pieces();
    assert_eq!(in_flight.len(), 1);
    assert_eq!(in_flight[0].index(), 0);
}

#[test]
fn test_piece_stats_for_index() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    storage.add_piece_stats_for_index(0);
    storage.add_piece_stats_for_index(0);
    storage.add_piece_stats_for_index(1);
    // Verify bitfield-based subtract works
    let bf = storage.get_bitfield(); // all zeros (no pieces complete)
    storage.subtract_piece_stats(&bf); // no-op since all bits are 0
}

#[test]
fn test_get_next_used_index() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    // Complete piece 0 and 2
    let piece0 = storage.get_missing_piece(0, &[], 0, 1).unwrap();
    storage.complete_piece(&piece0);
    let piece2 = storage.get_missing_piece_by_index(2, 1).unwrap();
    storage.complete_piece(&piece2);
    // Next used after 0 should be 2
    assert_eq!(storage.get_next_used_index(0), 2);
    // Next used after 2 should be num_pieces (4)
    assert_eq!(storage.get_next_used_index(2), 4);
}

#[test]
fn test_filtered_lengths_default() {
    let storage = DefaultPieceStorage::new(1024, 4096);
    // No filter active — filtered == unfiltered
    assert_eq!(storage.get_filtered_total_length(), 4096);
    assert_eq!(storage.get_filtered_completed_length(), 0);
}

#[test]
fn test_is_selective_downloading_mode_default() {
    let storage = DefaultPieceStorage::new(1024, 4096);
    assert!(!storage.is_selective_downloading_mode());
}

// ── New BitfieldMan method tests ────────────────────────────────────

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

#[test]
fn test_bitfield_man_filtered_total_length() {
    let mut bfman = BitfieldMan::new(1024, 4096);
    // No filter: returns total
    assert_eq!(bfman.get_filtered_total_length(), 4096);
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
    assert!(bfman.is_filter_bit_set(0));  // before range -> selected
    assert!(!bfman.is_filter_bit_set(1)); // in range -> NOT selected
    assert!(bfman.is_filter_bit_set(2));  // after range -> selected
    assert!(bfman.is_filter_bit_set(3));  // after range -> selected
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

// ── DefaultPieceStorage with filter tests ───────────────────────────

#[test]
fn test_download_finished_with_filter() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    // Set up selective downloading: only pieces 0,1
    storage.bfman.add_filter(0, 2048);
    storage.bfman.enable_filter();

    // Complete only the filtered pieces
    let piece0 = storage.get_missing_piece_by_index(0, 1).unwrap();
    storage.complete_piece(&piece0);
    assert!(!storage.download_finished()); // piece 1 not done

    let piece1 = storage.get_missing_piece_by_index(1, 1).unwrap();
    storage.complete_piece(&piece1);
    assert!(storage.download_finished()); // all filtered pieces done
    assert!(!storage.all_download_finished()); // but not ALL pieces
}

#[test]
fn test_set_bitfield_clears_use_and_adds_stats() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    // Mark piece 0 as in-use
    storage.get_missing_piece(0, &[], 0, 1).unwrap();
    assert!(storage.is_piece_used(0));

    // Set bitfield — should clear use bits and update stats
    let bf: Vec<u8> = vec![0b11000000]; // pieces 0,1 complete
    storage.set_bitfield(&bf);
    assert!(!storage.is_piece_used(0)); // use bit cleared
}

#[test]
fn test_mark_pieces_done_zero_clears() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    storage.get_missing_piece(0, &[], 0, 1).unwrap();
    storage.mark_pieces_done(0);
    assert_eq!(storage.get_completed_length(), 0);
}

#[test]
fn test_mark_pieces_done_full() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    storage.mark_pieces_done(4096);
    assert!(storage.download_finished());
    assert!(storage.all_download_finished());
}

// ── Stream piece selector tests ────────────────────────────────────────

#[test]
fn test_sparse_selection_basic() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    // No ignore, no filter — all 4 pieces available
    let ignore = vec![0u8; 1];

    // First selection: range [0,4), start=0 -> return 0
    let piece0 = storage.get_missing_piece(0, &ignore, 0, 1);
    assert!(piece0.is_some());
    assert_eq!(piece0.unwrap().index(), 0);

    // Second selection: piece 0 is in-use -> range [1,4)
    // Because piece 0 (before the range start) is in-use,
    // sparse adjusts start to midpoint: (1+4)/2 = 2
    // Then checks: range_size * piece_length >= min_split_size -> 2*1024 >= 0 -> true
    // Returns piece 2 (C++ behavior: avoid interfering with active download)
    let piece1 = storage.get_missing_piece(0, &ignore, 0, 1);
    assert!(piece1.is_some());
    assert_eq!(piece1.unwrap().index(), 2);
}

#[test]
fn test_inorder_selection_basic() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    storage.set_stream_piece_selector(StreamPieceSelectorKind::Inorder);
    let ignore = vec![0u8; 1];

    // Inorder always returns start_index first
    let piece0 = storage.get_missing_piece(0, &ignore, 0, 1);
    assert_eq!(piece0.unwrap().index(), 0);

    let piece1 = storage.get_missing_piece(0, &ignore, 0, 1);
    assert_eq!(piece1.unwrap().index(), 1);
}

#[test]
fn test_geom_selection_basic() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    storage.set_stream_piece_selector(StreamPieceSelectorKind::Geom);
    let ignore = vec![0u8; 1];

    // Geom starts from offsetIndex=0, searches [0,1), then [1,2.5), etc.
    let piece0 = storage.get_missing_piece(0, &ignore, 0, 1);
    assert_eq!(piece0.unwrap().index(), 0);
}

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

#[test]
fn test_get_missing_piece_by_index_filter_check() {
    let mut storage = DefaultPieceStorage::new(1024, 4096);
    // Enable filter for pieces 0,1 only
    storage.bfman.add_filter(0, 2048);
    storage.bfman.enable_filter();

    // Piece 2 is not filter-selected -> should return None
    let result = storage.get_missing_piece_by_index(2, 1);
    assert!(result.is_none());

    // Piece 0 is filter-selected -> should succeed
    let result = storage.get_missing_piece_by_index(0, 1);
    assert!(result.is_some());
}

// ── Range-based query tests (C++ BitfieldMan methods) ──

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
    // Empty range should return true
    assert!(bf.is_bit_set_offset_range(0, 0));

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

//! Tests for DefaultPieceStorage and PieceStorage trait within the piece_storage module.

use super::super::*;

// ── Basic DefaultPieceStorage tests ─────────────────────────────────

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

// ── PieceStorage method tests ───────────────────────────────────────

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

    // C++ does NOT filter by myCuid — all entries with haveIndex > lastHaveIndex are returned.
    // CUID 1 asks with last_have_index=0: have_index > 0 matches both entries
    // -> returns indexes [0, 1], new_last = last entry's have_index = 2
    let (indexes, new_last) = storage.get_advertised_piece_indexes(1, 0);
    assert_eq!(indexes, vec![0, 1]);
    assert_eq!(new_last, 2); // C++ returns last entry's haveIndex (not +1)

    // CUID 2 asks with last_have_index=0: same result — CUID filtering is NOT done here
    let (indexes2, _) = storage.get_advertised_piece_indexes(2, 0);
    assert_eq!(indexes2, vec![0, 1]);

    // CUID 1 asks with last_have_index=2: only have_index > 2 matches
    // -> nothing new since last_have_index, returns lastHaveIndex unchanged
    let (indexes3, new_last3) = storage.get_advertised_piece_indexes(1, 2);
    assert!(indexes3.is_empty());
    assert_eq!(new_last3, 2); // C++ returns lastHaveIndex when no entries match after it
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
    // No filter active — C++ returns 0 for filtered total when filter
    // is disabled (filterBitfield_ is null). Filtered completed falls
    // back to unfiltered completed length.
    assert_eq!(storage.get_filtered_total_length(), 0);
    assert_eq!(storage.get_filtered_completed_length(), 0);
}

#[test]
fn test_is_selective_downloading_mode_default() {
    let storage = DefaultPieceStorage::new(1024, 4096);
    assert!(!storage.is_selective_downloading_mode());
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

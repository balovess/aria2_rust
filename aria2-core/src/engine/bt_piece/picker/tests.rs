use super::*;

#[test]
fn test_piece_selection_strategy_variants() {
    assert_ne!(
        PieceSelectionStrategy::Sequential,
        PieceSelectionStrategy::RarestFirst
    );
    assert_ne!(
        PieceSelectionStrategy::Random,
        PieceSelectionStrategy::Geometric
    );
}

#[test]
fn test_piece_priority_mode_variants() {
    assert_ne!(
        PiecePriorityMode::SequentialHead,
        PiecePriorityMode::SequentialTail
    );
    assert_ne!(
        PiecePriorityMode::SequentialTail,
        PiecePriorityMode::RarestFirst
    );
}

#[test]
fn test_piece_picker_new() {
    let picker = PiecePicker::new(100);
    assert!(picker.endgame_candidates().is_empty());
    assert_eq!(picker.remaining_count(), 100);
    assert!(!picker.is_complete());
}

#[test]
fn test_allowed_piece_filter_controls_selection_and_completion() {
    let mut picker = PiecePicker::new(4);
    picker.set_endgame_threshold(0);
    picker.set_allowed_pieces(&[1, 3]);

    assert_eq!(picker.allowed_count(), 2);
    assert_eq!(picker.remaining_count(), 2);
    assert!(!picker.is_allowed(0));
    assert!(picker.is_allowed(1));
    assert_eq!(picker.pick_next(), Some(1));

    picker.mark_completed(1);
    assert_eq!(picker.remaining_count(), 1);
    assert!(!picker.is_complete());
    assert_eq!(picker.pick_next(), Some(3));

    picker.mark_completed(3);
    assert!(picker.is_complete());
    assert_eq!(picker.remaining_count(), 0);
    assert_eq!(picker.pick_next(), None);
}

#[test]
fn test_allowed_piece_filter_keeps_endgame_candidates_selective() {
    let mut picker = PiecePicker::new(5);
    picker.set_allowed_pieces(&[2, 4]);

    assert_eq!(picker.endgame_candidates(), &[2, 4]);
    picker.mark_completed(0);
    assert_eq!(picker.endgame_candidates(), &[2, 4]);
    picker.mark_completed(2);
    assert_eq!(picker.endgame_candidates(), &[4]);
}

#[test]
fn test_piece_picker_new_u32() {
    let picker = PiecePicker::new(50u32);
    assert_eq!(picker.remaining_count(), 50);
}

#[test]
fn test_piece_picker_set_strategy() {
    let mut picker = PiecePicker::new(100);
    picker.set_strategy(PieceSelectionStrategy::Sequential);
    // Verify it compiles and runs without panic
    let _ = picker.select(&[0xFF; 13], 100);
}

#[test]
fn test_piece_picker_set_priority_mode() {
    let mut picker = PiecePicker::new(100);
    picker.set_priority_mode(PiecePriorityMode::SequentialHead);
    assert_eq!(picker.priority_mode(), PiecePriorityMode::SequentialHead);
}

#[test]
fn test_explicit_priority_pieces_precede_base_selector() {
    let mut picker = PiecePicker::new(5);
    picker.set_strategy(PieceSelectionStrategy::RarestFirst);
    picker.set_frequencies_from_peers(&[0, 0, 0, 9, 0]);
    picker.set_priority_pieces(vec![3, 1, 3]);

    assert_eq!(picker.priority_pieces().len(), 2);
    assert!(picker.priority_pieces().contains(&1));
    assert!(picker.priority_pieces().contains(&3));

    let first = picker
        .pick_next()
        .expect("priority piece should be selected");
    assert!(first == 1 || first == 3);
    picker.mark_completed(first);
    let second = picker
        .pick_next()
        .expect("second priority piece should follow");
    assert!(second == 1 || second == 3);
    assert_ne!(first, second);
}

#[test]
fn test_explicit_priority_pieces_respect_peer_bitfield() {
    let mut picker = PiecePicker::new(4);
    picker.set_priority_pieces(vec![2, 1]);

    // The peer has piece 1 but not piece 2 (MSB-first bitfield).
    assert_eq!(picker.select(&[0b0100_0000], 4), Some(1));
}

#[test]
fn test_piece_pick_strategy_variants() {
    assert_ne!(
        PiecePickStrategy::Sequential,
        PiecePickStrategy::RarestFirst
    );
    assert_ne!(PiecePickStrategy::Random, PiecePickStrategy::Geometric);
}

#[test]
fn test_piece_picker_config_default() {
    let config = PiecePickerConfig::default();
    assert_eq!(config.strategy, PiecePickStrategy::RarestFirst);
    assert_eq!(config.request_queue_size, 16);
    assert!(config.end_game_threshold > 0.9);
}

#[test]
fn test_picked_piece() {
    let picked = PickedPiece {
        index: 42,
        priority: 5,
        is_end_game: false,
    };
    assert_eq!(picked.index, 42);
    assert_eq!(picked.priority, 5);
    assert!(!picked.is_end_game);
}

#[test]
fn test_mark_completed_and_is_complete() {
    let mut picker = PiecePicker::new(3u32);
    assert!(!picker.is_complete());
    assert_eq!(picker.remaining_count(), 3);

    picker.mark_completed(0);
    assert!(!picker.is_complete());
    assert_eq!(picker.remaining_count(), 2);

    picker.mark_completed(1);
    picker.mark_completed(2);
    assert!(picker.is_complete());
    assert_eq!(picker.remaining_count(), 0);
}

#[test]
fn test_export_bitfield() {
    let mut picker = PiecePicker::new(8u32);
    picker.mark_completed(0);
    picker.mark_completed(7);
    let bf = picker.export_bitfield();
    assert_eq!(bf.len(), 1);
    // Bit 0 (MSB) and bit 7 (LSB) set => 10000001 = 0x81
    assert_eq!(bf[0], 0x81);
}

#[test]
fn test_get_piece_info() {
    let mut picker = PiecePicker::new(10u32);
    picker.mark_completed(3);
    picker.set_frequencies_from_peers(&[0, 2, 0, 0, 5, 0, 0, 0, 0, 0]);

    let info = picker.get_piece_info(3).unwrap();
    assert!(info.is_completed);
    assert_eq!(info.index, 3);
    assert_eq!(info.frequency, 0);

    let info4 = picker.get_piece_info(4).unwrap();
    assert!(!info4.is_completed);
    assert_eq!(info4.frequency, 5);

    assert!(picker.get_piece_info(100).is_none());
}

#[test]
fn test_pieces_iter() {
    let picker = PiecePicker::new(5u32);
    let indices: Vec<u32> = picker.pieces_iter().map(|p| p.index).collect();
    assert_eq!(indices, vec![0, 1, 2, 3, 4]);
}

#[test]
fn test_pieces_iter_empty() {
    let picker = PiecePicker::new(0u32);
    let indices: Vec<u32> = picker.pieces_iter().map(|p| p.index).collect();
    assert!(indices.is_empty());
}

#[test]
fn test_export_bitfield_partial() {
    let mut picker = PiecePicker::new(16u32);
    // Set pieces 0, 3, 7
    picker.mark_completed(0);
    picker.mark_completed(3);
    picker.mark_completed(7);
    let bf = picker.export_bitfield();
    // MSB-first: piece0=bit7(0x80), piece3=bit4(0x10), piece7=bit0(0x01)
    // 0x80|0x10|0x01 = 0x91
    assert_eq!(bf.len(), 2);
    assert_eq!(bf[0], 0x91);
    assert_eq!(bf[1], 0x00);
}

#[test]
fn test_is_complete_zero_pieces() {
    let picker = PiecePicker::new(0u32);
    // Zero pieces: vacuously true
    assert!(picker.is_complete());
    assert_eq!(picker.remaining_count(), 0);
}

#[test]
fn test_set_frequencies_from_peers() {
    let mut picker = PiecePicker::new(4u32);
    let freqs: Vec<usize> = vec![3, 1, 5, 2];
    picker.set_frequencies_from_peers(&freqs);

    let info = picker.get_piece_info(2).unwrap();
    assert_eq!(info.frequency, 5);
}

#[test]
fn test_set_frequencies_replaces_short_snapshot() {
    let mut picker = PiecePicker::new(3u32);
    picker.set_frequencies_from_peers(&[9, 8, 7]);
    picker.set_frequencies_from_peers(&[5]);

    assert_eq!(picker.get_piece_info(1).unwrap().frequency, 0);
    assert_eq!(picker.get_piece_info(2).unwrap().frequency, 0);
    assert_eq!(picker.pick_next_without_endgame(), Some(1));
}

// ── Selection behaviour ──────────────────────────────────────────────

#[test]
fn test_sequential_pick_advances_in_order() {
    let mut picker = PiecePicker::new(5u32);
    picker.set_strategy(PieceSelectionStrategy::Sequential);

    for expected in 0..5u32 {
        assert_eq!(picker.pick_next(), Some(expected));
        picker.mark_completed(expected);
    }
    assert_eq!(picker.pick_next(), None);
    assert!(picker.is_complete());
}

#[test]
fn test_sequential_skips_in_progress() {
    let mut picker = PiecePicker::new(4u32);
    picker.set_strategy(PieceSelectionStrategy::Sequential);
    // Disable end-game, which would otherwise re-offer in-flight pieces
    // (a 4-piece torrent is below the default threshold).
    picker.set_endgame_threshold(0);

    picker.mark_in_progress(0, true);
    assert_eq!(picker.pick_next(), Some(1));

    // Releasing piece 0 rewinds the cursor.
    picker.mark_in_progress(0, false);
    assert_eq!(picker.pick_next(), Some(0));
}

#[test]
fn test_tail_mode_picks_from_the_end() {
    let mut picker = PiecePicker::new(6u32);
    picker.set_priority_mode(PiecePriorityMode::SequentialTail);

    assert_eq!(picker.pick_next(), Some(5));
    picker.mark_completed(5);
    assert_eq!(picker.pick_next(), Some(4));
}

#[test]
fn test_head_mode_picks_from_the_start() {
    let mut picker = PiecePicker::new(6u32);
    picker.set_priority_mode(PiecePriorityMode::SequentialHead);

    assert_eq!(picker.pick_next(), Some(0));
    picker.mark_completed(0);
    assert_eq!(picker.pick_next(), Some(1));
}

#[test]
fn test_rarest_first_prefers_lowest_frequency() {
    let mut picker = PiecePicker::new(4u32);
    picker.set_strategy(PieceSelectionStrategy::RarestFirst);
    picker.set_frequencies_from_peers(&[9, 4, 1, 7]);

    assert_eq!(picker.pick_next(), Some(2));
    picker.mark_completed(2);
    assert_eq!(picker.pick_next(), Some(1));
}

#[test]
fn test_rarest_cursor_reopens_piece_after_frequency_order_scan() {
    let mut picker = PiecePicker::new(4u32);
    picker.set_frequencies_from_peers(&[9, 1, 4, 7]);
    picker.mark_in_progress(1, true);

    assert_eq!(picker.pick_next_without_endgame(), Some(2));
    picker.mark_in_progress(1, false);
    assert_eq!(picker.pick_next_without_endgame(), Some(1));
}

#[test]
fn test_pick_next_without_endgame_excludes_in_progress_piece() {
    let mut picker = PiecePicker::new(4u32);
    picker.set_endgame_threshold(4);
    picker.mark_in_progress(0, true);

    assert_eq!(picker.pick_next(), Some(0));
    assert_eq!(picker.pick_next_without_endgame(), Some(1));
}

#[test]
fn test_priority_strategy_prefers_highest_priority() {
    let mut picker = PiecePicker::new(4u32);
    picker.set_strategy(PieceSelectionStrategy::Priority);
    picker.set_priority(3, 9);
    picker.set_priority(1, 5);

    assert_eq!(picker.pick_next(), Some(3));
    picker.mark_completed(3);
    assert_eq!(picker.pick_next(), Some(1));
}

#[test]
fn test_longest_sequence_picks_run_start() {
    let mut picker = PiecePicker::new(8u32);
    picker.set_strategy(PieceSelectionStrategy::LongestSequence);
    // Leave 0 available (run length 1) and 3..=7 available (run length 5).
    picker.mark_completed(1);
    picker.mark_completed(2);

    assert_eq!(picker.pick_next(), Some(3));
}

#[test]
fn test_random_strategy_returns_available_piece() {
    let mut picker = PiecePicker::new(16u32);
    picker.set_strategy(PieceSelectionStrategy::Random);
    for i in 0..15u32 {
        picker.mark_completed(i);
    }
    // Only piece 15 is left, so the random pick is deterministic.
    assert_eq!(picker.pick_next(), Some(15));
}

#[test]
fn test_geometric_strategy_returns_available_piece() {
    let mut picker = PiecePicker::new(32u32);
    picker.set_strategy(PieceSelectionStrategy::Geometric);
    for _ in 0..50 {
        let picked = picker.pick_next().expect("piece must be available");
        assert!(picked < 32);
        assert!(!picker.is_completed(picked));
    }
}

#[test]
fn test_select_respects_peer_bitfield() {
    let mut picker = PiecePicker::new(8u32);
    picker.set_strategy(PieceSelectionStrategy::Sequential);

    // Peer only has piece 5 (MSB-first: bit index 5 => 0b0000_0100).
    let bf = [0b0000_0100u8];
    assert_eq!(picker.select(&bf, 8), Some(5));

    picker.mark_completed(5);
    assert_eq!(picker.select(&bf, 8), None);
}

#[test]
fn test_select_with_zero_nbits_returns_none() {
    let mut picker = PiecePicker::new(8u32);
    assert_eq!(picker.select(&[0xFF], 0), None);
}

#[test]
fn test_select_ignores_bits_beyond_bitfield_length() {
    let mut picker = PiecePicker::new(16u32);
    picker.set_strategy(PieceSelectionStrategy::Sequential);
    // Short bitfield: only the first byte is present.
    let bf = [0x00u8];
    assert_eq!(picker.select(&bf, 16), None);
}

#[test]
fn test_endgame_allows_duplicate_requests() {
    let mut picker = PiecePicker::new(4u32);
    picker.set_strategy(PieceSelectionStrategy::Sequential);
    picker.set_endgame_threshold(4);

    picker.mark_in_progress(0, true);
    picker.mark_in_progress(1, true);
    picker.mark_in_progress(2, true);
    picker.mark_in_progress(3, true);

    // All pieces are in flight, but end-game mode re-offers them.
    assert!(picker.endgame_active());
    assert_eq!(picker.pick_next(), Some(0));
}

#[test]
fn test_endgame_candidates_populated_near_completion() {
    let mut picker = PiecePicker::new(4u32);
    picker.set_endgame_threshold(2);

    picker.mark_completed(0);
    assert!(picker.endgame_candidates().is_empty(), "remaining=3 > 2");

    picker.mark_completed(1);
    assert_eq!(picker.endgame_candidates(), &[2, 3]);

    picker.mark_completed(2);
    picker.mark_completed(3);
    assert!(picker.endgame_candidates().is_empty(), "download finished");
}

#[test]
fn test_mark_completed_is_idempotent() {
    let mut picker = PiecePicker::new(3u32);
    picker.mark_completed(1);
    picker.mark_completed(1);
    picker.mark_completed(1);
    assert_eq!(picker.remaining_count(), 2);
    assert!(!picker.is_complete());
}

#[test]
fn test_pick_next_on_empty_torrent() {
    let mut picker = PiecePicker::new(0u32);
    assert_eq!(picker.pick_next(), None);
    assert!(!picker.endgame_active());
}

#[test]
fn test_mark_in_progress_flag_roundtrip() {
    let mut picker = PiecePicker::new(2u32);
    assert!(!picker.is_in_progress(0));
    picker.mark_in_progress(0, true);
    assert!(picker.is_in_progress(0));
    assert!(picker.get_piece_info(0).unwrap().in_progress);
    picker.mark_in_progress(0, false);
    assert!(!picker.is_in_progress(0));
}

#[test]
fn test_mark_completed_clears_in_progress() {
    let mut picker = PiecePicker::new(2u32);
    picker.mark_in_progress(1, true);
    picker.mark_completed(1);
    assert!(!picker.is_in_progress(1));
    assert!(picker.is_completed(1));
}

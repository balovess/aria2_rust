//! Tests for SegmentMan.

use super::peer_stat::PeerStat;
use super::segment_man_impl::SegmentMan;

use crate::segment::piece_storage::DefaultPieceStorage;

/// Helper: create a SegmentMan with a DefaultPieceStorage.
fn create_segment_man(piece_length: u64, total_length: u64) -> SegmentMan {
    let mut man = SegmentMan::new(piece_length, total_length);
    let storage = DefaultPieceStorage::new(piece_length, total_length);
    man.set_piece_storage(Box::new(storage));
    man
}

// ── Construction ────────────────────────────────────────────────────

#[test]
fn test_new_initializes_ignore_bitfield() {
    let man = SegmentMan::new(1024 * 1024, 10 * 1024 * 1024);
    // All segments should be ignored by default (filter enabled, all bits set)
    assert!(man.all_segments_ignored());
    assert_eq!(man.total_length(), 10 * 1024 * 1024);
}

#[test]
fn test_init_clears_state() {
    let mut man = create_segment_man(1024, 4096);
    man.register_peer_stat(PeerStat::new(1, "host".to_string(), "http".to_string()));
    man.init();
    assert!(man.peer_stats().is_empty());
    assert!(man.used_segment_entries.is_empty());
}

// ── Segment checkout ────────────────────────────────────────────────

#[test]
fn test_get_segment_returns_pieced_segment() {
    let mut man = create_segment_man(1024 * 1024, 10 * 1024 * 1024);
    // Recognize a range so pieces are selectable
    man.recognize_segment_for(0, 10 * 1024 * 1024);

    let segment = man.get_segment(1, 0);
    assert!(segment.is_some());

    let seg = segment.unwrap();
    assert_eq!(seg.index(), 0);
    assert_eq!(seg.length(), 1024 * 1024);
    assert_eq!(seg.position(), 0);
    assert!(!seg.is_complete());
}

#[test]
fn test_get_segment_returns_none_when_all_done() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    // Checkout and complete all 4 pieces
    for _ in 0..4 {
        let seg = man.get_segment(1, 0).unwrap();
        man.complete_segment(1, &seg);
    }

    // No more segments available
    assert!(man.get_segment(1, 0).is_none());
}

#[test]
fn test_get_segment_with_index() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    let seg = man.get_segment_with_index(1, 2);
    assert!(seg.is_some());
    assert_eq!(seg.unwrap().index(), 2);
}

#[test]
fn test_get_segment_with_index_out_of_range() {
    let man = create_segment_man(1024, 4096);
    let mut man = man;
    man.recognize_segment_for(0, 4096);

    assert!(man.get_segment_with_index(1, 10).is_none());
}

// ── Segment cancellation ────────────────────────────────────────────

#[test]
fn test_cancel_segment_by_cuid() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    // Checkout two segments for CUID 1
    // With Default (sparse) stream selector:
    // - First: piece 0 (range [0,4), start=0)
    // - Second: piece 2 (range [1,4), adjusted to midpoint because piece 0 is in-use)
    let seg1 = man.get_segment(1, 0).unwrap();
    let seg2 = man.get_segment(1, 0).unwrap();
    assert_eq!(seg1.index(), 0);
    assert_eq!(seg2.index(), 2); // sparse midpoint

    // Cancel all segments for CUID 1
    man.cancel_segment(1);

    // The pieces should be available again
    let seg3 = man.get_segment(2, 0).unwrap();
    assert_eq!(seg3.index(), 0); // Piece 0 was released
}

#[test]
fn test_cancel_segment_by_segment() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    let seg = man.get_segment(1, 0).unwrap();
    assert_eq!(seg.index(), 0);

    // Cancel the specific segment
    man.cancel_segment_by_segment(1, &seg);

    // Piece should be available again
    let seg2 = man.get_segment(2, 0).unwrap();
    assert_eq!(seg2.index(), 0);
}

#[test]
fn test_cancel_segment_by_index() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    let seg = man.get_segment(1, 0).unwrap();
    assert_eq!(seg.index(), 0);

    // Cancel by piece index
    let cancelled = man.cancel_segment_by_index(0);
    assert!(cancelled);

    // Piece should be available again
    let seg2 = man.get_segment(2, 0).unwrap();
    assert_eq!(seg2.index(), 0);

    // Since we just checked out piece 0 for CUID 2,
    // cancel_segment_by_index(0) should succeed again
    let cancelled2 = man.cancel_segment_by_index(0);
    assert!(cancelled2);

    // No more entries for piece 0 — should return false
    let cancelled3 = man.cancel_segment_by_index(0);
    assert!(!cancelled3);
}

#[test]
fn test_cancel_all_segments() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    let _seg1 = man.get_segment(1, 0).unwrap();
    let _seg2 = man.get_segment(1, 0).unwrap();

    assert_eq!(man.used_segment_entries.len(), 2);
    man.cancel_all_segments();
    assert!(man.used_segment_entries.is_empty());
}

// ── Segment completion ──────────────────────────────────────────────

#[test]
fn test_complete_segment() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    let seg = man.get_segment(1, 0).unwrap();
    let result = man.complete_segment(1, &seg);
    assert!(result);
    assert!(man.has_segment(0));
    assert!(man.used_segment_entries.is_empty());
}

// ── Download progress ───────────────────────────────────────────────

#[test]
fn test_download_finished() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    assert!(!man.download_finished());

    for i in 0..4 {
        let seg = man.get_segment(1, 0).unwrap();
        assert_eq!(seg.index(), i);
        man.complete_segment(1, &seg);
    }

    assert!(man.download_finished());
}

#[test]
fn test_download_length() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    assert_eq!(man.download_length(), 0);

    let seg = man.get_segment(1, 0).unwrap();
    man.complete_segment(1, &seg);

    assert_eq!(man.download_length(), 1024);
}

// ── Peer statistics ─────────────────────────────────────────────────

#[test]
fn test_register_and_get_peer_stat() {
    let mut man = create_segment_man(1024, 4096);
    let stat = PeerStat::new(42, "example.com".to_string(), "http".to_string());
    man.register_peer_stat(stat);

    let found = man.get_peer_stat(42);
    assert!(found.is_some());
    assert_eq!(found.unwrap().hostname, "example.com");

    assert!(man.get_peer_stat(99).is_none());
}

#[test]
fn test_update_fastest_peer_stat() {
    let mut man = create_segment_man(1024, 4096);

    let mut stat1 = PeerStat::new(1, "host".to_string(), "http".to_string());
    stat1.avg_download_speed = 1000;
    stat1.session_download_length = 5000;
    man.update_fastest_peer_stat(&stat1);

    let mut stat2 = PeerStat::new(2, "host".to_string(), "http".to_string());
    stat2.avg_download_speed = 2000;
    stat2.session_download_length = 3000;
    man.update_fastest_peer_stat(&stat2);

    // stat2 is faster, so it should replace stat1
    // but session_download_length should be accumulated
    let fastest = &man.fastest_peer_stats()[0];
    assert_eq!(fastest.avg_download_speed, 2000);
    assert_eq!(fastest.session_download_length, 8000); // 5000 + 3000
}

// ── Ignore bitfield ─────────────────────────────────────────────────

#[test]
fn test_ignore_and_recognize_segments() {
    let mut man = create_segment_man(1024, 4096);
    // By default all segments are ignored
    assert!(man.all_segments_ignored());

    // Recognize a range
    man.recognize_segment_for(0, 2048);
    assert!(!man.all_segments_ignored());

    // Ignore it again
    man.ignore_segment_for(0, 2048);
    assert!(man.all_segments_ignored());
}

#[test]
fn test_get_segment_respects_ignore_bitfield() {
    let mut man = create_segment_man(1024, 4096);
    // By default all segments are ignored — get_segment should return None
    assert!(man.get_segment(1, 0).is_none());

    // Recognize one piece
    man.recognize_segment_for(0, 1024);
    let seg = man.get_segment(1, 0);
    assert!(seg.is_some());
    assert_eq!(seg.unwrap().index(), 0);
}

// ── Written length memo ─────────────────────────────────────────────

#[test]
fn test_erase_segment_written_length_memo() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    let seg = man.get_segment(1, 0).unwrap();
    man.cancel_segment_by_segment(1, &seg);
    assert_eq!(man.segment_written_length_memo.len(), 1);

    man.erase_segment_written_length_memo();
    assert!(man.segment_written_length_memo.is_empty());
}

// ── Count free pieces ───────────────────────────────────────────────

#[test]
fn test_count_free_piece_from() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    assert_eq!(man.count_free_piece_from(0), 4);

    let _seg = man.get_segment(1, 0).unwrap();
    // Piece 0 is in-use but not completed
    assert_eq!(man.count_free_piece_from(0), 0);
}

// ── In-flight segment indices ───────────────────────────────────────

#[test]
fn test_get_in_flight_segment_indices() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    // With Default (sparse) stream selector:
    // - First: piece 0
    // - Second: piece 2 (midpoint because piece 0 is in-use)
    // - Third for CUID 2: piece 1 or 3 (depends on remaining ranges)
    let _seg1 = man.get_segment(1, 0).unwrap();
    let _seg2 = man.get_segment(1, 0).unwrap();
    let _seg3 = man.get_segment(2, 0).unwrap();

    let cuid1_indices = man.get_in_flight_segment_indices(1);
    // CUID 1 got pieces 0 and 2
    assert_eq!(cuid1_indices, vec![0, 2]);

    let cuid2_indices = man.get_in_flight_segment_indices(2);
    // CUID 2 got the next available piece
    assert!(!cuid2_indices.is_empty());
}

// ── Full download lifecycle ─────────────────────────────────────────

#[test]
fn test_full_download_lifecycle() {
    let mut man = create_segment_man(1024 * 1024, 5 * 1024 * 1024);
    man.recognize_segment_for(0, 5 * 1024 * 1024);

    // Simulate downloading all 5 pieces.
    // Order depends on the stream piece selector strategy.
    // Sparse selector may not return pieces in sequential order.
    let mut downloaded_indices = Vec::new();
    for _ in 0..5 {
        let mut seg = man.get_segment(1, 0).unwrap();
        downloaded_indices.push(seg.index());
        assert!(!seg.is_complete());

        // Simulate writing data
        seg.update_written_length(1024 * 1024);
        assert!(seg.is_complete());

        // Complete the segment
        let result = man.complete_segment(1, &seg);
        assert!(result);
    }

    // All 5 distinct pieces should have been downloaded
    assert_eq!(downloaded_indices.len(), 5);
    downloaded_indices.sort();
    assert_eq!(downloaded_indices, vec![0, 1, 2, 3, 4]);

    assert!(man.download_finished());
    assert_eq!(man.download_length(), 5 * 1024 * 1024);
}

// ── complete_segment advertises piece ──────────────────────────────────

#[test]
fn test_complete_segment_advertises_piece() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    // Complete a segment — should advertise the piece
    let seg = man.get_segment(1, 0).unwrap();
    let piece_index = seg.index();
    let result = man.complete_segment(1, &seg);
    assert!(result);
    assert!(man.has_segment(piece_index));

    // Verify advertisement via PieceStorage delegation
    // get_advertised_piece_indexes should return the completed piece
    let (indexes, _) = man.piece_storage.as_ref().unwrap().get_advertised_piece_indexes(999, 0);
    assert!(indexes.contains(&piece_index));
}

#[test]
fn test_complete_segment_advertises_multiple_pieces() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    // Complete all 4 pieces
    let mut completed = Vec::new();
    for _ in 0..4 {
        let seg = man.get_segment(1, 0).unwrap();
        completed.push(seg.index());
        man.complete_segment(1, &seg);
    }

    // All 4 pieces should be advertised
    let (indexes, _) = man.piece_storage.as_ref().unwrap().get_advertised_piece_indexes(999, 0);
    assert_eq!(indexes.len(), 4);
    for idx in &completed {
        assert!(indexes.contains(idx));
    }
}

// ── get_segments_for_file_entry ────────────────────────────────────────

#[test]
fn test_get_segments_for_file_entry_basic() {
    // 8 pieces of 1024 bytes each = 8192 total
    let mut man = create_segment_man(1024, 8192);
    man.recognize_segment_for(0, 8192);

    // File entry covers pieces 2-4 (offset=2048, length=3072)
    let segments = man.get_segments_for_file_entry(1, 0, 2048, 3072, 3);
    assert!(!segments.is_empty());
    // All returned segments should have position_to_write within [2048, 5120)
    for seg in &segments {
        let pos = seg.position_to_write();
        assert!(pos >= 2048, "segment pos {} should be >= 2048", pos);
        assert!(pos < 5120, "segment pos {} should be < 5120", pos);
    }
}

#[test]
fn test_get_segments_for_file_entry_max_segments() {
    let mut man = create_segment_man(1024, 8192);
    man.recognize_segment_for(0, 8192);

    // Request only 1 segment for a file that covers pieces 0-3
    let segments = man.get_segments_for_file_entry(1, 0, 0, 4096, 1);
    assert_eq!(segments.len(), 1);
}

#[test]
fn test_get_segments_for_file_entry_zero_max() {
    let mut man = create_segment_man(1024, 8192);
    man.recognize_segment_for(0, 8192);

    // max_segments=0 should return empty
    let segments = man.get_segments_for_file_entry(1, 0, 0, 4096, 0);
    assert!(segments.is_empty());
}

#[test]
fn test_get_segments_for_file_entry_zero_length() {
    let mut man = create_segment_man(1024, 8192);
    man.recognize_segment_for(0, 8192);

    // file_length=0 should return empty
    let segments = man.get_segments_for_file_entry(1, 0, 0, 0, 5);
    assert!(segments.is_empty());
}

#[test]
fn test_get_segments_for_file_entry_respects_ignore() {
    let mut man = create_segment_man(1024, 8192);
    // By default all segments are ignored
    let segments = man.get_segments_for_file_entry(1, 0, 0, 4096, 3);
    assert!(segments.is_empty());
}

#[test]
fn test_get_segments_for_file_entry_multi_file() {
    // Simulate a multi-file torrent:
    // Total = 8192 bytes, piece_length = 1024, 8 pieces
    // File A: offset=0, length=3072 (pieces 0-2)
    // File B: offset=3072, length=5120 (pieces 3-7)
    let mut man = create_segment_man(1024, 8192);
    man.recognize_segment_for(0, 8192);

    // Get segments for File B (offset=3072, length=5120)
    let segments_b = man.get_segments_for_file_entry(1, 0, 3072, 5120, 5);
    // All segments should fall within File B's range [3072, 8192)
    for seg in &segments_b {
        let pos = seg.position_to_write();
        assert!(pos >= 3072, "segment pos {} should be >= 3072", pos);
        assert!(pos < 8192, "segment pos {} should be < 8192", pos);
    }

    // Get segments for File A (offset=0, length=3072)
    let segments_a = man.get_segments_for_file_entry(2, 0, 0, 3072, 3);
    for seg in &segments_a {
        let pos = seg.position_to_write();
        // pos is u64, always >= 0
        assert!(pos < 3072, "segment pos {} should be < 3072", pos);
    }
}

// ── Have advertisement (BT-specific) ──────────────────────────────────

#[cfg(feature = "bittorrent")]
#[test]
fn test_advertise_piece_delegation() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    // Advertise piece 0 by CUID 1
    man.advertise_piece(1, 0);

    // Query from another CUID — should see piece 0
    let (indexes, new_last) = man.get_advertised_piece_indexes(2, 0);
    assert!(indexes.contains(&0));
    assert!(new_last > 0);
}

#[cfg(feature = "bittorrent")]
#[test]
fn test_get_advertised_piece_indexes_excludes_own_cuid() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    // CUID 1 completes piece 0
    man.advertise_piece(1, 0);

    // CUID 1 querying should NOT see its own advertisement
    let (indexes, _) = man.get_advertised_piece_indexes(1, 0);
    assert!(!indexes.contains(&0));

    // CUID 2 querying should see piece 0
    let (indexes2, _) = man.get_advertised_piece_indexes(2, 0);
    assert!(indexes2.contains(&0));
}

#[cfg(feature = "bittorrent")]
#[test]
fn test_get_advertised_piece_indexes_since_last() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    // Advertise pieces 0 and 1
    man.advertise_piece(1, 0);
    man.advertise_piece(1, 1);

    // Get all indexes
    let (indexes, new_last) = man.get_advertised_piece_indexes(2, 0);
    assert_eq!(indexes.len(), 2);

    // Now advertise piece 2
    man.advertise_piece(1, 2);

    // Only new entries since new_last should appear
    let (indexes2, _) = man.get_advertised_piece_indexes(2, new_last);
    assert!(indexes2.contains(&2));
    assert!(!indexes2.contains(&0));
    assert!(!indexes2.contains(&1));
}

#[cfg(feature = "bittorrent")]
#[test]
fn test_remove_advertised_piece() {
    let mut man = create_segment_man(1024, 4096);
    man.recognize_segment_for(0, 4096);

    // Advertise a piece
    man.advertise_piece(1, 0);

    // Verify it's there
    let (indexes, _) = man.get_advertised_piece_indexes(2, 0);
    assert!(indexes.contains(&0));

    // Remove all entries with registered_time <= far future (effectively all)
    let far_future_ms = std::u64::MAX;
    man.remove_advertised_piece(far_future_ms);

    // Should be empty now
    let (indexes2, _) = man.get_advertised_piece_indexes(2, 0);
    assert!(indexes2.is_empty());
}

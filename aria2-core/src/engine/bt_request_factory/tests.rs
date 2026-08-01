//! Tests for BtRequestFactory.

#![allow(dead_code)] // test infrastructure reused across suites

use super::*;
use crate::segment::piece::Piece;
use std::sync::Arc;

// -- Mock PieceStorageProvider for testing --

/// Mock piece storage provider that tracks cancel_piece calls.
#[derive(Debug)]
struct MockPieceStorage {
    cancelled: Arc<std::sync::Mutex<Vec<(usize, u64)>>>,
}

impl MockPieceStorage {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn cancelled_pieces(&self) -> Vec<(usize, u64)> {
        self.cancelled.lock().unwrap().clone()
    }
}

impl PieceStorageProvider for MockPieceStorage {
    fn cancel_piece(&self, piece_index: usize, cuid: u64) {
        self.cancelled.lock().unwrap().push((piece_index, cuid));
    }
}

// -- Helper: create a piece with the given index and length --

fn make_piece(index: usize, length: u64) -> Piece {
    Piece::new(index, length)
}

// -- Construction tests --

#[test]
fn test_new_factory() {
    let factory = BtRequestFactory::new(16384);
    assert_eq!(factory.count_target_piece(), 0);
    assert_eq!(factory.count_missing_block(), 0);
    assert!(factory.get_target_piece_indexes().is_empty());
}

#[test]
fn test_set_cuid() {
    let mut factory = BtRequestFactory::new(16384);
    factory.set_cuid(42);
    assert_eq!(factory.cuid, 42);
}

// -- Add/remove target piece tests --

#[test]
fn test_add_target_piece() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536));
    factory.add_target_piece(make_piece(1, 65536));
    assert_eq!(factory.count_target_piece(), 2);
    assert_eq!(factory.get_target_piece_indexes(), vec![0, 1]);
}

#[test]
fn test_remove_target_piece() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536));
    factory.add_target_piece(make_piece(1, 65536));
    factory.add_target_piece(make_piece(2, 65536));

    let removed = factory.remove_target_piece(1);
    assert!(removed.is_some());
    assert_eq!(removed.unwrap().index(), 1);
    assert_eq!(factory.count_target_piece(), 2);
    assert_eq!(factory.get_target_piece_indexes(), vec![0, 2]);
}

#[test]
fn test_remove_target_piece_not_found() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536));
    let removed = factory.remove_target_piece(99);
    assert!(removed.is_none());
    assert_eq!(factory.count_target_piece(), 1);
}

#[test]
fn test_remove_target_piece_cancels_in_storage() {
    let mut factory = BtRequestFactory::new(16384);
    let mock_storage = MockPieceStorage::new();
    factory.set_piece_storage(Box::new(MockPieceStorage {
        cancelled: mock_storage.cancelled.clone(),
    }));
    factory.set_cuid(42);

    factory.add_target_piece(make_piece(5, 65536));
    factory.remove_target_piece(5);

    let cancelled = mock_storage.cancelled_pieces();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0], (5, 42));
}

#[test]
fn test_remove_all_target_pieces() {
    let mut factory = BtRequestFactory::new(16384);
    let mock_storage = MockPieceStorage::new();
    factory.set_piece_storage(Box::new(MockPieceStorage {
        cancelled: mock_storage.cancelled.clone(),
    }));
    factory.set_cuid(10);

    factory.add_target_piece(make_piece(0, 65536));
    factory.add_target_piece(make_piece(1, 65536));
    factory.add_target_piece(make_piece(2, 65536));

    let removed = factory.remove_all_target_pieces();
    assert_eq!(removed.len(), 3);
    assert_eq!(factory.count_target_piece(), 0);

    // All pieces should be cancelled in storage
    let cancelled = mock_storage.cancelled_pieces();
    assert_eq!(cancelled.len(), 3);
    assert_eq!(cancelled[0], (0, 10));
    assert_eq!(cancelled[1], (1, 10));
    assert_eq!(cancelled[2], (2, 10));
}

#[test]
fn test_remove_all_target_pieces_empty() {
    let mut factory = BtRequestFactory::new(16384);
    let removed = factory.remove_all_target_pieces();
    assert!(removed.is_empty());
}

// -- Count missing block tests --

#[test]
fn test_count_missing_block_empty() {
    let factory = BtRequestFactory::new(16384);
    assert_eq!(factory.count_missing_block(), 0);
}

#[test]
fn test_count_missing_block_aggregation() {
    let mut factory = BtRequestFactory::new(16384);
    // Two pieces with 4 blocks each = 8 missing blocks
    factory.add_target_piece(make_piece(0, 65536));
    factory.add_target_piece(make_piece(1, 65536));
    assert_eq!(factory.count_missing_block(), 8);
}

#[test]
fn test_count_missing_block_after_partial_completion() {
    let mut factory = BtRequestFactory::new(16384);
    let mut piece = make_piece(0, 65536);
    piece.complete_block(0); // 1 of 4 blocks complete
    piece.complete_block(1); // 2 of 4 blocks complete
    factory.add_target_piece(piece);
    assert_eq!(factory.count_missing_block(), 2);
}

// -- Remove completed piece tests --

#[test]
fn test_remove_completed_piece() {
    let mut factory = BtRequestFactory::new(16384);

    let mut piece0 = make_piece(0, 65536);
    piece0.complete_block(0);
    piece0.complete_block(1);
    piece0.complete_block(2);
    piece0.complete_block(3);

    let piece1 = make_piece(1, 65536); // Not complete

    factory.add_target_piece(piece0);
    factory.add_target_piece(piece1);

    let removed = factory.remove_completed_piece();
    assert_eq!(removed, vec![0]);
    assert_eq!(factory.count_target_piece(), 1);
    assert_eq!(factory.get_target_piece_indexes(), vec![1]);
}

#[test]
fn test_remove_completed_piece_none_complete() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536));
    let removed = factory.remove_completed_piece();
    assert!(removed.is_empty());
    assert_eq!(factory.count_target_piece(), 1);
}

#[test]
fn test_remove_completed_piece_all_complete() {
    let mut factory = BtRequestFactory::new(16384);

    let mut piece0 = make_piece(0, 65536);
    let mut piece1 = make_piece(1, 65536);
    // Complete all blocks
    for i in 0..4 {
        piece0.complete_block(i);
        piece1.complete_block(i);
    }

    factory.add_target_piece(piece0);
    factory.add_target_piece(piece1);

    let removed = factory.remove_completed_piece();
    assert_eq!(removed.len(), 2);
    assert!(removed.contains(&0));
    assert!(removed.contains(&1));
    assert_eq!(factory.count_target_piece(), 0);
}

// -- do_choked_action tests --

#[test]
fn test_do_choked_action_removes_non_allowed() {
    let mut factory = BtRequestFactory::new(16384);
    let mock_storage = MockPieceStorage::new();
    factory.set_piece_storage(Box::new(MockPieceStorage {
        cancelled: mock_storage.cancelled.clone(),
    }));
    factory.set_cuid(5);

    factory.add_target_piece(make_piece(0, 65536));
    factory.add_target_piece(make_piece(1, 65536));
    factory.add_target_piece(make_piece(2, 65536));

    // Only piece 1 is in allowed-fast
    let removed = factory.do_choked_action(|idx| idx == 1);
    assert_eq!(removed.len(), 2);
    assert!(removed.contains(&0));
    assert!(removed.contains(&2));
    assert_eq!(factory.count_target_piece(), 1);
    assert_eq!(factory.get_target_piece_indexes(), vec![1]);

    // Cancelled in storage
    let cancelled = mock_storage.cancelled_pieces();
    assert_eq!(cancelled.len(), 2);
}

#[test]
fn test_do_choked_action_all_allowed() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536));
    factory.add_target_piece(make_piece(1, 65536));

    let removed = factory.do_choked_action(|_| true);
    assert!(removed.is_empty());
    assert_eq!(factory.count_target_piece(), 2);
}

#[test]
fn test_do_choked_action_none_allowed() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536));
    factory.add_target_piece(make_piece(1, 65536));

    let removed = factory.do_choked_action(|_| false);
    assert_eq!(removed.len(), 2);
    assert_eq!(factory.count_target_piece(), 0);
}

// -- create_request_messages normal mode tests --

#[test]
fn test_create_request_messages_normal() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536)); // 4 blocks

    let requests = factory.create_request_messages(2, false, |_, _| false);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].index, 0);
    assert_eq!(requests[0].begin, 0);
    assert_eq!(requests[0].length, 16384);
    assert_eq!(requests[1].index, 0);
    assert_eq!(requests[1].begin, 16384);
    assert_eq!(requests[1].length, 16384);
}

#[test]
fn test_create_request_messages_normal_multiple_pieces() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 32768)); // 2 blocks
    factory.add_target_piece(make_piece(1, 32768)); // 2 blocks

    let requests = factory.create_request_messages(3, false, |_, _| false);
    assert_eq!(requests.len(), 3);
    // First 2 from piece 0
    assert_eq!(requests[0].index, 0);
    assert_eq!(requests[1].index, 0);
    // Third from piece 1
    assert_eq!(requests[2].index, 1);
}

#[test]
fn test_create_request_messages_max_count_zero() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536));

    let requests = factory.create_request_messages(0, false, |_, _| false);
    assert!(requests.is_empty());
}

#[test]
fn test_create_request_messages_marks_blocks_in_use() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536)); // 4 blocks

    // First call: request 2 blocks
    let requests = factory.create_request_messages(2, false, |_, _| false);
    assert_eq!(requests.len(), 2);

    // Second call: blocks 0 and 1 are already in-use, so we get blocks 2 and 3
    let requests2 = factory.create_request_messages(2, false, |_, _| false);
    assert_eq!(requests2.len(), 2);
    assert_eq!(requests2[0].begin, 32768);
    assert_eq!(requests2[1].begin, 49152);

    // Third call: all blocks are either completed or in-use
    let requests3 = factory.create_request_messages(2, false, |_, _| false);
    assert!(requests3.is_empty());
}

#[test]
fn test_create_request_messages_empty_factory() {
    let mut factory = BtRequestFactory::new(16384);
    let requests = factory.create_request_messages(10, false, |_, _| false);
    assert!(requests.is_empty());
}

#[test]
fn test_create_request_messages_all_blocks_in_use() {
    let mut factory = BtRequestFactory::new(16384);
    let mut piece = make_piece(0, 32768); // 2 blocks
    // Mark both blocks as in-use
    piece.set_block_in_use(0);
    piece.set_block_in_use(1);
    factory.add_target_piece(piece);

    let requests = factory.create_request_messages(2, false, |_, _| false);
    assert!(requests.is_empty());
}

// -- create_request_messages endgame mode tests --

#[test]
fn test_create_request_messages_endgame() {
    let mut factory = BtRequestFactory::new(16384);
    let mut piece = make_piece(0, 65536); // 4 blocks
    // Complete block 0, leave blocks 1-3 missing
    piece.complete_block(0);
    factory.add_target_piece(piece);

    // In endgame mode, missing blocks 1-3 should be requested
    // (blocks 1-3 are all "missing" even if in-use)
    let requests = factory.create_request_messages(10, true, |_, _| false);
    // Should get requests for the 3 missing blocks
    assert_eq!(requests.len(), 3);
    // Verify all are for piece 0
    for req in &requests {
        assert_eq!(req.index, 0);
    }
}

#[test]
fn test_create_request_messages_endgame_skips_outstanding() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536)); // 4 blocks

    // Block 1 is outstanding (already requested by another peer)
    let requests = factory.create_request_messages(10, true, |piece_idx, block_idx| {
        piece_idx == 0 && block_idx == 1
    });

    // Should get requests for blocks 0, 2, 3 (block 1 is outstanding)
    assert_eq!(requests.len(), 3);
    let requested_blocks: Vec<u32> = requests.iter().map(|r| r.begin / 16384).collect();
    assert!(!requested_blocks.contains(&1));
}

#[test]
fn test_create_request_messages_endgame_max_count() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536)); // 4 blocks

    let requests = factory.create_request_messages(2, true, |_, _| false);
    assert_eq!(requests.len(), 2);
}

#[test]
fn test_create_request_messages_endgame_all_outstanding() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536)); // 4 blocks

    // All blocks are outstanding
    let requests = factory.create_request_messages(10, true, |_, _| true);
    assert!(requests.is_empty());
}

#[test]
fn test_create_request_messages_endgame_does_not_mark_in_use() {
    // In endgame mode, blocks are NOT marked as in-use on the Piece,
    // because multiple peers may request the same block.
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 32768)); // 2 blocks

    let _ = factory.create_request_messages(10, true, |_, _| false);

    // Verify the blocks are NOT marked as in-use (endgame doesn't mark)
    // We check by calling normal mode and seeing all blocks are still available
    let requests = factory.create_request_messages(2, false, |_, _| false);
    assert_eq!(requests.len(), 2);
}

// -- Edge case tests --

#[test]
fn test_get_target_piece_indexes() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(5, 65536));
    factory.add_target_piece(make_piece(10, 65536));
    factory.add_target_piece(make_piece(3, 65536));

    let indexes = factory.get_target_piece_indexes();
    assert_eq!(indexes, vec![5, 10, 3]); // Order preserved (FIFO)
}

#[test]
fn test_create_request_messages_piece_with_non_aligned_length() {
    let mut factory = BtRequestFactory::new(16384);
    // 50000 bytes = 4 blocks (last block is 848 bytes)
    factory.add_target_piece(make_piece(0, 50000));

    let requests = factory.create_request_messages(4, false, |_, _| false);
    assert_eq!(requests.len(), 4);
    // Last block length should be 848
    assert_eq!(requests[3].length, 848);
    assert_eq!(requests[3].begin, 49152);
}

#[test]
fn test_count_target_piece() {
    let mut factory = BtRequestFactory::new(16384);
    assert_eq!(factory.count_target_piece(), 0);
    factory.add_target_piece(make_piece(0, 65536));
    assert_eq!(factory.count_target_piece(), 1);
    factory.add_target_piece(make_piece(1, 65536));
    assert_eq!(factory.count_target_piece(), 2);
    factory.remove_target_piece(0);
    assert_eq!(factory.count_target_piece(), 1);
}

#[test]
fn test_remove_all_then_add() {
    let mut factory = BtRequestFactory::new(16384);
    factory.add_target_piece(make_piece(0, 65536));
    factory.add_target_piece(make_piece(1, 65536));
    factory.remove_all_target_pieces();
    assert_eq!(factory.count_target_piece(), 0);

    factory.add_target_piece(make_piece(2, 65536));
    assert_eq!(factory.count_target_piece(), 1);
    assert_eq!(factory.get_target_piece_indexes(), vec![2]);
}

#[test]
fn test_do_choked_action_empty_factory() {
    let mut factory = BtRequestFactory::new(16384);
    let removed = factory.do_choked_action(|_| false);
    assert!(removed.is_empty());
}

#[test]
fn test_create_request_messages_endgame_empty_factory() {
    let mut factory = BtRequestFactory::new(16384);
    let requests = factory.create_request_messages(10, true, |_, _| false);
    assert!(requests.is_empty());
}

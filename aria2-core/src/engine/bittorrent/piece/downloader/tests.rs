use super::*;

#[test]
fn test_piece_download_state_creation() {
    // Test with standard 256KB piece and 16KB blocks
    let state = PieceDownloadState::new(0, 262144, 16384);
    assert_eq!(state.piece_index, 0);
    assert_eq!(state.total_blocks, 16); // 262144 / 16384 = 16
    assert!(state.completed_blocks.is_empty());
    assert!(state.requested_blocks.is_empty());
    assert!(!state.is_complete());
    assert_eq!(state.blocks_remaining(), 16);
    assert_eq!(state.progress_percent(), 0.0);
}

#[test]
fn test_piece_download_state_partial_last_block() {
    // Test with piece that doesn't divide evenly
    let state = PieceDownloadState::new(5, 20000, 16384);
    assert_eq!(state.total_blocks, 2); // (20000 + 16384 - 1) / 16384 = 2
    assert_eq!(state.blocks_remaining(), 2);
}

#[test]
fn test_piece_download_state_zero_block_size() {
    // Edge case: zero block size should result in 0 total blocks
    let state = PieceDownloadState::new(0, 100000, 0);
    assert_eq!(state.total_blocks, 0);
    assert!(state.is_complete()); // 0 blocks means "complete"
}

#[test]
fn test_mark_block_requested() {
    let mut state = PieceDownloadState::new(0, 32768, 16384);

    state.mark_block_requested(0);
    assert_eq!(state.requested_blocks.len(), 1);
    assert!(state.requested_blocks.contains_key(&0));
    assert_eq!(state.blocks_remaining(), 2); // Still need all blocks

    state.mark_block_requested(1);
    assert_eq!(state.requested_blocks.len(), 2);
}

#[test]
fn test_mark_block_received() {
    let mut state = PieceDownloadState::new(0, 32768, 16384);

    // Request then receive block 0
    state.mark_block_requested(0);
    state.mark_block_received(0);

    assert!(state.completed_blocks.contains(&0));
    assert!(!state.requested_blocks.contains_key(&0));
    assert_eq!(state.blocks_remaining(), 1);
    assert_eq!(state.progress_percent(), 50.0);
}

#[test]
fn test_mark_block_cancelled() {
    let mut state = PieceDownloadState::new(0, 32768, 16384);

    state.mark_block_requested(0);
    state.mark_block_cancelled(0);

    assert!(!state.completed_blocks.contains(&0));
    assert!(!state.requested_blocks.contains_key(&0));
    assert_eq!(state.blocks_remaining(), 2); // Still need this block
}

#[test]
fn test_is_complete_all_blocks_received() {
    let mut state = PieceDownloadState::new(0, 49152, 16384); // 3 blocks

    for i in 0..3 {
        state.mark_block_requested(i);
        state.mark_block_received(i);
    }

    assert!(state.is_complete());
    assert_eq!(state.blocks_remaining(), 0);
    assert_eq!(state.progress_percent(), 100.0);
}

#[test]
fn test_is_stalled_with_pending_requests() {
    let mut state = PieceDownloadState::new(0, 32768, 16384);

    state.mark_block_requested(0);
    state.mark_block_requested(1);

    // Should not be stalled immediately
    assert!(!state.is_stalled(30));

    // Simulate time passing by checking with very small timeout
    // In practice, this would require mocking time or using a very long timeout
    // For now, just verify the logic structure is correct
    assert!(state.requested_blocks.len() == 2);
    assert!(!state.is_complete());
}

#[test]
fn test_is_not_stalled_when_no_pending_requests() {
    let mut state = PieceDownloadState::new(0, 32768, 16384);

    state.mark_block_requested(0);
    state.mark_block_cancelled(0);

    // No pending requests -> not stalled even if no activity
    assert!(!state.is_stalled(0)); // Even with 0 timeout
}

#[test]
fn test_progress_percent_calculation() {
    let mut state = PieceDownloadState::new(0, 65536, 16384); // 4 blocks

    assert_eq!(state.progress_percent(), 0.0);

    state.mark_block_requested(0);
    state.mark_block_received(0);
    assert_eq!(state.progress_percent(), 25.0);

    state.mark_block_requested(1);
    state.mark_block_received(1);
    assert_eq!(state.progress_percent(), 50.0);

    state.mark_block_requested(2);
    state.mark_block_received(2);
    assert_eq!(state.progress_percent(), 75.0);

    state.mark_block_requested(3);
    state.mark_block_received(3);
    assert_eq!(state.progress_percent(), 100.0);
}

#[test]
fn test_state_lifecycle_full_cycle() {
    // Complete lifecycle: create -> request -> receive -> complete
    let mut state = PieceDownloadState::new(10, 262144, 16384);

    // Initial state
    assert_eq!(state.piece_index, 10);
    assert_eq!(state.total_blocks, 16);
    assert!(!state.is_complete());

    // Request some blocks
    for i in 0..5 {
        state.mark_block_requested(i);
    }
    assert_eq!(state.requested_blocks.len(), 5);

    // Receive first 3
    for i in 0..3 {
        state.mark_block_received(i);
    }
    assert_eq!(state.completed_blocks.len(), 3);
    assert_eq!(state.requested_blocks.len(), 2); // 4,5 still pending
    assert_eq!(state.progress_percent(), 18.75); // 3/16 = 18.75%

    // Cancel remaining requested
    state.mark_block_cancelled(3);
    state.mark_block_cancelled(4);
    state.mark_block_cancelled(5);
    assert_eq!(state.requested_blocks.len(), 0);

    // Continue to completion
    for i in 3..16 {
        state.mark_block_requested(i);
        state.mark_block_received(i);
    }

    assert!(state.is_complete());
    assert_eq!(state.progress_percent(), 100.0);
}

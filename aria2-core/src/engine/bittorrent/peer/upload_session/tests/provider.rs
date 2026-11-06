use super::*;

#[tokio::test]
async fn test_in_memory_provider_creation() {
    let provider = InMemoryPieceProvider::new(16384, 10);
    assert_eq!(provider.num_pieces(), 10);
    assert_eq!(provider.piece_length(), 16384);
    assert!(!provider.has_piece(0));
    assert!(provider.get_piece_data(0, 0, 100).await.is_none());
}

#[tokio::test]
async fn test_in_memory_provider_set_and_get() {
    let mut provider = InMemoryPieceProvider::new(256, 4);
    assert_eq!(provider.piece_length(), 256);
    provider.set_piece_data(0, vec![0xAB; 256]);
    provider.set_piece_data(2, vec![0xCD; 128]);

    assert!(provider.has_piece(0));
    assert!(!provider.has_piece(1));
    assert!(provider.has_piece(2));

    let data = provider.get_piece_data(0, 10, 50).await.unwrap();
    assert_eq!(data.len(), 50);
    assert!(data.iter().all(|&b| b == 0xAB));

    let partial = provider.get_piece_data(2, 100, 28).await.unwrap();
    assert_eq!(partial.len(), 28);
    assert!(partial.iter().all(|&b| b == 0xCD));
}

#[tokio::test]
async fn test_in_memory_provider_set_all_from_pattern() {
    let mut provider = InMemoryPieceProvider::new(100, 5);
    provider
        .set_all_from_pattern(|piece_idx, byte_idx| ((piece_idx * 37 + byte_idx * 13) % 256) as u8);

    for i in 0..5u32 {
        assert!(provider.has_piece(i));
        let data = provider.get_piece_data(i, 0, 100).await.unwrap();
        for (j, &byte) in data.iter().enumerate() {
            assert_eq!(byte, ((i * 37 + j as u32 * 13) % 256) as u8);
        }
    }
}

#[tokio::test]
async fn pattern_provider_uses_piece_length_and_explicit_short_tail() {
    let piece_length = 16;
    let mut provider = InMemoryPieceProvider::new(piece_length, 3);
    provider
        .set_all_from_pattern(|piece_idx, byte_idx| ((piece_idx * 37 + byte_idx * 13) % 256) as u8);

    for piece_index in 0..3 {
        let data = provider
            .get_piece_data(piece_index, 0, piece_length)
            .await
            .unwrap();
        assert_eq!(data.len(), piece_length as usize);
        assert!(
            provider
                .get_piece_data(piece_index, piece_length, 1)
                .await
                .is_none(),
            "generated piece {piece_index} must not extend past piece_length"
        );
    }

    let short_tail = vec![0xe1; 5];
    provider.set_piece_data(2, short_tail.clone());
    assert_eq!(provider.get_piece_data(2, 0, 5).await.unwrap(), short_tail);
    assert!(provider.get_piece_data(2, 5, 1).await.is_none());
}

#[tokio::test]
async fn test_in_memory_provider_offset_beyond_piece() {
    let mut provider = InMemoryPieceProvider::new(50, 2);
    provider.set_piece_data(0, vec![0x42; 50]);

    assert!(provider.get_piece_data(0, 40, 20).await.is_some());
    assert!(provider.get_piece_data(0, 60, 10).await.is_none());
    assert!(provider.get_piece_data(99, 0, 10).await.is_none());
}

#[tokio::test]
async fn test_in_memory_provider_last_piece_smaller() {
    let total_size = 260u32;
    let piece_len = 100u32;
    let num_pieces = total_size.div_ceil(piece_len);
    let mut provider = InMemoryPieceProvider::new(piece_len, num_pieces);

    provider.set_all_from_pattern(|_, idx| idx as u8);
    provider.set_piece_data(2, (0..60).map(|idx| idx as u8).collect());

    assert!(provider.has_piece(0));
    assert!(provider.has_piece(1));
    assert!(provider.has_piece(2));

    let last_piece = provider.get_piece_data(2, 0, 60).await.unwrap();
    assert_eq!(last_piece.len(), 60);
}

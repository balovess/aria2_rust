use super::*;

#[test]
fn test_peer_session_resource_bitfield() {
    // 4 pieces of 256 KiB each = 1 MiB total
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert_eq!(res.num_pieces(), 4);
    assert_eq!(res.bitfield_length(), 1);

    // Initially no pieces
    for i in 0..4 {
        assert!(!res.has_piece(i), "piece {} should not be set", i);
    }

    // Set piece 0
    res.update_bitfield(0, 1);
    assert!(res.has_piece(0));
    assert!(!res.has_piece(1));

    // Set piece 3
    res.update_bitfield(3, 1);
    assert!(res.has_piece(3));

    // Clear piece 0
    res.update_bitfield(0, 0);
    assert!(!res.has_piece(0));

    // Set bitfield from raw bytes
    res.set_bitfield(&[0xC0]); // bits 0 and 1
    assert!(res.has_piece(0));
    assert!(res.has_piece(1));
    assert!(!res.has_piece(2));
    assert!(!res.has_piece(3));
}

#[test]
fn test_peer_session_resource_seeder() {
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert!(!res.is_seeder());

    res.mark_seeder();
    assert!(res.is_seeder());
    for i in 0..4 {
        assert!(res.has_piece(i), "seeder should have piece {}", i);
    }
}

#[test]
fn test_peer_session_resource_reconfigure() {
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert_eq!(res.num_pieces(), 4);

    res.reconfigure(512 * 1024, 8, 4 * 1024 * 1024);
    assert_eq!(res.num_pieces(), 8);
    assert_eq!(res.bitfield_length(), 1);
}

#[test]
fn test_peer_session_resource_out_of_range() {
    let res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert!(!res.has_piece(100)); // out of range
}

#[test]
fn test_peer_session_resource_update_bitfield_out_of_range() {
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    // Should not panic on out-of-range index
    res.update_bitfield(100, 1);
    assert!(!res.has_piece(100));
}

// -----------------------------------------------------------------------
// PeerSessionResource — Fast Extension tests
// -----------------------------------------------------------------------

#[test]
fn test_peer_session_resource_fast_extension() {
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert!(!res.is_fast_extension_enabled());

    res.set_fast_extension_enabled(true);
    assert!(res.is_fast_extension_enabled());
}

// -----------------------------------------------------------------------
// PeerSessionResource — Extension Protocol tests
// -----------------------------------------------------------------------

#[test]
fn test_peer_session_resource_extensions() {
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    // Register extensions
    res.add_extension("ut_pex", 1);
    res.add_extension("ut_metadata", 2);

    assert_eq!(res.get_extension_message_id("ut_pex"), Some(1));
    assert_eq!(res.get_extension_message_id("ut_metadata"), Some(2));
    assert_eq!(res.get_extension_message_id("unknown"), None);
}

// -----------------------------------------------------------------------
// BtPeerConn — session resource lifecycle
// -----------------------------------------------------------------------

#[test]
fn test_peer_session_resource_large_bitfield() {
    // 100 pieces of 1 MiB each = 100 MiB total
    let mut res = PeerSessionResource::new(1024 * 1024, 100, 100 * 1024 * 1024);
    assert_eq!(res.num_pieces(), 100);
    assert_eq!(res.bitfield_length(), 13); // ceil(100/8) = 13

    // Set piece 0 and 99
    res.update_bitfield(0, 1);
    res.update_bitfield(99, 1);
    assert!(res.has_piece(0));
    assert!(res.has_piece(99));
    assert!(!res.has_piece(50));

    // Mark seeder — all 100 bits should be set
    res.mark_seeder();
    assert!(res.is_seeder());
    for i in 0..100 {
        assert!(res.has_piece(i), "seeder should have piece {}", i);
    }
    // Piece 100 is out of range
    assert!(!res.has_piece(100));
}

#[test]
fn test_peer_session_resource_zero_length() {
    let res = PeerSessionResource::new(0, 0, 0);
    assert_eq!(res.num_pieces(), 0);
    // Vacuously a seeder
    assert!(res.is_seeder());
}

#[test]
fn test_peer_session_resource_uses_explicit_piece_count() {
    // v2 multi-file content can be two bytes while occupying two aligned
    // protocol pieces.
    let res = PeerSessionResource::new(16 * 1024, 2, 2);
    assert_eq!(res.num_pieces(), 2);
    assert_eq!(res.bitfield_length(), 1);
}

#[test]
fn test_peer_session_resource_accessors() {
    let res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert_eq!(res.piece_length(), 256 * 1024);
    assert_eq!(res.total_length(), 1024 * 1024);
}

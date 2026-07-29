use super::*;
use crate::engine::multi_file_layout::MultiFileLayout;

#[tokio::test]
async fn test_write_piece_to_multi_files_basic() {
    use aria2_protocol::bittorrent::torrent::parser::{FileEntry, InfoDict};

    let info = InfoDict {
        name: "write_test".to_string(),
        piece_length: 256,
        pieces: vec![[0u8; 20], [1u8; 20]],
        length: None,
        files: Some(vec![
            FileEntry {
                length: 200,
                path: vec!["sub".to_string(), "a.bin".to_string()],
            },
            FileEntry {
                length: 312,
                path: vec!["sub".to_string(), "b.bin".to_string()],
            },
        ]),
        private: None,
    };

    let base_dir = std::env::temp_dir().join(format!("mf_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(&base_dir).unwrap();
    let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();

    layout.create_directories().unwrap();

    let piece_data: Vec<u8> = (0..=255u8).collect();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        BtDownloadCommand::write_piece_to_multi_files(
            &layout,
            0,
            &piece_data,
            layout.piece_length(),
        ),
    )
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("write_piece_to_multi_files failed: {}", e),
        Err(_) => panic!("write_piece_to_multi_files timed out after 10s"),
    }

    let file_a = base_dir.join("sub").join("a.bin");
    let file_b = base_dir.join("sub").join("b.bin");
    assert!(file_a.exists(), "File a.bin should exist after write");
    assert!(file_b.exists(), "File b.bin should exist after write");

    let a_contents = std::fs::read(&file_a).unwrap();
    assert_eq!(a_contents.len(), 200, "a.bin should have 200 bytes");
    assert_eq!(
        &a_contents[..],
        &piece_data[..200],
        "a.bin contents should match first 200 bytes of piece"
    );

    let b_contents = std::fs::read(&file_b).unwrap();
    assert_eq!(
        b_contents.len(),
        56,
        "b.bin should have 56 bytes (remaining from piece 0)"
    );
    assert_eq!(
        &b_contents[..],
        &piece_data[200..256],
        "b.bin contents should match remaining bytes"
    );

    let _ = std::fs::remove_dir_all(&base_dir);
}

#[test]
fn test_write_piece_resolve_logic() {
    use aria2_protocol::bittorrent::torrent::parser::{FileEntry, InfoDict};

    let info = InfoDict {
        name: "resolve_test".to_string(),
        piece_length: 128,
        pieces: vec![[0u8; 20], [1u8; 20]],
        length: None,
        files: Some(vec![
            FileEntry {
                length: 100,
                path: vec!["x".to_string(), "f1.dat".to_string()],
            },
            FileEntry {
                length: 156,
                path: vec!["x".to_string(), "f2.dat".to_string()],
            },
        ]),
        private: None,
    };

    let base = std::path::PathBuf::from("d:/tmp/resolve_test");
    let layout = MultiFileLayout::from_info_dict(&info, &base).unwrap();
    assert_eq!(layout.num_files(), 2);
    assert_eq!(layout.total_size(), 256);
    assert!(layout.is_multi_file());

    let r0 = layout.resolve_file_offset(0, 0);
    assert_eq!(
        r0,
        Some((0, 0)),
        "Piece 0 offset 0 should map to file 0 offset 0"
    );

    let r1 = layout.resolve_file_offset(0, 99);
    assert_eq!(
        r1,
        Some((0, 99)),
        "Piece 0 offset 99 should map to file 0 offset 99"
    );

    let r2 = layout.resolve_file_offset(0, 100);
    assert_eq!(
        r2,
        Some((1, 0)),
        "Piece 0 offset 100 should map to file 1 offset 0 (cross-file boundary)"
    );

    let r3 = layout.resolve_file_offset(0, 127);
    assert_eq!(
        r3,
        Some((1, 27)),
        "Piece 0 offset 127 should map to file 1 offset 27"
    );

    let r4 = layout.resolve_file_offset(1, 0);
    assert_eq!(
        r4,
        Some((1, 28)),
        "Piece 1 offset 0 should map to file 1 offset 28"
    );

    let r5 = layout.resolve_file_offset(1, 127);
    assert_eq!(
        r5,
        Some((1, 155)),
        "Piece 1 offset 127 should map to file 1 offset 155"
    );

    let r_oob = layout.resolve_file_offset(1, 128);
    assert_eq!(r_oob, None, "Out-of-range offset should return None");
}

// ==================================================================
// Phase 14 / Task I4 — Coalesced multi-file write tests
// ==================================================================

#[tokio::test]
async fn test_coalesced_multi_file_write_basic() {
    use aria2_protocol::bittorrent::torrent::parser::{FileEntry, InfoDict};

    let info = InfoDict {
        name: "coalesce_test".to_string(),
        piece_length: 256,
        pieces: vec![[0u8; 20], [1u8; 20]],
        length: None,
        files: Some(vec![
            FileEntry {
                length: 200,
                path: vec!["sub".to_string(), "a.bin".to_string()],
            },
            FileEntry {
                length: 312,
                path: vec!["sub".to_string(), "b.bin".to_string()],
            },
        ]),
        private: None,
    };

    let base_dir = std::env::temp_dir().join(format!("coal_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(&base_dir).unwrap();
    let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();

    layout.create_directories().unwrap();

    let piece_data: Vec<u8> = (0..=255u8).collect();
    let piece_bytes = bytes::Bytes::from(piece_data.clone());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        BtDownloadCommand::write_piece_to_multi_files_coalesced(
            &layout,
            0,
            &piece_bytes,
            layout.piece_length(),
        ),
    )
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("write_piece_to_multi_files_coalesced failed: {}", e),
        Err(_) => panic!("coalesced write timed out after 10s"),
    }

    // Verify file contents match the original (non-coalesced) behaviour
    let file_a = base_dir.join("sub").join("a.bin");
    let file_b = base_dir.join("sub").join("b.bin");
    assert!(
        file_a.exists(),
        "File a.bin should exist after coalesced write"
    );
    assert!(
        file_b.exists(),
        "File b.bin should exist after coalesced write"
    );

    let a_contents = std::fs::read(&file_a).unwrap();
    assert_eq!(a_contents.len(), 200, "a.bin should have 200 bytes");
    assert_eq!(
        &a_contents[..],
        &piece_data[..200],
        "a.bin contents should match first 200 bytes of piece"
    );

    let b_contents = std::fs::read(&file_b).unwrap();
    assert_eq!(
        b_contents.len(),
        56,
        "b.bin should have 56 bytes (remaining from piece 0)"
    );
    assert_eq!(
        &b_contents[..],
        &piece_data[200..256],
        "b.bin contents should match remaining bytes"
    );

    let _ = std::fs::remove_dir_all(&base_dir);
}

#[tokio::test]
async fn test_coalesced_multi_file_write_three_files() {
    // Create a layout with 3 files so that piece 0 spans all three.
    // This exercises the coalesce logic when multiple raw writes land in
    // different files and some may be mergeable within COALESCE_GAP.
    use aria2_protocol::bittorrent::torrent::parser::{FileEntry, InfoDict};

    let info = InfoDict {
        name: "coal_3file".to_string(),
        piece_length: 512,
        pieces: vec![[0u8; 20]],
        length: None,
        files: Some(vec![
            FileEntry {
                length: 150,
                path: vec!["c3".to_string(), "f1.dat".to_string()],
            },
            FileEntry {
                length: 200,
                path: vec!["c3".to_string(), "f2.dat".to_string()],
            },
            FileEntry {
                length: 162,
                path: vec!["c3".to_string(), "f3.dat".to_string()],
            },
        ]),
        private: None,
    };

    let base_dir = std::env::temp_dir().join(format!("coal3_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(&base_dir).unwrap();
    let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();
    layout.create_directories().unwrap();

    // Piece data that spans all three files (512 bytes total)
    let piece_data: Vec<u8> = (0..512u64).map(|i| (i % 251) as u8).collect();
    let piece_bytes = bytes::Bytes::from(piece_data.clone());

    BtDownloadCommand::write_piece_to_multi_files_coalesced(
        &layout,
        0,
        &piece_bytes,
        layout.piece_length(),
    )
    .await
    .expect("coalesced write should succeed");

    let f1 = std::fs::read(layout.file_absolute_path(0).unwrap()).unwrap();
    let f2 = std::fs::read(layout.file_absolute_path(1).unwrap()).unwrap();
    let f3 = std::fs::read(layout.file_absolute_path(2).unwrap()).unwrap();

    assert_eq!(f1.len(), 150, "f1 should receive 150 bytes");
    assert_eq!(f2.len(), 200, "f2 should receive 200 bytes");
    assert_eq!(f3.len(), 162, "f3 should receive 162 bytes");

    assert_eq!(&f1[..], &piece_data[..150], "f1 content mismatch");
    assert_eq!(&f2[..], &piece_data[150..350], "f2 content mismatch");
    assert_eq!(&f3[..], &piece_data[350..512], "f3 content mismatch");

    let _ = std::fs::remove_dir_all(&base_dir);
}

/// Verify that the coalesced writer produces **identical on-disk results**
/// as the original non-coalesced writer for the same input.
#[tokio::test]
async fn test_coalesced_matches_original_writer() {
    use aria2_protocol::bittorrent::torrent::parser::{FileEntry, InfoDict};

    let info = InfoDict {
        name: "cmp_test".to_string(),
        piece_length: 384,
        pieces: vec![[0u8; 20], [1u8; 20]],
        length: None,
        files: Some(vec![
            FileEntry {
                length: 100,
                path: vec!["cmp".to_string(), "x1.bin".to_string()],
            },
            FileEntry {
                length: 284,
                path: vec!["cmp".to_string(), "x2.bin".to_string()],
            },
        ]),
        private: None,
    };

    let piece_data: Vec<u8> = (0..384u64).map(|i| ((i * 7 + 13) % 256) as u8).collect();
    let piece_bytes = bytes::Bytes::from(piece_data.clone());

    // --- Original writer ---
    let dir_orig = std::env::temp_dir().join(format!("cmp_orig_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir_orig);
    std::fs::create_dir_all(&dir_orig).unwrap();
    let layout_orig = MultiFileLayout::from_info_dict(&info, &dir_orig).unwrap();
    layout_orig.create_directories().unwrap();

    BtDownloadCommand::write_piece_to_multi_files(
        &layout_orig,
        0,
        &piece_data,
        layout_orig.piece_length(),
    )
    .await
    .expect("original writer should succeed");

    // --- Coalesced writer ---
    let dir_coal = std::env::temp_dir().join(format!("cmp_coal_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir_coal);
    std::fs::create_dir_all(&dir_coal).unwrap();
    let layout_coal = MultiFileLayout::from_info_dict(&info, &dir_coal).unwrap();
    layout_coal.create_directories().unwrap();

    BtDownloadCommand::write_piece_to_multi_files_coalesced(
        &layout_coal,
        0,
        &piece_bytes,
        layout_coal.piece_length(),
    )
    .await
    .expect("coalesced writer should succeed");

    // Compare byte-for-byte
    for i in 0..layout_orig.num_files() {
        let orig_bytes = std::fs::read(layout_orig.file_absolute_path(i).unwrap()).unwrap();
        let coal_bytes = std::fs::read(layout_coal.file_absolute_path(i).unwrap()).unwrap();
        assert_eq!(
            orig_bytes, coal_bytes,
            "File {} differs between original and coalesced writers",
            i
        );
    }

    let _ = std::fs::remove_dir_all(&dir_orig);
    let _ = std::fs::remove_dir_all(&dir_coal);
}

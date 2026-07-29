use super::*;
use crate::engine::bt_piece_downloader::FileBackedPieceProvider;
use crate::engine::bt_upload_session::PieceDataProvider;
use crate::engine::multi_file_layout::MultiFileLayout;

#[test]
fn test_multi_file_piece_provider_reads_correct_file() {
    use aria2_protocol::bittorrent::torrent::parser::{FileEntry, InfoDict};

    let info = InfoDict {
        name: "provider_test".to_string(),
        piece_length: 128,
        pieces: vec![[0u8; 20], [1u8; 20]],
        length: None,
        files: Some(vec![
            FileEntry {
                length: 100,
                path: vec!["p".to_string(), "a.dat".to_string()],
            },
            FileEntry {
                length: 156,
                path: vec!["p".to_string(), "b.dat".to_string()],
            },
        ]),
        private: None,
    };

    let base_dir = std::env::temp_dir().join(format!("mfp_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(base_dir.join("p")).unwrap();

    let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();
    layout.create_directories().unwrap();

    let file_a = layout.file_absolute_path(0).unwrap().to_path_buf();
    let file_b = layout.file_absolute_path(1).unwrap().to_path_buf();

    let data_a: Vec<u8> = (0..100u8).collect();
    let data_b: Vec<u8> = (100..=255u8).collect();
    std::fs::write(&file_a, &data_a).unwrap();
    std::fs::write(&file_b, &data_b).unwrap();

    let provider = FileBackedPieceProvider::new(base_dir.clone(), 128, 2, Some(layout));

    let result = provider.get_piece_data(0, 0, 10);
    assert!(result.is_some(), "Should read from file a at offset 0");
    assert_eq!(
        result.unwrap(),
        (0..10u8).collect::<Vec<u8>>(),
        "First 10 bytes should match file a"
    );

    let result_mid = provider.get_piece_data(0, 50, 50);
    assert!(result_mid.is_some());
    assert_eq!(
        result_mid.unwrap(),
        (50..100u8).collect::<Vec<u8>>(),
        "Bytes 50-99 from file a"
    );

    let result_cross = provider.get_piece_data(0, 95, 5);
    assert!(result_cross.is_some());
    assert_eq!(
        result_cross.unwrap(),
        (95..100u8).collect::<Vec<u8>>(),
        "Last 5 bytes of file a"
    );

    let result_b = provider.get_piece_data(1, 28, 50);
    assert!(result_b.is_some());
    assert_eq!(
        result_b.unwrap(),
        (156u8..=205u8).collect::<Vec<u8>>(),
        "Piece 1 offset 28 = global byte 156 = file b offset 56"
    );

    let _ = std::fs::remove_dir_all(&base_dir);
}

#[test]
fn test_single_file_piece_provider_unchanged() {
    let tmp = std::env::temp_dir().join(format!("sfp_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let file_path = tmp.join("single.bin");
    let data: Vec<u8> = (0..=255u8).collect();
    std::fs::write(&file_path, &data).unwrap();

    let provider = FileBackedPieceProvider::new(file_path.clone(), 128, 2, None);

    let result = provider.get_piece_data(0, 0, 16);
    assert!(
        result.is_some(),
        "Single-file provider should read successfully"
    );
    assert_eq!(
        result.unwrap(),
        (0..16u8).collect::<Vec<u8>>(),
        "First 16 bytes should match"
    );

    let result_mid = provider.get_piece_data(0, 64, 32);
    assert!(result_mid.is_some());
    assert_eq!(
        result_mid.unwrap(),
        (64..96u8).collect::<Vec<u8>>(),
        "Mid-piece read should match"
    );

    let result_p1 = provider.get_piece_data(1, 0, 32);
    assert!(result_p1.is_some());
    assert_eq!(
        result_p1.unwrap(),
        (128..160u8).collect::<Vec<u8>>(),
        "Piece 1 offset 0 = byte 128"
    );

    let result_end = provider.get_piece_data(1, 127, 1);
    assert!(result_end.is_some());
    assert_eq!(result_end.unwrap(), vec![255u8], "Last byte should be 255");

    assert_eq!(provider.num_pieces(), 2);
    assert!(provider.has_piece(0));
    assert!(provider.has_piece(1));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn test_multi_file_cross_boundary_read() {
    use aria2_protocol::bittorrent::torrent::parser::{FileEntry, InfoDict};

    let info = InfoDict {
        name: "cross_boundary_test".to_string(),
        piece_length: 256,
        pieces: vec![[0u8; 20], [1u8; 20], [2u8; 20]],
        length: None,
        files: Some(vec![
            FileEntry {
                length: 150,
                path: vec!["cb".to_string(), "f1.bin".to_string()],
            },
            FileEntry {
                length: 150,
                path: vec!["cb".to_string(), "f2.bin".to_string()],
            },
            FileEntry {
                length: 100,
                path: vec!["cb".to_string(), "f3.bin".to_string()],
            },
        ]),
        private: None,
    };

    let base_dir = std::env::temp_dir().join(format!("cb_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(base_dir.join("cb")).unwrap();

    let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();
    layout.create_directories().unwrap();

    let file1 = layout.file_absolute_path(0).unwrap().to_path_buf();
    let file2 = layout.file_absolute_path(1).unwrap().to_path_buf();
    let file3 = layout.file_absolute_path(2).unwrap().to_path_buf();

    let data1: Vec<u8> = (0..150u8).collect();
    let data2: Vec<u8> = (150..300).map(|i: u64| i as u8).collect();
    let data3: Vec<u8> = (300..400).map(|i: u64| i as u8).collect();

    std::fs::write(&file1, &data1).unwrap();
    std::fs::write(&file2, &data2).unwrap();
    std::fs::write(&file3, &data3).unwrap();

    let provider = FileBackedPieceProvider::new(base_dir.clone(), 256, 3, Some(layout));

    let result = provider.get_piece_data(0, 140, 10);
    assert!(result.is_some(), "Read within file1 should succeed");
    let data = result.unwrap();
    assert_eq!(data.len(), 10, "Should read exactly 10 bytes");
    assert_eq!(
        data,
        (140..150u8).collect::<Vec<u8>>(),
        "Bytes 140-149 from file1"
    );

    let result_p1 = provider.get_piece_data(1, 0, 100);
    assert!(result_p1.is_some(), "Read from piece 1 should work");
    let data_p1 = result_p1.unwrap();
    assert_eq!(data_p1.len(), 100);

    let _ = std::fs::remove_dir_all(&base_dir);
}

#[test]
fn test_large_offset_and_edge_cases() {
    use aria2_protocol::bittorrent::torrent::parser::{FileEntry, InfoDict};

    let info = InfoDict {
        name: "edge_case_test".to_string(),
        piece_length: 1024,
        pieces: vec![[0u8; 20]],
        length: None,
        files: Some(vec![
            FileEntry {
                length: 800,
                path: vec!["ec".to_string(), "big.dat".to_string()],
            },
            FileEntry {
                length: 224,
                path: vec!["ec".to_string(), "small.dat".to_string()],
            },
        ]),
        private: None,
    };

    let base_dir = std::env::temp_dir().join(format!("ec_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(base_dir.join("ec")).unwrap();

    let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();
    layout.create_directories().unwrap();

    let big_file = layout.file_absolute_path(0).unwrap().to_path_buf();
    let small_file = layout.file_absolute_path(1).unwrap().to_path_buf();

    let big_data: Vec<u8> = (0..800).map(|i: u64| (i % 256) as u8).collect();
    let small_data: Vec<u8> = (800..1024).map(|i: u64| (i % 256) as u8).collect();

    std::fs::write(&big_file, &big_data).unwrap();
    std::fs::write(&small_file, &small_data).unwrap();

    let provider = FileBackedPieceProvider::new(base_dir.clone(), 1024, 1, Some(layout));

    let result_start = provider.get_piece_data(0, 0, 1);
    assert!(result_start.is_some());
    assert_eq!(result_start.unwrap(), vec![0u8], "First byte should be 0");

    let result_near_end = provider.get_piece_data(0, 1023, 1);
    assert!(result_near_end.is_some());
    assert_eq!(
        result_near_end.unwrap(),
        vec![255u8],
        "Last byte should be 255"
    );

    let result_zero_len = provider.get_piece_data(0, 500, 0);
    assert!(
        result_zero_len.is_some(),
        "Zero-length read should return empty"
    );
    assert_eq!(
        result_zero_len.unwrap().len(),
        0,
        "Zero-length read should return empty vec"
    );

    let result_full_piece = provider.get_piece_data(0, 0, 512);
    assert!(result_full_piece.is_some());
    assert_eq!(
        result_full_piece.unwrap().len(),
        512,
        "Half piece read should return 512 bytes"
    );

    let _ = std::fs::remove_dir_all(&base_dir);
}

#[test]
fn test_provider_error_handling() {
    use aria2_protocol::bittorrent::torrent::parser::{FileEntry, InfoDict};

    let info = InfoDict {
        name: "error_test".to_string(),
        piece_length: 128,
        pieces: vec![[0u8; 20]],
        length: None,
        files: Some(vec![
            FileEntry {
                length: 100,
                path: vec!["err".to_string(), "exists.dat".to_string()],
            },
            FileEntry {
                length: 50,
                path: vec!["err".to_string(), "missing.dat".to_string()],
            },
        ]),
        private: None,
    };

    let base_dir = std::env::temp_dir().join(format!("err_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base_dir);
    std::fs::create_dir_all(base_dir.join("err")).unwrap();

    let layout = MultiFileLayout::from_info_dict(&info, &base_dir).unwrap();
    layout.create_directories().unwrap();

    let exists_file = layout.file_absolute_path(0).unwrap().to_path_buf();
    let data: Vec<u8> = (0..100u8).collect();
    std::fs::write(&exists_file, &data).unwrap();

    let provider = FileBackedPieceProvider::new(base_dir.clone(), 128, 1, Some(layout));

    let result_valid = provider.get_piece_data(0, 0, 50);
    assert!(
        result_valid.is_some(),
        "Read from existing file should succeed"
    );

    let result_oob_piece = provider.get_piece_data(5, 0, 10);
    assert!(
        result_oob_piece.is_none(),
        "Out-of-bounds piece index should return None"
    );

    let result_oob_offset = provider.get_piece_data(0, 200, 10);
    assert!(
        result_oob_offset.is_none() || result_oob_offset.as_ref().is_none_or(|d| d.is_empty()),
        "Out-of-bounds offset should return None or empty"
    );

    assert_eq!(provider.num_pieces(), 1);
    assert!(provider.has_piece(0));

    let result_oob_piece2 = provider.get_piece_data(99, 0, 10);
    assert!(
        result_oob_piece2.is_none(),
        "Out-of-bounds piece index should return None"
    );

    let _ = std::fs::remove_dir_all(&base_dir);
}

#[test]
fn test_single_file_provider_with_varying_piece_sizes() {
    let tmp = std::env::temp_dir().join(format!("vary_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let file_path = tmp.join("variable.bin");
    let data: Vec<u8> = (0..=999u64).map(|i| (i % 256) as u8).collect();
    std::fs::write(&file_path, &data).unwrap();

    let provider_small = FileBackedPieceProvider::new(file_path.clone(), 256, 4, None);
    assert_eq!(provider_small.num_pieces(), 4);

    let r1 = provider_small.get_piece_data(0, 0, 256);
    assert!(r1.is_some());
    assert_eq!(r1.unwrap().len(), 256);

    let r_last = provider_small.get_piece_data(3, 0, 16);
    assert!(r_last.is_some());
    assert_eq!(r_last.unwrap().len(), 16);

    let provider_large = FileBackedPieceProvider::new(file_path.clone(), 2048, 1, None);
    assert_eq!(provider_large.num_pieces(), 1);

    let r_overflow = provider_large.get_piece_data(0, 900, 200);
    assert!(
        r_overflow.is_none(),
        "Read beyond file size should return None"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

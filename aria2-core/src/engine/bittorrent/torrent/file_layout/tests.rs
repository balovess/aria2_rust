use super::*;
use aria2_protocol::bittorrent::torrent::parser::FileEntry;

fn make_single_file_info_dict() -> InfoDict {
    InfoDict {
        name: "single_file.bin".to_string(),
        piece_length: 512,
        pieces: vec![[0u8; 20], [1u8; 20]],
        length: Some(1024),
        files: None,
        private: None,
        meta_version: None,
        v2_files: None,
        pieces_root: None,
    }
}

fn make_multi_file_info_dict() -> InfoDict {
    InfoDict {
        name: "multi_dir".to_string(),
        piece_length: 512,
        pieces: vec![[0u8; 20], [1u8; 20], [2u8; 20]],
        length: None,
        files: Some(vec![
            FileEntry {
                length: 500,
                path: vec!["dir1".to_string(), "file1.txt".to_string()],
            },
            FileEntry {
                length: 524,
                path: vec!["dir2".to_string(), "file2.dat".to_string()],
            },
            FileEntry {
                length: 300,
                path: vec!["dir3".to_string(), "file3.log".to_string()],
            },
        ]),
        private: None,
        meta_version: None,
        v2_files: None,
        pieces_root: None,
    }
}

#[test]
fn test_from_info_dict_single_file() {
    let info = make_single_file_info_dict();
    let base = Path::new("/tmp/download");
    let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

    assert_eq!(layout.num_files(), 1);
    assert!(!layout.is_multi_file());
    assert_eq!(layout.total_size(), 1024);
    assert_eq!(layout.piece_length(), 512);
    assert_eq!(layout.total_pieces(), 2);

    let file = layout.get_file_info(0).unwrap();
    assert_eq!(file.path, vec!["single_file.bin"]);
    assert_eq!(file.length, 1024);
    assert_eq!(file.start_piece, 0);
    assert_eq!(file.end_piece, 1);
    assert_eq!(file.start_offset_in_piece, 0);
    assert_eq!(file.end_offset_in_piece, 512);
}

#[test]
fn test_from_info_dict_multi_file() {
    let info = make_multi_file_info_dict();
    let base = Path::new("/tmp/download");
    let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

    assert_eq!(layout.num_files(), 3);
    assert!(layout.is_multi_file());
    assert_eq!(layout.total_size(), 1324);

    let f0 = layout.get_file_info(0).unwrap();
    assert_eq!(f0.length, 500);
    assert_eq!(f0.start_piece, 0);
    assert_eq!(f0.end_piece, 0);
    assert_eq!(f0.start_offset_in_piece, 0);
    assert_eq!(f0.end_offset_in_piece, 500);

    let f1 = layout.get_file_info(1).unwrap();
    assert_eq!(f1.length, 524);
    assert_eq!(f1.start_piece, 0);
    assert_eq!(f1.end_piece, 1);
    assert_eq!(f1.start_offset_in_piece, 500);
    assert_eq!(f1.end_offset_in_piece, 512);

    let f2 = layout.get_file_info(2).unwrap();
    assert_eq!(f2.length, 300);
    assert_eq!(f2.start_piece, 2);
    assert_eq!(f2.end_piece, 2);
    assert_eq!(f2.start_offset_in_piece, 0);
    assert_eq!(f2.end_offset_in_piece, 300);
}

#[test]
fn test_from_info_dict_empty_files() {
    let info = InfoDict {
        name: "empty".to_string(),
        piece_length: 512,
        pieces: vec![],
        length: None,
        files: Some(vec![]),
        private: None,
        meta_version: None,
        v2_files: None,
        pieces_root: None,
    };
    let base = Path::new("/tmp/download");
    let result = MultiFileLayout::from_info_dict(&info, base);
    assert!(result.is_err());
}

#[test]
fn v2_files_are_piece_aligned_and_count_content_only() {
    let info = InfoDict {
        name: "root".to_string(),
        piece_length: 16 * 1024,
        pieces: Vec::new(),
        length: None,
        files: None,
        private: None,
        meta_version: Some(2),
        v2_files: Some(vec![
            aria2_protocol::bittorrent::torrent::parser::V2FileEntry {
                length: 1,
                path: vec!["a".to_string()],
                pieces_root: Some([1; 32]),
            },
            aria2_protocol::bittorrent::torrent::parser::V2FileEntry {
                length: 16 * 1024,
                path: vec!["b".to_string()],
                pieces_root: Some([2; 32]),
            },
        ]),
        pieces_root: None,
    };
    let layout = MultiFileLayout::from_info_dict(&info, Path::new("/tmp")).unwrap();
    assert_eq!(layout.total_size(), 16 * 1024 + 1);
    assert_eq!(layout.piece_space_size(), 2 * 16 * 1024);
    assert_eq!(layout.total_pieces(), 2);
    assert_eq!(layout.content_bytes_in_piece(0), 1);
    assert_eq!(layout.content_bytes_in_piece(1), 16 * 1024);
}

#[test]
fn test_create_directories() {
    let info = make_multi_file_info_dict();
    let temp_dir = tempfile::tempdir().unwrap();
    let base = temp_dir.path();
    let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

    let result = layout.create_directories();
    assert!(result.is_ok());

    let dir1 = base.join("dir1");
    let dir2 = base.join("dir2");
    let dir3 = base.join("dir3");

    assert!(dir1.exists());
    assert!(dir2.exists());
    assert!(dir3.exists());

    // temp_dir is automatically cleaned up when dropped
}

#[test]
fn test_resolve_file_offset_single_file() {
    let info = make_single_file_info_dict();
    let base = Path::new("/tmp/download");
    let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

    let result = layout.resolve_file_offset(0, 0);
    assert_eq!(result, Some((0, 0)));

    let result = layout.resolve_file_offset(0, 256);
    assert_eq!(result, Some((0, 256)));

    let result = layout.resolve_file_offset(1, 0);
    assert_eq!(result, Some((0, 512)));

    let result = layout.resolve_file_offset(1, 511);
    assert_eq!(result, Some((0, 1023)));
}

#[test]
fn test_resolve_file_offset_multi_file() {
    let info = make_multi_file_info_dict();
    let base = Path::new("/tmp/download");
    let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

    let result = layout.resolve_file_offset(0, 0);
    assert_eq!(result, Some((0, 0)));

    let result = layout.resolve_file_offset(0, 499);
    assert_eq!(result, Some((0, 499)));

    let result = layout.resolve_file_offset(0, 500);
    assert_eq!(result, Some((1, 0)));

    let result = layout.resolve_file_offset(1, 0);
    assert_eq!(result, Some((1, 12)));

    let result = layout.resolve_file_offset(1, 200);
    assert_eq!(result, Some((1, 212)));

    let result = layout.resolve_file_offset(2, 50);
    assert_eq!(result, Some((2, 50)));
}

#[test]
fn test_resolve_file_offset_boundary() {
    let info = make_multi_file_info_dict();
    let base = Path::new("/tmp/download");
    let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

    let result = layout.resolve_file_offset(0, 499);
    assert_eq!(result, Some((0, 499)));

    let result = layout.resolve_file_offset(0, 500);
    assert_eq!(result, Some((1, 0)));

    let result = layout.resolve_file_offset(1, 511);
    assert_eq!(result, Some((1, 523)));

    let result = layout.resolve_file_offset(2, 0);
    assert_eq!(result, Some((2, 0)));
}

#[test]
fn test_resolve_file_offset_out_of_range() {
    let info = make_single_file_info_dict();
    let base = Path::new("/tmp/download");
    let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

    let result = layout.resolve_file_offset(1, 512);
    assert_eq!(result, None);

    let result = layout.resolve_file_offset(2, 0);
    assert_eq!(result, None);

    let result = layout.resolve_file_offset(u32::MAX, 0);
    assert_eq!(result, None);
}

#[test]
fn test_file_completed_bytes() {
    let info = make_single_file_info_dict();
    let base = Path::new("/tmp/download");
    let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

    let no_pieces = [0u8; 1];
    assert_eq!(layout.file_completed_bytes(0, &no_pieces), 0);

    let piece0_only = [0b10000000u8];
    assert_eq!(layout.file_completed_bytes(0, &piece0_only), 512);

    let both_pieces = [0b11000000u8];
    assert_eq!(layout.file_completed_bytes(0, &both_pieces), 1024);
}

#[test]
fn test_file_list_returns_correct_entries() {
    let info = make_multi_file_info_dict();
    let base = Path::new("/tmp/download");
    let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

    let list = layout.file_list();
    assert_eq!(list.len(), 3);

    assert_eq!(list[0].index, 0);
    assert_eq!(list[0].path, "dir1/file1.txt");
    assert_eq!(list[0].length, 500);
    assert_eq!(list[0].completed_length, 0);

    assert_eq!(list[1].index, 1);
    assert_eq!(list[1].path, "dir2/file2.dat");
    assert_eq!(list[1].length, 524);

    assert_eq!(list[2].index, 2);
    assert_eq!(list[2].path, "dir3/file3.log");
    assert_eq!(list[2].length, 300);
}

#[test]
fn test_is_multi_file_flags() {
    let single = make_single_file_info_dict();
    let single_layout = MultiFileLayout::from_info_dict(&single, Path::new("/tmp")).unwrap();
    assert!(!single_layout.is_multi_file());

    let multi = make_multi_file_info_dict();
    let multi_layout = MultiFileLayout::from_info_dict(&multi, Path::new("/tmp")).unwrap();
    assert!(multi_layout.is_multi_file());
}

#[test]
fn test_total_size_matches() {
    let single = make_single_file_info_dict();
    let single_layout = MultiFileLayout::from_info_dict(&single, Path::new("/tmp")).unwrap();
    assert_eq!(single_layout.total_size(), 1024);

    let multi = make_multi_file_info_dict();
    let multi_layout = MultiFileLayout::from_info_dict(&multi, Path::new("/tmp")).unwrap();
    assert_eq!(multi_layout.total_size(), 1324);
    assert_eq!(multi_layout.total_size(), 500 + 524 + 300);
}

#[test]
fn test_file_absolute_path() {
    let info = make_multi_file_info_dict();
    let base = Path::new("/base/dir");
    let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

    let p0 = layout.file_absolute_path(0).unwrap();
    assert_eq!(p0, Path::new("/base/dir/dir1/file1.txt"));

    let p1 = layout.file_absolute_path(1).unwrap();
    assert_eq!(p1, Path::new("/base/dir/dir2/file2.dat"));

    let p2 = layout.file_absolute_path(2).unwrap();
    assert_eq!(p2, Path::new("/base/dir/dir3/file3.log"));

    assert!(layout.file_absolute_path(3).is_none());
}

#[test]
fn test_file_completed_bytes_multi_file_partial() {
    let info = make_multi_file_info_dict();
    let base = Path::new("/tmp/download");
    let layout = MultiFileLayout::from_info_dict(&info, base).unwrap();

    let only_piece0 = [0b10000000u8];
    assert_eq!(layout.file_completed_bytes(0, &only_piece0), 500);
    assert_eq!(layout.file_completed_bytes(1, &only_piece0), 12);
    assert_eq!(layout.file_completed_bytes(2, &only_piece0), 0);

    let piece0_and_1 = [0b11000000u8];
    assert_eq!(layout.file_completed_bytes(0, &piece0_and_1), 500);
    assert_eq!(layout.file_completed_bytes(1, &piece0_and_1), 524);
    assert_eq!(layout.file_completed_bytes(2, &piece0_and_1), 0);

    let all_pieces = [0b11100000u8];
    assert_eq!(layout.file_completed_bytes(0, &all_pieces), 500);
    assert_eq!(layout.file_completed_bytes(1, &all_pieces), 524);
    assert_eq!(layout.file_completed_bytes(2, &all_pieces), 300);
}

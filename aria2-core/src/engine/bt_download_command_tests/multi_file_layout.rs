use super::build_test_torrent;
use super::*;
use crate::request::request_group::{DownloadOptions, GroupId};

fn build_multi_file_torrent() -> Vec<u8> {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use std::collections::BTreeMap;

    let file1_path = BencodeValue::List(vec![
        BencodeValue::Bytes(b"dir1".to_vec()),
        BencodeValue::Bytes(b"file1.txt".to_vec()),
    ]);
    let mut file1_dict = BTreeMap::new();
    file1_dict.insert(b"length".to_vec(), BencodeValue::Int(500));
    file1_dict.insert(b"path".to_vec(), file1_path);

    let file2_path = BencodeValue::List(vec![
        BencodeValue::Bytes(b"dir2".to_vec()),
        BencodeValue::Bytes(b"file2.dat".to_vec()),
    ]);
    let mut file2_dict = BTreeMap::new();
    file2_dict.insert(b"length".to_vec(), BencodeValue::Int(524));
    file2_dict.insert(b"path".to_vec(), file2_path);

    let files_list = BencodeValue::List(vec![
        BencodeValue::Dict(file1_dict),
        BencodeValue::Dict(file2_dict),
    ]);

    let mut info_dict = BTreeMap::new();
    info_dict.insert(b"name".to_vec(), BencodeValue::Bytes(b"multitest".to_vec()));
    info_dict.insert(b"files".to_vec(), files_list);
    info_dict.insert(b"piece length".to_vec(), BencodeValue::Int(512));

    let mut pieces_hash = Vec::new();
    pieces_hash.extend_from_slice(&[0u8; 20]);
    pieces_hash.extend_from_slice(&[1u8; 20]);
    info_dict.insert(b"pieces".to_vec(), BencodeValue::Bytes(pieces_hash));

    let mut root_dict = BTreeMap::new();
    root_dict.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
    );
    root_dict.insert(b"info".to_vec(), BencodeValue::Dict(info_dict));

    BencodeValue::Dict(root_dict).encode()
}

#[test]
fn test_multi_file_layout_created_for_multi_torrent() {
    let torrent_bytes = build_multi_file_torrent();
    let options = DownloadOptions::default();
    let gid = GroupId::new(100);
    let cmd = BtDownloadCommand::new(gid, &torrent_bytes, &options, Some("d:/tmp/multitest"))
        .expect("Failed to create command from multi-file torrent");

    assert!(
        cmd.multi_file_layout.is_some(),
        "multi_file_layout should be Some for multi-file torrent"
    );
    let layout = cmd.multi_file_layout.as_ref().unwrap();
    assert!(layout.is_multi_file());
    assert_eq!(layout.num_files(), 2);
    assert_eq!(layout.total_size(), 1024);
}

#[test]
fn test_index_out_updates_context_and_writer_layout() {
    let torrent_bytes = build_multi_file_torrent();
    let options = DownloadOptions {
        dir: Some("d:/tmp/multitest".to_string()),
        index_out: Some("1=first.iso\n2=nested/second.dat".to_string()),
        ..DownloadOptions::default()
    };

    let cmd = BtDownloadCommand::new(GroupId::new(102), &torrent_bytes, &options, None)
        .expect("index-out should be applied while constructing a BT command");

    let context = cmd
        .group
        .read()
        .expect("request group lock")
        .get_download_context()
        .expect("BT context");
    let base_dir = std::path::Path::new("d:/tmp/multitest");
    assert_eq!(
        context.get_file_entries()[0].path(),
        base_dir.join("first.iso").to_string_lossy()
    );
    assert_eq!(
        context.get_file_entries()[1].path(),
        base_dir.join("nested/second.dat").to_string_lossy()
    );

    let layout = cmd.multi_file_layout.as_ref().expect("multi-file layout");
    assert_eq!(
        layout.file_absolute_path(0).unwrap(),
        &base_dir.join("first.iso")
    );
    assert_eq!(
        layout.file_absolute_path(1).unwrap(),
        &base_dir.join("nested/second.dat")
    );
}

#[test]
fn test_index_out_updates_single_file_output_path() {
    let temp_dir = tempfile::tempdir().expect("temporary download directory");
    let options = DownloadOptions {
        dir: Some(temp_dir.path().to_string_lossy().into_owned()),
        index_out: Some("1=renamed.bin".to_string()),
        ..DownloadOptions::default()
    };

    let cmd = BtDownloadCommand::new(GroupId::new(103), &build_test_torrent(), &options, None)
        .expect("index-out should override a single-file BT output");

    assert_eq!(cmd.output_path, temp_dir.path().join("renamed.bin"));
    let context = cmd
        .group
        .read()
        .expect("request group lock")
        .get_download_context()
        .expect("BT context");
    assert_eq!(
        context.get_file_entries()[0].path(),
        cmd.output_path.to_string_lossy()
    );
}

#[test]
fn test_single_file_no_layout() {
    let cmd = create_test_command();

    assert!(
        cmd.multi_file_layout.is_none(),
        "multi_file_layout should be None for single-file torrent"
    );
}

#[test]
fn test_is_multi_file_accessor() {
    let single_cmd = create_test_command();
    assert!(
        !single_cmd.is_multi_file(),
        "Single-file torrent should return false"
    );

    let multi_bytes = build_multi_file_torrent();
    let options = DownloadOptions::default();
    let gid = GroupId::new(101);
    let multi_cmd = BtDownloadCommand::new(gid, &multi_bytes, &options, Some("d:/tmp/test_acc"))
        .expect("Failed to create multi-file command");
    assert!(
        multi_cmd.is_multi_file(),
        "Multi-file torrent should return true"
    );

    assert!(multi_cmd.get_multi_file_layout().is_some());
    assert!(create_test_command().get_multi_file_layout().is_none());
}

use super::*;

#[test]
fn test_parse_single_file_torrent() {
    let data = make_simple_torrent();
    let torrent = TorrentMeta::parse(&data).unwrap();

    assert_eq!(torrent.announce, "http://tracker.example.com/announce");
    assert_eq!(torrent.info.name, "test_file.bin");
    assert_eq!(torrent.info.piece_length, 512);
    assert_eq!(torrent.info.pieces.len(), 2);
    assert_eq!(torrent.info.length, Some(1024));
    assert!(torrent.is_single_file());
    assert!(!torrent.is_private());
    assert_eq!(torrent.num_pieces(), 2);
    assert_eq!(torrent.total_size(), 1024);
    assert_eq!(torrent.info.meta_version, None);
    assert!(torrent.info_hash_v2.is_none());
}

#[test]
fn test_rejects_unsupported_metainfo_version() {
    let mut info = BTreeMap::new();
    info.insert(b"meta version".to_vec(), BencodeValue::Int(3));
    info.insert(b"piece length".to_vec(), BencodeValue::Int(16 * 1024));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 20]));
    let root = BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://x".to_vec()),
        ),
        (b"info".to_vec(), BencodeValue::Dict(info)),
    ]);
    let error = TorrentMeta::parse(&BencodeValue::Dict(root).encode()).unwrap_err();
    assert!(error.contains("unsupported BitTorrent metainfo version 3"));
}

#[test]
fn test_parse_multi_file_torrent() {
    let pieces_data = vec![0u8; 40];
    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), BencodeValue::Bytes(b"multi_dir".to_vec()));

    let mut f1 = BTreeMap::new();
    f1.insert(b"length".to_vec(), BencodeValue::Int(500));
    f1.insert(
        b"path".to_vec(),
        BencodeValue::List(vec![
            BencodeValue::Bytes(b"dir1".to_vec()),
            BencodeValue::Bytes(b"file1.txt".to_vec()),
        ]),
    );

    let mut f2 = BTreeMap::new();
    f2.insert(b"length".to_vec(), BencodeValue::Int(524));
    f2.insert(
        b"path".to_vec(),
        BencodeValue::List(vec![
            BencodeValue::Bytes(b"dir2".to_vec()),
            BencodeValue::Bytes(b"file2.dat".to_vec()),
        ]),
    );

    info.insert(
        b"files".to_vec(),
        BencodeValue::List(vec![BencodeValue::Dict(f1), BencodeValue::Dict(f2)]),
    );
    info.insert(b"piece length".to_vec(), BencodeValue::Int(512));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(pieces_data));

    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));

    let data = BencodeValue::Dict(root).encode();
    let torrent = TorrentMeta::parse(&data).unwrap();

    assert!(!torrent.is_single_file());
    assert_eq!(torrent.info.files.as_ref().unwrap().len(), 2);
    assert_eq!(torrent.total_size(), 1024);
}

#[test]
fn test_error_missing_fields() {
    let empty = BencodeValue::Dict(BTreeMap::new()).encode();
    assert!(TorrentMeta::parse(&empty).is_err());

    let mut r = BTreeMap::new();
    r.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://x".to_vec()),
    );
    let no_info = BencodeValue::Dict(r).encode();
    assert!(TorrentMeta::parse(&no_info).is_err());
}

#[test]
fn torrent_parser_rejects_unsafe_output_paths() {
    for name in [
        "../outside",
        "..\\outside",
        "/outside",
        "C:outside",
        "bad\0name",
    ] {
        let torrent = make_torrent_with_name_and_path(name, None);
        assert!(
            TorrentMeta::parse(&torrent).is_err(),
            "unsafe torrent root name was accepted: {name:?}"
        );
    }

    let unsafe_paths: &[&[&str]] = &[
        &[".."],
        &["..", "outside"],
        &["../outside"],
        &["/outside"],
        &["C:outside"],
        &["bad\0name"],
    ];
    for path in unsafe_paths {
        let torrent = make_torrent_with_name_and_path("safe-root", Some(*path));
        assert!(
            TorrentMeta::parse(&torrent).is_err(),
            "unsafe torrent file path was accepted: {path:?}"
        );
    }
}

#[test]
fn torrent_parser_rejects_non_string_v1_path_components() {
    let file = BencodeValue::Dict(BTreeMap::from([
        (b"length".to_vec(), BencodeValue::Int(1)),
        (
            b"path".to_vec(),
            BencodeValue::List(vec![
                BencodeValue::Bytes(b"folder".to_vec()),
                BencodeValue::Int(7),
                BencodeValue::Bytes(b"file.bin".to_vec()),
            ]),
        ),
    ]));
    let info = BencodeValue::Dict(BTreeMap::from([
        (b"name".to_vec(), BencodeValue::Bytes(b"safe-root".to_vec())),
        (b"files".to_vec(), BencodeValue::List(vec![file])),
        (b"piece length".to_vec(), BencodeValue::Int(16_384)),
        (b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20])),
    ]));
    let torrent = BencodeValue::Dict(BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        ),
        (b"info".to_vec(), info),
    ]))
    .encode();

    assert!(
        TorrentMeta::parse(&torrent).is_err(),
        "non-string path component must not be silently discarded"
    );
}

#[test]
fn torrent_parser_percent_encodes_non_utf8_path_components() {
    let file = BencodeValue::Dict(BTreeMap::from([
        (b"length".to_vec(), BencodeValue::Int(1)),
        (
            b"path".to_vec(),
            BencodeValue::List(vec![
                BencodeValue::Bytes(vec![0xfe]),
                BencodeValue::Bytes(b"file.bin".to_vec()),
            ]),
        ),
    ]));
    let info = BencodeValue::Dict(BTreeMap::from([
        (b"name".to_vec(), BencodeValue::Bytes(vec![0xff, b'r'])),
        (b"files".to_vec(), BencodeValue::List(vec![file])),
        (b"piece length".to_vec(), BencodeValue::Int(16_384)),
        (b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20])),
    ]));
    let torrent = BencodeValue::Dict(BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        ),
        (b"info".to_vec(), info),
    ]))
    .encode();

    let metadata = TorrentMeta::parse(&torrent).expect("non-UTF-8 path bytes are representable");
    assert_eq!(metadata.info.name, "%FFr");
    assert_eq!(metadata.info.files.unwrap()[0].path, ["%FE", "file.bin"]);
}

#[test]
fn test_rejects_negative_single_file_length() {
    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), BencodeValue::Bytes(b"file".to_vec()));
    info.insert(b"length".to_vec(), BencodeValue::Int(-1));
    info.insert(b"piece length".to_vec(), BencodeValue::Int(16_384));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(Vec::new()));
    let root = BencodeValue::Dict(BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://x".to_vec()),
        ),
        (b"info".to_vec(), BencodeValue::Dict(info)),
    ]));

    let error = TorrentMeta::parse(&root.encode()).unwrap_err();
    assert!(error.contains("file length must be non-negative"));
}

#[test]
fn test_rejects_negative_multi_file_length() {
    let file = BencodeValue::Dict(BTreeMap::from([
        (b"length".to_vec(), BencodeValue::Int(-1)),
        (
            b"path".to_vec(),
            BencodeValue::List(vec![BencodeValue::Bytes(b"file".to_vec())]),
        ),
    ]));
    let info = BencodeValue::Dict(BTreeMap::from([
        (b"name".to_vec(), BencodeValue::Bytes(b"root".to_vec())),
        (b"files".to_vec(), BencodeValue::List(vec![file])),
        (b"piece length".to_vec(), BencodeValue::Int(16_384)),
        (b"pieces".to_vec(), BencodeValue::Bytes(Vec::new())),
    ]));
    let root = BencodeValue::Dict(BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://x".to_vec()),
        ),
        (b"info".to_vec(), info),
    ]));

    let error = TorrentMeta::parse(&root.encode()).unwrap_err();
    assert!(error.contains("file length must be non-negative"));
}

#[test]
fn test_info_hash_consistency() {
    let data = make_simple_torrent();
    let t1 = TorrentMeta::parse(&data).unwrap();
    let t2 = TorrentMeta::parse(&data).unwrap();
    assert_eq!(t1.info_hash.as_hex(), t2.info_hash.as_hex());

    let (parsed, info_bytes) = TorrentMeta::parse_with_info_bytes(&data).unwrap();
    assert_eq!(parsed.info_hash, t1.info_hash);
    assert_eq!(
        parsed.info_hash.bytes,
        crate::bittorrent::torrent::info_hash::InfoHash::from_info_bytes(&info_bytes).bytes
    );
    let (root, _) = BencodeValue::decode(&data).unwrap();
    assert_eq!(info_bytes, root.dict_get(b"info").unwrap().encode());
}

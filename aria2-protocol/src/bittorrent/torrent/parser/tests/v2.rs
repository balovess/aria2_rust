use super::*;

#[test]
fn test_parse_v2_file_tree_and_piece_layers() {
    let piece_hash = [0x22u8; 32];
    let root_hash = crate::bittorrent::torrent::merkle::parent_hash(&piece_hash, &piece_hash);
    let mut leaf = BTreeMap::new();
    leaf.insert(b"length".to_vec(), BencodeValue::Int(32768));
    leaf.insert(
        b"pieces root".to_vec(),
        BencodeValue::Bytes(root_hash.to_vec()),
    );
    let mut file_name = BTreeMap::new();
    file_name.insert(b"".to_vec(), BencodeValue::Dict(leaf));
    let mut file_tree = BTreeMap::new();
    file_tree.insert(b"payload.bin".to_vec(), BencodeValue::Dict(file_name));

    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), BencodeValue::Bytes(b"payload".to_vec()));
    info.insert(b"meta version".to_vec(), BencodeValue::Int(2));
    info.insert(b"piece length".to_vec(), BencodeValue::Int(16384));
    info.insert(b"file tree".to_vec(), BencodeValue::Dict(file_tree));

    let mut layers = BTreeMap::new();
    layers.insert(
        root_hash.to_vec(),
        BencodeValue::Bytes(vec![piece_hash, piece_hash].into_iter().flatten().collect()),
    );
    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));
    root.insert(b"piece layers".to_vec(), BencodeValue::Dict(layers));

    let torrent = TorrentMeta::parse(&BencodeValue::Dict(root).encode()).unwrap();
    assert_eq!(torrent.info.meta_version, Some(2));
    assert_eq!(torrent.info.pieces.len(), 0);
    assert!(torrent.info_hash_v2.is_some());
    assert_eq!(
        torrent.network_info_hash(),
        torrent.info_hash_v2.unwrap()[..20]
    );
    let files = torrent.info.v2_files.as_ref().unwrap();
    assert_eq!(files[0].path, vec!["payload.bin"]);
    assert_eq!(files[0].pieces_root, Some(root_hash));
    assert_eq!(
        torrent.piece_layers.get(&root_hash),
        Some(&vec![piece_hash, piece_hash])
    );
    assert_eq!(torrent.piece_space_size(), 32768);
    assert_eq!(torrent.num_pieces(), 2);
}

#[test]
fn test_v2_file_tree_rejects_invalid_piece_root() {
    let mut leaf = BTreeMap::new();
    leaf.insert(b"length".to_vec(), BencodeValue::Int(1));
    leaf.insert(b"pieces root".to_vec(), BencodeValue::Bytes(vec![0u8; 31]));
    let mut named = BTreeMap::new();
    named.insert(b"".to_vec(), BencodeValue::Dict(leaf));
    let mut tree = BTreeMap::new();
    tree.insert(b"file".to_vec(), BencodeValue::Dict(named));
    let mut info = BTreeMap::new();
    info.insert(b"meta version".to_vec(), BencodeValue::Int(2));
    info.insert(b"piece length".to_vec(), BencodeValue::Int(1));
    info.insert(b"file tree".to_vec(), BencodeValue::Dict(tree));
    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://x".to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));
    assert!(TorrentMeta::parse(&BencodeValue::Dict(root).encode()).is_err());
}

#[test]
fn test_v2_empty_file_may_omit_pieces_root() {
    let mut leaf = BTreeMap::new();
    leaf.insert(b"length".to_vec(), BencodeValue::Int(0));
    let mut named = BTreeMap::new();
    named.insert(b"".to_vec(), BencodeValue::Dict(leaf));
    let mut tree = BTreeMap::new();
    tree.insert(b"empty".to_vec(), BencodeValue::Dict(named));
    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), BencodeValue::Bytes(b"root".to_vec()));
    info.insert(b"meta version".to_vec(), BencodeValue::Int(2));
    info.insert(b"piece length".to_vec(), BencodeValue::Int(16384));
    info.insert(b"file tree".to_vec(), BencodeValue::Dict(tree));
    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));

    let torrent = TorrentMeta::parse(&BencodeValue::Dict(root).encode()).unwrap();
    assert_eq!(torrent.info.v2_files.unwrap()[0].pieces_root, None);
}

#[test]
fn torrent_parser_rejects_control_characters_in_v2_file_tree_paths() {
    let leaf = BTreeMap::from([
        (b"length".to_vec(), BencodeValue::Int(1)),
        (b"pieces root".to_vec(), BencodeValue::Bytes(vec![1; 32])),
    ]);
    let file_node = BTreeMap::from([(Vec::new(), BencodeValue::Dict(leaf))]);
    let file_tree = BTreeMap::from([(b"bad\x01name".to_vec(), BencodeValue::Dict(file_node))]);
    let info = BTreeMap::from([
        (b"name".to_vec(), BencodeValue::Bytes(b"safe-root".to_vec())),
        (b"meta version".to_vec(), BencodeValue::Int(2)),
        (b"piece length".to_vec(), BencodeValue::Int(16_384)),
        (b"file tree".to_vec(), BencodeValue::Dict(file_tree)),
    ]);
    let root = BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        ),
        (b"info".to_vec(), BencodeValue::Dict(info)),
    ]);

    assert!(TorrentMeta::parse(&BencodeValue::Dict(root).encode()).is_err());
}

#[test]
fn torrent_parser_percent_encodes_non_utf8_v2_file_tree_components() {
    let empty_file = BencodeValue::Dict(BTreeMap::from([(
        Vec::new(),
        BencodeValue::Dict(BTreeMap::from([(b"length".to_vec(), BencodeValue::Int(0))])),
    )]));
    let file_tree = BencodeValue::Dict(BTreeMap::from([(vec![0xff], empty_file)]));
    let info = BencodeValue::Dict(BTreeMap::from([
        (b"name".to_vec(), BencodeValue::Bytes(b"safe-root".to_vec())),
        (b"meta version".to_vec(), BencodeValue::Int(2)),
        (b"piece length".to_vec(), BencodeValue::Int(16_384)),
        (b"file tree".to_vec(), file_tree),
    ]));
    let root = BencodeValue::Dict(BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        ),
        (b"info".to_vec(), info),
    ]));

    let metadata =
        TorrentMeta::parse(&root.encode()).expect("non-UTF-8 v2 path bytes are representable");
    assert_eq!(metadata.info.v2_files.unwrap()[0].path, ["%FF"]);
}

#[test]
fn test_parse_hybrid_single_file_validates_both_layouts() {
    let data = b"hybrid payload";
    let sha1_piece: [u8; 20] = sha1::Sha1::digest(data).into();
    let root = crate::bittorrent::torrent::merkle::file_root(data);
    let leaf = BTreeMap::from([
        (b"length".to_vec(), BencodeValue::Int(data.len() as i64)),
        (b"pieces root".to_vec(), BencodeValue::Bytes(root.to_vec())),
    ]);
    let mut file_node = BTreeMap::new();
    file_node.insert(Vec::new(), BencodeValue::Dict(leaf));
    let mut file_tree = BTreeMap::new();
    file_tree.insert(b"hybrid.bin".to_vec(), BencodeValue::Dict(file_node));
    let info = BencodeValue::Dict(BTreeMap::from([
        (b"file tree".to_vec(), BencodeValue::Dict(file_tree)),
        (b"length".to_vec(), BencodeValue::Int(data.len() as i64)),
        (b"meta version".to_vec(), BencodeValue::Int(2)),
        (
            b"name".to_vec(),
            BencodeValue::Bytes(b"hybrid.bin".to_vec()),
        ),
        (b"piece length".to_vec(), BencodeValue::Int(16384)),
        (b"pieces".to_vec(), BencodeValue::Bytes(sha1_piece.to_vec())),
    ]));
    let root_dict = BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.invalid/announce".to_vec()),
        ),
        (b"info".to_vec(), info),
    ]);

    let torrent = TorrentMeta::parse(&BencodeValue::Dict(root_dict).encode()).unwrap();
    assert_eq!(torrent.info.meta_version, Some(2));
    assert_eq!(torrent.info.pieces.len(), 1);
    assert!(torrent.info_hash_v2.is_some());
}

#[test]
fn test_parse_hybrid_rejects_mismatched_v1_length() {
    let root = crate::bittorrent::torrent::merkle::file_root(b"data");
    let leaf = BTreeMap::from([
        (b"length".to_vec(), BencodeValue::Int(4)),
        (b"pieces root".to_vec(), BencodeValue::Bytes(root.to_vec())),
    ]);
    let mut file_node = BTreeMap::new();
    file_node.insert(Vec::new(), BencodeValue::Dict(leaf));
    let mut file_tree = BTreeMap::new();
    file_tree.insert(b"file.bin".to_vec(), BencodeValue::Dict(file_node));
    let info = BencodeValue::Dict(BTreeMap::from([
        (b"file tree".to_vec(), BencodeValue::Dict(file_tree)),
        (b"length".to_vec(), BencodeValue::Int(3)),
        (b"meta version".to_vec(), BencodeValue::Int(2)),
        (b"name".to_vec(), BencodeValue::Bytes(b"file.bin".to_vec())),
        (b"piece length".to_vec(), BencodeValue::Int(16384)),
        (b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20])),
    ]));
    let root_dict = BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.invalid/announce".to_vec()),
        ),
        (b"info".to_vec(), info),
    ]);
    let error = TorrentMeta::parse(&BencodeValue::Dict(root_dict).encode()).unwrap_err();
    assert!(error.contains("hybrid single-file layouts do not match"));
}

#[test]
fn test_parse_hybrid_rejects_padding_at_wrong_position() {
    let piece_length = 16_384i64;
    let files = [
        (
            "one.bin",
            1i64,
            crate::bittorrent::torrent::merkle::file_root(b"1"),
        ),
        (
            "two.bin",
            1i64,
            crate::bittorrent::torrent::merkle::file_root(b"2"),
        ),
    ];
    let mut file_tree = BTreeMap::new();
    for (name, length, root) in files {
        file_tree.insert(
            name.as_bytes().to_vec(),
            BencodeValue::Dict(BTreeMap::from([(
                Vec::new(),
                BencodeValue::Dict(BTreeMap::from([
                    (b"length".to_vec(), BencodeValue::Int(length)),
                    (b"pieces root".to_vec(), BencodeValue::Bytes(root.to_vec())),
                ])),
            )])),
        );
    }
    let content_file = |name: &[u8], length: i64| {
        BencodeValue::Dict(BTreeMap::from([
            (b"length".to_vec(), BencodeValue::Int(length)),
            (
                b"path".to_vec(),
                BencodeValue::List(vec![BencodeValue::Bytes(name.to_vec())]),
            ),
        ]))
    };
    let padding = BencodeValue::Dict(BTreeMap::from([
        (b"length".to_vec(), BencodeValue::Int(16_383)),
        (
            b"path".to_vec(),
            BencodeValue::List(vec![
                BencodeValue::Bytes(b".pad".to_vec()),
                BencodeValue::Bytes(b"16383".to_vec()),
            ]),
        ),
    ]));
    let info = BencodeValue::Dict(BTreeMap::from([
        (b"file tree".to_vec(), BencodeValue::Dict(file_tree)),
        (b"meta version".to_vec(), BencodeValue::Int(2)),
        (b"name".to_vec(), BencodeValue::Bytes(b"root".to_vec())),
        (b"piece length".to_vec(), BencodeValue::Int(piece_length)),
        (b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 40])),
        (
            b"files".to_vec(),
            BencodeValue::List(vec![
                content_file(b"one.bin", 1),
                content_file(b"two.bin", 1),
                padding,
            ]),
        ),
    ]));
    let root = BencodeValue::Dict(BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://x".to_vec()),
        ),
        (b"info".to_vec(), info),
    ]));

    let error = TorrentMeta::parse(&root.encode()).unwrap_err();
    assert!(error.contains("hybrid padding files do not match"));
}

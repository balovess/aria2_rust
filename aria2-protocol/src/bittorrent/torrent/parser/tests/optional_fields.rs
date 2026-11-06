use super::*;

#[test]
fn test_parse_with_optional_fields() {
    let _data = make_simple_torrent();

    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
    );
    root.insert(
        b"comment".to_vec(),
        BencodeValue::Bytes(b"A test torrent".to_vec()),
    );
    root.insert(
        b"created by".to_vec(),
        BencodeValue::Bytes(b"aria2-rust-tester".to_vec()),
    );
    root.insert(b"creation date".to_vec(), BencodeValue::Int(1700000000));
    root.insert(
        b"nodes".to_vec(),
        BencodeValue::List(vec![
            BencodeValue::List(vec![
                BencodeValue::Bytes(b" router.example ".to_vec()),
                BencodeValue::Int(6881),
            ]),
            BencodeValue::List(vec![
                BencodeValue::Bytes(b"invalid-port".to_vec()),
                BencodeValue::Int(0),
            ]),
        ]),
    );

    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test.bin".to_vec()));
    info.insert(b"length".to_vec(), BencodeValue::Int(100));
    info.insert(b"piece length".to_vec(), BencodeValue::Int(50));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 40]));
    info.insert(b"private".to_vec(), BencodeValue::Int(1));

    root.insert(b"info".to_vec(), BencodeValue::Dict(info));

    let data = BencodeValue::Dict(root).encode();
    let t = TorrentMeta::parse(&data).unwrap();
    assert_eq!(t.comment.as_deref(), Some("A test torrent"));
    assert_eq!(t.created_by.as_deref(), Some("aria2-rust-tester"));
    assert_eq!(t.creation_date, Some(1700000000));
    assert_eq!(t.nodes, vec![("router.example".to_string(), 6881)]);
    assert!(t.is_private());
}

#[test]
fn test_parse_web_seeds_single() {
    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test.bin".to_vec()));
    info.insert(b"length".to_vec(), BencodeValue::Int(1024));
    info.insert(b"piece length".to_vec(), BencodeValue::Int(512));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 40]));

    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
    );
    root.insert(
        b"url-list".to_vec(),
        BencodeValue::Bytes(b"http://webseed.example.com/file.bin".to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));

    let data = BencodeValue::Dict(root).encode();
    let torrent = TorrentMeta::parse(&data).unwrap();

    assert_eq!(torrent.web_seeds.len(), 1);
    assert_eq!(torrent.web_seeds[0], "http://webseed.example.com/file.bin");
}

#[test]
fn test_parse_web_seeds_multiple() {
    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test.bin".to_vec()));
    info.insert(b"length".to_vec(), BencodeValue::Int(2048));
    info.insert(b"piece length".to_vec(), BencodeValue::Int(512));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 80]));

    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
    );
    root.insert(
        b"url-list".to_vec(),
        BencodeValue::List(vec![
            BencodeValue::Bytes(b"http://seed1.example.com/file.bin".to_vec()),
            BencodeValue::Bytes(b"http://seed2.example.com/file.bin".to_vec()),
            BencodeValue::Bytes(b"https://seed3.example.com/file.bin".to_vec()),
        ]),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));

    let data = BencodeValue::Dict(root).encode();
    let torrent = TorrentMeta::parse(&data).unwrap();

    assert_eq!(torrent.web_seeds.len(), 3);
    assert_eq!(torrent.web_seeds[0], "http://seed1.example.com/file.bin");
    assert_eq!(torrent.web_seeds[1], "http://seed2.example.com/file.bin");
    assert_eq!(torrent.web_seeds[2], "https://seed3.example.com/file.bin");
}

#[test]
fn test_parse_web_seeds_missing() {
    let data = make_simple_torrent();
    let torrent = TorrentMeta::parse(&data).unwrap();
    assert!(torrent.web_seeds.is_empty());
}

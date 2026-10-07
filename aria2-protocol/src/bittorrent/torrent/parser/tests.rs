use super::*;
use sha1::Digest;
use std::collections::BTreeMap;

fn make_simple_torrent() -> Vec<u8> {
    let mut pieces_data = vec![0u8; 40];
    for (i, piece) in pieces_data.iter_mut().enumerate().take(40) {
        *piece = i as u8;
    }

    let mut info = BTreeMap::new();
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"test_file.bin".to_vec()),
    );
    info.insert(b"length".to_vec(), BencodeValue::Int(1024));
    info.insert(b"piece length".to_vec(), BencodeValue::Int(512));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(pieces_data));

    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));

    BencodeValue::Dict(root).encode()
}

fn make_torrent_with_name_and_path(name: &str, path: Option<&[&str]>) -> Vec<u8> {
    let mut info = BTreeMap::from([
        (
            b"name".to_vec(),
            BencodeValue::Bytes(name.as_bytes().to_vec()),
        ),
        (b"piece length".to_vec(), BencodeValue::Int(16_384)),
        (b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20])),
    ]);
    if let Some(components) = path {
        let file = BTreeMap::from([
            (b"length".to_vec(), BencodeValue::Int(1)),
            (
                b"path".to_vec(),
                BencodeValue::List(
                    components
                        .iter()
                        .map(|component| BencodeValue::Bytes(component.as_bytes().to_vec()))
                        .collect(),
                ),
            ),
        ]);
        info.insert(
            b"files".to_vec(),
            BencodeValue::List(vec![BencodeValue::Dict(file)]),
        );
    } else {
        info.insert(b"length".to_vec(), BencodeValue::Int(1));
    }

    let root = BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
        ),
        (b"info".to_vec(), BencodeValue::Dict(info)),
    ]);
    BencodeValue::Dict(root).encode()
}

#[path = "tests/basic.rs"]
mod basic;
#[path = "tests/optional_fields.rs"]
mod optional_fields;
#[path = "tests/v2.rs"]
mod v2;

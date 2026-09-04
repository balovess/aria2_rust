#![allow(dead_code)]
use sha1::{Digest, Sha1};
use sha2::Sha256;

pub fn build_test_torrent(
    name: &str,
    total_size: u64,
    piece_length: u32,
    tracker_url: &str,
) -> Vec<u8> {
    build_test_torrent_with_web_seeds(name, total_size, piece_length, tracker_url, &[])
}

pub fn build_test_torrent_with_web_seeds(
    name: &str,
    total_size: u64,
    piece_length: u32,
    tracker_url: &str,
    web_seed_urls: &[String],
) -> Vec<u8> {
    let file_data = generate_file_data(total_size);
    let num_pieces = total_size.div_ceil(piece_length as u64) as usize;
    let mut pieces_hash = Vec::with_capacity(num_pieces * 20);

    for i in 0..num_pieces {
        let start = i * piece_length as usize;
        let end = std::cmp::min(start + piece_length as usize, file_data.len());
        let mut hasher = Sha1::new();
        hasher.update(&file_data[start..end]);
        pieces_hash.extend_from_slice(&hasher.finalize());
    }

    let info_dict = build_info_dict(name, total_size, piece_length, &pieces_hash);

    let announce_key = b"announce";
    let announce_val = bencode_str(tracker_url);

    let info_key = b"info";
    let info_val = bencode_dict(&info_dict);

    let mut torrent_entries = vec![
        (announce_key.to_vec(), announce_val),
        (info_key.to_vec(), info_val),
    ];
    if !web_seed_urls.is_empty() {
        let web_seed_value = if web_seed_urls.len() == 1 {
            bencode_str(&web_seed_urls[0])
        } else {
            bencode_list(
                &web_seed_urls
                    .iter()
                    .map(|url| bencode_str(url))
                    .collect::<Vec<_>>(),
            )
        };
        torrent_entries.push((b"url-list".to_vec(), web_seed_value));
    }

    bencode_dict(&torrent_entries)
}

/// Build a multi-file torrent whose files contain one contiguous generated
/// byte stream. Keeping the piece hashes over that stream lets E2E tests
/// exercise pieces that cross physical file boundaries.
pub fn build_multi_file_test_torrent(
    name: &str,
    file_lengths: &[u64],
    piece_length: u32,
    tracker_url: &str,
) -> Vec<u8> {
    let total_size: u64 = file_lengths.iter().sum();
    let file_data = generate_file_data(total_size);
    let num_pieces = total_size.div_ceil(piece_length as u64) as usize;
    let mut pieces_hash = Vec::with_capacity(num_pieces * 20);
    for i in 0..num_pieces {
        let start = i * piece_length as usize;
        let end = (start + piece_length as usize).min(file_data.len());
        let mut hasher = Sha1::new();
        hasher.update(&file_data[start..end]);
        pieces_hash.extend_from_slice(&hasher.finalize());
    }

    let mut files = Vec::with_capacity(file_lengths.len());
    for (index, length) in file_lengths.iter().enumerate() {
        let path = format!("part-{index}.bin");
        let file_dict = vec![
            (b"length".to_vec(), bencode_int(*length)),
            (b"path".to_vec(), bencode_list(&[bencode_str(&path)])),
        ];
        files.push(bencode_dict(&file_dict));
    }

    let info_dict = vec![
        (b"files".to_vec(), bencode_list(&files)),
        (b"name".to_vec(), bencode_str(name)),
        (b"piece length".to_vec(), bencode_int(piece_length as u64)),
        (b"pieces".to_vec(), bencode_bytes(&pieces_hash)),
    ];
    bencode_dict(&[
        (b"announce".to_vec(), bencode_str(tracker_url)),
        (b"info".to_vec(), bencode_dict(&info_dict)),
    ])
}

pub fn generate_file_data(size: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(size as usize);
    for i in 0..size {
        data.push((i % 256) as u8);
    }
    data
}

/// Build a BEP 52 v2 single-file torrent and return its payload alongside the
/// metainfo. The v2 piece layer is included only when the file spans more
/// than one 16 KiB piece.
pub fn build_v2_test_torrent(
    name: &str,
    total_size: u64,
    piece_length: u32,
    tracker_url: &str,
) -> (Vec<u8>, Vec<u8>) {
    assert!(piece_length >= 16 * 1024);
    assert!(piece_length.is_power_of_two());
    let data = generate_file_data(total_size);
    let piece_count = total_size.div_ceil(piece_length as u64) as usize;
    let piece_roots: Vec<[u8; 32]> = (0..piece_count)
        .map(|index| {
            let start = index * piece_length as usize;
            let end = (start + piece_length as usize).min(data.len());
            v2_piece_root(&data[start..end], piece_length as usize)
        })
        .collect();
    let file_root = v2_root_from_layer(&piece_roots);

    let leaf = vec![
        (b"length".to_vec(), bencode_int(total_size)),
        (b"pieces root".to_vec(), bencode_bytes(&file_root)),
    ];
    let mut piece_layers = Vec::new();
    if total_size > piece_length as u64 {
        let layer: Vec<u8> = piece_roots
            .iter()
            .flat_map(|root| root.iter())
            .copied()
            .collect();
        piece_layers.push((file_root.to_vec(), bencode_bytes(&layer)));
    }

    let file_tree = bencode_dict(&[(
        name.as_bytes().to_vec(),
        bencode_dict(&[(b"".to_vec(), bencode_dict(&leaf))]),
    )]);
    let info = bencode_dict(&[
        (b"file tree".to_vec(), file_tree),
        (b"meta version".to_vec(), bencode_int(2)),
        (b"name".to_vec(), bencode_str(name)),
        (b"piece length".to_vec(), bencode_int(piece_length as u64)),
    ]);
    let mut root_entries = vec![
        (b"announce".to_vec(), bencode_str(tracker_url)),
        (b"info".to_vec(), info),
    ];
    if !piece_layers.is_empty() {
        root_entries.push((b"piece layers".to_vec(), bencode_dict(&piece_layers)));
    }
    (bencode_dict(&root_entries), data)
}

/// Build a minimal multi-file v2 torrent with a per-file alignment gap.
/// Returning the logical piece payloads lets the independent mock peer serve
/// the same v2 piece address space as the production downloader.
pub fn build_v2_multi_file_test_torrent(tracker_url: &str) -> (Vec<u8>, Vec<Vec<u8>>) {
    let piece_length = 16 * 1024u32;
    let pieces = vec![vec![0x11u8], vec![0x22u8]];
    let names = [b"one.bin".as_slice(), b"two.bin".as_slice()];
    let mut file_tree = Vec::new();
    for (name, data) in names.iter().zip(&pieces) {
        let root = v2_piece_root(data, piece_length as usize);
        let leaf = bencode_dict(&[
            (b"length".to_vec(), bencode_int(data.len() as u64)),
            (b"pieces root".to_vec(), bencode_bytes(&root)),
        ]);
        file_tree.push((name.to_vec(), bencode_dict(&[(Vec::new(), leaf)])));
    }
    let info = bencode_dict(&[
        (b"file tree".to_vec(), bencode_dict(&file_tree)),
        (b"meta version".to_vec(), bencode_int(2)),
        (b"name".to_vec(), bencode_str("v2-multi")),
        (b"piece length".to_vec(), bencode_int(piece_length as u64)),
    ]);
    (
        bencode_dict(&[
            (b"announce".to_vec(), bencode_str(tracker_url)),
            (b"info".to_vec(), info),
        ]),
        pieces,
    )
}

/// Build the same aligned layout as a BEP 52 hybrid torrent. The v1 piece
/// stream includes the zero-filled padding file while the v2 roots cover only
/// the two one-byte payload files.
pub fn build_hybrid_multi_file_test_torrent(
    tracker_url: &str,
) -> (Vec<u8>, Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let piece_length = 16 * 1024usize;
    let content = vec![vec![0x31u8], vec![0x32u8]];
    let mut wire_pieces = vec![vec![0u8; piece_length], vec![0x32u8]];
    wire_pieces[0][0] = content[0][0];
    let pieces_hash: Vec<u8> = wire_pieces
        .iter()
        .flat_map(|piece| sha1::Sha1::digest(piece).to_vec())
        .collect();

    let mut file_tree = Vec::new();
    for (name, data) in [b"one.bin".as_slice(), b"two.bin".as_slice()]
        .iter()
        .zip(&content)
    {
        let root = v2_piece_root(data, piece_length);
        let leaf = bencode_dict(&[
            (b"length".to_vec(), bencode_int(data.len() as u64)),
            (b"pieces root".to_vec(), bencode_bytes(&root)),
        ]);
        file_tree.push((name.to_vec(), bencode_dict(&[(Vec::new(), leaf)])));
    }
    let content_file = |name: &[u8], length: u64| {
        bencode_dict(&[
            (b"length".to_vec(), bencode_int(length)),
            (b"path".to_vec(), bencode_list(&[bencode_bytes(name)])),
        ])
    };
    let padding_file = bencode_dict(&[
        (b"length".to_vec(), bencode_int((piece_length - 1) as u64)),
        (
            b"path".to_vec(),
            bencode_list(&[bencode_bytes(b".pad"), bencode_bytes(b"16383")]),
        ),
    ]);
    let info = bencode_dict(&[
        (b"file tree".to_vec(), bencode_dict(&file_tree)),
        (
            b"files".to_vec(),
            bencode_list(&[
                content_file(b"one.bin", 1),
                padding_file,
                content_file(b"two.bin", 1),
            ]),
        ),
        (b"meta version".to_vec(), bencode_int(2)),
        (b"name".to_vec(), bencode_str("hybrid-multi")),
        (b"piece length".to_vec(), bencode_int(piece_length as u64)),
        (b"pieces".to_vec(), bencode_bytes(&pieces_hash)),
    ]);
    (
        bencode_dict(&[
            (b"announce".to_vec(), bencode_str(tracker_url)),
            (b"info".to_vec(), info),
        ]),
        wire_pieces,
        content,
    )
}

fn v2_piece_root(data: &[u8], piece_length: usize) -> [u8; 32] {
    let mut leaves: Vec<[u8; 32]> = data
        .chunks(16 * 1024)
        .map(|chunk| Sha256::digest(chunk).into())
        .collect();
    leaves.resize(piece_length / (16 * 1024), [0u8; 32]);
    v2_reduce_tree(leaves)
}

fn v2_root_from_layer(layer: &[[u8; 32]]) -> [u8; 32] {
    let mut leaves = layer.to_vec();
    leaves.resize(leaves.len().next_power_of_two(), [0u8; 32]);
    v2_reduce_tree(leaves)
}

fn v2_reduce_tree(mut hashes: Vec<[u8; 32]>) -> [u8; 32] {
    while hashes.len() > 1 {
        hashes = hashes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let mut input = [0u8; 64];
                input[..32].copy_from_slice(&pair[0]);
                input[32..].copy_from_slice(&pair[1]);
                Sha256::digest(input).into()
            })
            .collect();
    }
    hashes[0]
}

pub fn expected_piece_data(piece_index: u32, piece_length: u32, total_size: u64) -> Vec<u8> {
    let start = piece_index as u64 * piece_length as u64;
    let end = std::cmp::min(start + piece_length as u64, total_size);
    let mut data = Vec::with_capacity((end - start) as usize);
    for i in start..end {
        data.push((i % 256) as u8);
    }
    data
}

fn build_info_dict(
    name: &str,
    total_size: u64,
    piece_length: u32,
    pieces_hash: &[u8],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![
        (b"length".to_vec(), bencode_int(total_size)),
        (b"name".to_vec(), bencode_str(name)),
        (b"piece length".to_vec(), bencode_int(piece_length as u64)),
        (b"pieces".to_vec(), bencode_bytes(pieces_hash)),
    ]
}

fn bencode_int(v: u64) -> Vec<u8> {
    format!("i{}e", v).into_bytes()
}
fn bencode_str(s: &str) -> Vec<u8> {
    format!("{}:{}", s.len(), s).into_bytes()
}
fn bencode_bytes(b: &[u8]) -> Vec<u8> {
    format!("{}:", b.len())
        .into_bytes()
        .into_iter()
        .chain(b.iter().copied())
        .collect()
}

fn bencode_list(values: &[Vec<u8>]) -> Vec<u8> {
    let mut result = b"l".to_vec();
    for value in values {
        result.extend_from_slice(value);
    }
    result.push(b'e');
    result
}

fn bencode_dict(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut result = b"d".to_vec();
    for (k, v) in entries {
        result.extend_from_slice(&(k.len().to_string().into_bytes()));
        result.push(b':');
        result.extend_from_slice(k);
        result.extend_from_slice(v);
    }
    result.push(b'e');
    result
}

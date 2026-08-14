#![allow(dead_code)]
use sha1::{Digest, Sha1};

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

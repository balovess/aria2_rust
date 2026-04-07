use sha1::{Sha1, Digest};

pub fn build_test_torrent(name: &str, total_size: u64, piece_length: u32, tracker_url: &str) -> Vec<u8> {
    let file_data = generate_file_data(total_size);
    let num_pieces = ((total_size + piece_length as u64 - 1) / piece_length as u64) as usize;
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

    let torrent = bencode_dict(&[
        (announce_key.to_vec(), announce_val),
        (info_key.to_vec(), info_val),
    ]);

    torrent
}

pub fn generate_file_data(size: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(size as usize);
    for i in 0..size { data.push((i % 256) as u8); }
    data
}

pub fn expected_piece_data(piece_index: u32, piece_length: u32, total_size: u64) -> Vec<u8> {
    let start = piece_index as u64 * piece_length as u64;
    let end = std::cmp::min(start + piece_length as u64, total_size);
    let mut data = Vec::with_capacity((end - start) as usize);
    for i in start..end { data.push((i % 256) as u8); }
    data
}

fn build_info_dict(name: &str, total_size: u64, piece_length: u32, pieces_hash: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![
        (b"length".to_vec(), bencode_int(total_size)),
        (b"name".to_vec(), bencode_str(name)),
        (b"piece length".to_vec(), bencode_int(piece_length as u64)),
        (b"pieces".to_vec(), bencode_bytes(pieces_hash)),
    ]
}

fn bencode_int(v: u64) -> Vec<u8> { format!("i{}e", v).into_bytes() }
fn bencode_str(s: &str) -> Vec<u8> { format!("{}:{}", s.len(), s).into_bytes() }
fn bencode_bytes(b: &[u8]) -> Vec<u8> { format!("{}:", b.len()).into_bytes().into_iter().chain(b.iter().copied()).collect() }

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

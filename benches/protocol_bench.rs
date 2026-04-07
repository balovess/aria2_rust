use criterion::{criterion_group, Criterion, black_box};

fn bench_bencode_encode_dict(c: &mut Criterion) {
    let data: std::collections::BTreeMap<String, aria2_protocol::bittorrent::bencode::BencodeValue> =
        (0..20).map(|i| (format!("key{}", i), aria2_protocol::bittorrent::bencode::BencodeValue::Int(i))).collect();
    c.bench(BenchmarkId::new("bencode_encode_dict_20_items"), |b| {
        b.iter_with_black_input(&data, |d| {
            let encoded = aria2_protocol::bittorrent::bencode::BencodeValue::Dict(d.clone());
            black_box(encoded.encode().len());
        });
    });
}

fn bench_bencode_encode_list(c: &mut Criterion) {
    let items: Vec<aria2_protocol::bittorrent::bencode::BencodeValue> =
        (0..50).map(|i| aria2_protocol::bittorrent::bencode::BencodeValue::Int(i)).collect();
    c.bench(BenchmarkId::new("bencode_encode_list_50_items"), |b| {
        b.iter_with_black_input(&items, |itms| {
            let encoded = aria2_protocol::bittorrent::bencode::BencodeValue::List(itms.clone());
            black_box(encoded.encode().len());
        });
    });
}

fn bench_bencode_encode_bytes(c: &mut Criterion) {
    let data: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
    c.bench(BenchmarkId::new("bencode_encode_bytes_4KB"), |b| {
        b.iter_with_black_input(&data, |d| {
            let val = aria2_protocol::bittorrent::bencode::BencodeValue::Bytes(d.clone());
            black_box(val.encode().len());
        });
    });
}

fn bench_bencode_decode_bytes(c: &mut Criterion) {
    let raw = aria2_protocol::bittorrent::bencode::BencodeValue::Bytes(
        (0..4096).map(|i| (i % 256) as u8).collect()
    ).encode();
    c.bench(BenchmarkId::new("bencode_decode_bytes_4KB"), |b| {
        b.iter_with_black_input(&raw, |r| {
            let decoded = aria2_protocol::bittorrent::bencode::BencodeValue::decode(r);
            black_box(decoded.is_ok());
        });
    });
}

fn bench_bt_handshake_build(c: &mut Criterion) {
    c.bench_function("bt_handshake_build", |b| {
        b.iter(|| {
            let handshake = aria2_protocol::bittorrent::message::handshake::HandshakeMessage::new();
            black_box(handshake.len());
        });
    });
}

fn bench_info_hash_calculation(c: &mut Criterion) {
    let info_dict_data = br#"{"name":"test.iso","length":1048576,"piece length":262144,"pieces":["abc123def456"]}"#;
    c.bench(BenchmarkId::new("info_hash_sha1_1KB_dict"), |b| {
        b.iter_with_black_input(info_dict_data, |data| {
            let hash = aria2_protocol::bittorrent::torrent::TorrentMeta::compute_info_hash(data);
            black_box(hash.len());
        });
    });
}

fn bench_dht_xor_distance(c: &mut Criterion) {
    let target: [u8; 20] = [0xFF; 20];
    let nodes: Vec<[u8; 20]> = (0..1000).map(|i| {
        let mut id = [0u8; 20];
        id[0] = (i >> 24) as u8;
        id[1] = (i >> 16) as u8;
        id[2] = (i >> 8) as u8;
        id[3] = i as u8;
        id
    }).collect();
    c.bench(BenchmarkId::new("dht_xor_distance_1000_nodes"), |b| {
        b.iter_with_black_input(&nodes, |ns| {
            let mut total_dist: u64 = 0;
            for n in ns.iter() {
                for (a, b) in target.iter().zip(n.iter()) {
                    total_dist += (*a ^ *b) as u64;
                }
            }
            black_box(total_dist);
        });
    });
}

fn bench_http_request_build(c: &mut Criterion) {
    c.bench_function("http_request_build", |b| {
        b.iter(|| {
            let uri = "http://example.com/large-file.iso";
            let headers = vec![
                ("User-Agent".to_string(), "aria2-rust/0.1.0".to_string()),
                ("Range".to_string(), "bytes=0-1048575".to_string()),
                ("Accept-Encoding".to_string(), "gzip, deflate".to_string()),
            ];
            black_box((uri.len(), headers.len()));
        });
    });
}

fn bench_checksum_md5_10mb(c: &mut Criterion) {
    let data: Vec<u8> = (0..(10 * 1024 * 1024)).map(|i| (i % 256) as u8).collect();
    use md5::Digest;
    c.bench(BenchmarkId::new("checksum_md5_10MB"), |b| {
        b.iter_with_black_input(&data, |d| {
            let mut hasher = md5::Md5::new();
            hasher.update(d);
            let result = hasher.finalize();
            black_box(result.len());
        });
    });
}

fn bench_checksum_sha256_10mb(c: &mut Criterion) {
    let data: Vec<u8> = (0..(10 * 1024 * 1024)).map(|i| (i % 256) as u8).collect();
    use sha2::Sha256;
    c.bench(BenchmarkId::new("checksum_sha256_10MB"), |b| {
        b.iter_with_black_input(&data, |d| {
            let mut hasher = Sha256::new();
            hasher.update(d);
            let result = hasher.finalize();
            black_box(result.len());
        });
    });
}

fn bench_serde_json_parse(c: &mut Criterion) {
    let json_str = r#"{"version":"2.0","method":"aria2.addUri","params":[["http://example.com/file.zip","http://mirror.com/file.zip"],"options":{"dir":"/downloads","split":4},"id":"req-1"}"#;
    c.bench(BenchmarkId::new("serde_json_parse_complex_object"), |b| {
        b.iter_with_black_input(json_str, |s| {
            let val: Result<serde_json::Value, _> = serde_json::from_str(s);
            black_box(val.map_or(0, |v| v.to_string().len()));
        });
    });
}

fn bench_serde_json_serialize(c: &mut Criterion) {
    let value = serde_json::json!({
        "version": "2.0",
        "result": "gid-00123456789abcdef",
        "id": "req-1"
    });
    c.bench(BenchmarkId::new("serde_json_serialize_response"), |b| {
        b.iter(|| {
            let s = serde_json::to_string(&value);
            black_box(s.ok());
        });
    });
}

criterion_group!(protocol_benches,
    bench_bencode_encode_dict,
    bench_bencode_encode_list,
    bench_bencode_encode_bytes,
    bench_bencode_decode_bytes,
    bench_bt_handshake_build,
    bench_info_hash_calculation,
    bench_dht_xor_distance,
    bench_http_request_build,
    bench_checksum_md5_10mb,
    bench_checksum_sha256_10mb,
    bench_serde_json_parse,
    bench_serde_json_serialize,
);

fn main() {
    let mut c = Criterion::default().sample_size(100).warm_up_time(std::time::Duration::from_millis(300));
    protocol_benches(&mut c);
}

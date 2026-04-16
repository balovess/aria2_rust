use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use aria2_protocol::bittorrent::message::handshake::Handshake;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group};
use sha1::{Digest, Sha1};

fn bench_bencode_encode_dict(c: &mut Criterion) {
    let data: std::collections::BTreeMap<Vec<u8>, BencodeValue> = (0..20)
        .map(|i| (format!("key{}", i).into_bytes(), BencodeValue::Int(i)))
        .collect();
    c.bench_with_input(
        BenchmarkId::new("bencode_encode_dict_20_items", 20),
        &data,
        |b, d| {
            b.iter(|| {
                let encoded = BencodeValue::Dict(d.clone()).encode();
                black_box(encoded.len());
            });
        },
    );
}

fn bench_bencode_encode_list(c: &mut Criterion) {
    let items: Vec<BencodeValue> = (0..50).map(BencodeValue::Int).collect();
    c.bench_with_input(
        BenchmarkId::new("bencode_encode_list_50_items", 50),
        &items,
        |b, itms| {
            b.iter(|| {
                let encoded = BencodeValue::List(itms.clone()).encode();
                black_box(encoded.len());
            });
        },
    );
}

fn bench_bencode_encode_bytes(c: &mut Criterion) {
    let data: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
    c.bench_with_input(
        BenchmarkId::new("bencode_encode_bytes_4KB", 4096),
        &data,
        |b, d| {
            b.iter(|| {
                let val = BencodeValue::Bytes(d.clone());
                black_box(val.encode().len());
            });
        },
    );
}

fn bench_bencode_decode_bytes(c: &mut Criterion) {
    let raw = BencodeValue::Bytes((0..4096).map(|i| (i % 256) as u8).collect()).encode();
    c.bench_with_input(
        BenchmarkId::new("bencode_decode_bytes_4KB", 4096),
        &raw,
        |b, r| {
            b.iter(|| {
                let decoded = BencodeValue::decode(r);
                black_box(decoded.is_ok());
            });
        },
    );
}

fn bench_bt_handshake_build(c: &mut Criterion) {
    let info_hash: [u8; 20] = [0xAB; 20];
    let peer_id: [u8; 20] = [0xCD; 20];
    c.bench_function("bt_handshake_build", |b| {
        b.iter(|| {
            let handshake = Handshake::new(&info_hash, &peer_id);
            black_box(handshake.info_hash.len());
        });
    });
}

fn bench_sha1_hash(c: &mut Criterion) {
    let data: Vec<u8> = (0..(1024 * 1024)).map(|i| (i % 256) as u8).collect();
    c.bench_with_input(BenchmarkId::new("sha1_hash_1MB", 1), &data, |b, d| {
        b.iter(|| {
            let mut hasher = Sha1::new();
            hasher.update(d);
            let result = hasher.finalize();
            black_box(result.len());
        });
    });
}

fn bench_dht_xor_distance(c: &mut Criterion) {
    let target: [u8; 20] = [0xFF; 20];
    let nodes: Vec<[u8; 20]> = (0..1000)
        .map(|i| {
            let mut id = [0u8; 20];
            id[0] = (i >> 24) as u8;
            id[1] = (i >> 16) as u8;
            id[2] = (i >> 8) as u8;
            id[3] = i as u8;
            id
        })
        .collect();
    c.bench_with_input(
        BenchmarkId::new("dht_xor_distance_1000_nodes", 1000),
        &nodes,
        |b, ns| {
            b.iter(|| {
                let mut total_dist: u64 = 0;
                for n in ns.iter() {
                    for (a, b) in target.iter().zip(n.iter()) {
                        total_dist += (*a ^ *b) as u64;
                    }
                }
                black_box(total_dist);
            });
        },
    );
}

fn bench_serde_json_parse(c: &mut Criterion) {
    let json_str: String = r#"{"version":"2.0","method":"aria2.addUri","params":[["http://example.com/file.zip","http://mirror.com/file.zip"],"options":{"dir":"/downloads","split":4},"id":"req-1"}"#.to_string();
    c.bench_function("serde_json_parse_complex_object", |b| {
        b.iter(|| {
            let val: Result<serde_json::Value, _> = serde_json::from_str(&json_str);
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
    c.bench_function("serde_json_serialize_response", |b| {
        b.iter(|| {
            let s = serde_json::to_string(&value);
            black_box(s.ok());
        });
    });
}

criterion_group!(
    protocol_benches,
    bench_bencode_encode_dict,
    bench_bencode_encode_list,
    bench_bencode_encode_bytes,
    bench_bencode_decode_bytes,
    bench_bt_handshake_build,
    bench_sha1_hash,
    bench_dht_xor_distance,
    bench_serde_json_parse,
    bench_serde_json_serialize,
);

fn main() {
    protocol_benches();
}

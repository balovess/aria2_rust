//! P2 new module performance benchmarks
//!
//! Coverage: auth system / LPD discovery / MSE encryption / stream decoder / BT progress persistence

use aria2_core::auth::credential_store::CredentialStore;
use aria2_core::auth::digest_auth::{
    AuthChallenge, DigestAlgorithm, DigestAuthProvider, parse_www_authenticate,
};
use aria2_core::engine::bt_progress_info_file::{BtProgress, BtProgressManager, DownloadStats};
use aria2_core::engine::lpd_manager::{LpdManager, parse_lpd_announcement};
use aria2_core::http::stream_filter::{ChunkedDecoder, GZipDecoder, StreamFilter, process_filters};
use aria2_protocol::bittorrent::extension::mse_crypto::Arc4Cipher;
use aria2_protocol::bittorrent::extension::mse_dh::MseDhKeyExchange;
use aria2_protocol::bittorrent::extension::mse_handshake::MseHandshake;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write as _;

// ====== Helper functions ======

fn make_test_hash(seed: u8) -> [u8; 20] {
    let mut hash = [0u8; 20];
    for (i, byte) in hash.iter_mut().enumerate() {
        *byte = seed.wrapping_mul(i as u8).wrapping_add(0xAB);
    }
    hash
}

fn create_digest_provider(algo: DigestAlgorithm) -> DigestAuthProvider {
    DigestAuthProvider::new(
        "benchmark_user".to_string(),
        "bench_pass123".to_string(),
        Some(algo),
    )
}

fn create_challenge() -> AuthChallenge {
    AuthChallenge {
        scheme: aria2_core::auth::digest_auth::AuthScheme::Digest {
            algorithm: DigestAlgorithm::Md5,
        },
        realm: "testrealm@host.com".to_string(),
        nonce: Some("dcd98b7102dd2f0e8b11d0f600bfb0c093".to_string()),
        opaque: Some("5ccc069c403ebaf9f0171e9517f40e41".to_string()),
        qop: Some("auth".to_string()),
        stale: false,
    }
}

fn create_populated_store(count: usize) -> CredentialStore {
    let store = CredentialStore::new();
    for i in 0..count {
        let domain = format!("domain{}.example.com", i);
        store.store(
            &domain,
            &format!("user{}", i),
            format!("pass{}", i).as_bytes(),
        );
    }
    store
}

fn compress_gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn build_chunked_data(total_size: usize, chunk_size: usize) -> Vec<u8> {
    let mut result = Vec::new();
    let mut remaining = total_size;
    while remaining > 0 {
        let size = chunk_size.min(remaining);
        result.extend_from_slice(format!("{:x}\r\n", size).as_bytes());
        result.extend_from_slice(&vec![0xAB; size]);
        result.extend_from_slice(b"\r\n");
        remaining -= size;
    }
    result.extend_from_slice(b"0\r\n\r\n");
    result
}

fn create_large_progress(num_pieces: u32) -> BtProgress {
    let bitfield_len = num_pieces.div_ceil(8) as usize;
    let bitfield: Vec<u8> = (0..bitfield_len)
        .map(|i| if i < bitfield_len - 1 { 0xFF } else { 0x0F })
        .collect();
    let peers: Vec<_> = (0..10.min(num_pieces))
        .map(|i| aria2_core::engine::bt_progress_info_file::PeerAddr {
            ip: format!("192.168.1.{}", i),
            port: 6881 + i as u16,
        })
        .collect();

    BtProgress {
        info_hash: make_test_hash(0x42),
        bitfield,
        peers,
        stats: DownloadStats {
            uploaded_bytes: num_pieces as u64 * 256 * 1024,
            downloaded_bytes: num_pieces as u64 * 512 * 1024,
            upload_speed: 1024.0 * 512.0,
            download_speed: 1024.0 * 2048.0,
            elapsed_seconds: 3600,
        },
        piece_length: 256 * 1024,
        total_size: num_pieces as u64 * 256 * 1024,
        num_pieces,
        upload_length: num_pieces as u64 * 256 * 1024,
        in_flight_pieces: Vec::new(),
        is_torrent: true,
        save_time: std::time::SystemTime::now(),
        version: 1,
    }
}

// ====== Auth System Benchmarks (4) ======

fn bench_digest_md5_build_header(c: &mut Criterion) {
    let provider = create_digest_provider(DigestAlgorithm::Md5);
    let challenge = create_challenge();
    c.bench_function("auth_digest_md5_build_header", |b| {
        b.iter(|| {
            let header = black_box(provider.build_authorization_header_with_method(
                black_box(&challenge),
                black_box("GET"),
                black_box("/dir/index.html"),
                black_box(None),
            ));
            black_box(header)
        });
    });
}

fn bench_digest_sha256_build_header(c: &mut Criterion) {
    let provider = create_digest_provider(DigestAlgorithm::Sha256);
    let challenge = create_challenge();
    c.bench_function("auth_digest_sha256_build_header", |b| {
        b.iter(|| {
            let header = black_box(provider.build_authorization_header_with_method(
                black_box(&challenge),
                black_box("GET"),
                black_box("/dir/index.html"),
                black_box(None),
            ));
            black_box(header)
        });
    });
}

fn bench_credential_store_lookup_100domains(c: &mut Criterion) {
    let store = create_populated_store(100);
    let domains: Vec<String> = (0..100)
        .map(|i| format!("domain{}.example.com", i))
        .collect();
    c.bench_function("credential_store_lookup_100domains", |b| {
        b.iter(|| {
            let mut count = 0usize;
            for d in &domains {
                if store.get(black_box(d)).is_some() {
                    count += 1;
                }
            }
            black_box(count)
        });
    });
}

fn bench_www_authenticate_parse(c: &mut Criterion) {
    let header = r#"Digest realm="testrealm@host.com", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth", algorithm=MD5, opaque="5ccc069c403ebaf9f0171e9517f40e41""#;
    c.bench_function("www_authenticate_parse_complex", |b| {
        b.iter(|| {
            let challenge = parse_www_authenticate(black_box(header));
            black_box(challenge.is_ok())
        });
    });
}

// ====== LPD Benchmarks (3) ======

fn bench_lpd_announce_serialize_50(c: &mut Criterion) {
    // Benchmark: format 50 LPD announcement text messages (Hash/Port/Token)
    let info_hashes: Vec<String> = (0..50)
        .map(|i| {
            let hash = make_test_hash(i as u8);
            hash.iter().map(|b| format!("{:02x}", b)).collect()
        })
        .collect();

    c.bench_function("lpd_announce_serialize_50", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for hash in &info_hashes {
                let msg = format!(
                    "Hash: {}\nPort: {}\nToken: {:08x}\n",
                    black_box(hash),
                    black_box(6881u16),
                    black_box(0xDEAD_BEEF_u32),
                );
                total += msg.len();
            }
            black_box(total)
        });
    });
}

fn bench_lpd_announce_deserialize_50(c: &mut Criterion) {
    // Benchmark: parse 50 LPD announcement text messages
    let serialized: Vec<Vec<u8>> = (0..50)
        .map(|i| {
            let hash = make_test_hash(i as u8);
            let hash_hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
            format!(
                "Hash: {}\nPort: {}\nToken: {:08x}\n",
                hash_hex,
                6881 + i as u16,
                0xDEAD_BEEF_u32,
            )
            .into_bytes()
        })
        .collect();

    c.bench_function("lpd_announce_deserialize_50", |b| {
        b.iter(|| {
            let mut count = 0usize;
            for data in &serialized {
                if parse_lpd_announcement(black_box(data), std::net::Ipv4Addr::UNSPECIFIED.into())
                    .is_some()
                {
                    count += 1;
                }
            }
            black_box(count)
        });
    });
}

fn bench_lpd_manager_handle_packet(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let manager = LpdManager::new();

    // Pre-register a download so packets match (must be inside tokio runtime context)
    let test_hash_hex: String = make_test_hash(0x01)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    rt.block_on(async {
        manager
            .register_torrent(&test_hash_hex, false)
            .await
            .unwrap();
        // Give time for async registration
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    });

    // Build a raw LPD announcement packet (text format per BEP-14)
    let packet = {
        let to_hash_hex: String = make_test_hash(0x02)
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        format!(
            "Hash: {}\nPort: {}\nToken: {:08x}\n",
            to_hash_hex, 6881, 0xCAFE_u32,
        )
        .into_bytes()
    };

    c.bench_function("lpd_manager_handle_packet", |b| {
        b.to_async(&rt).iter(|| async {
            let _peer = parse_lpd_announcement(
                black_box(&packet),
                std::net::Ipv4Addr::new(192, 168, 1, 100).into(),
            );
        });
    });
}

// ====== MSE Benchmarks (3) ======

fn bench_mse_handshake_full(c: &mut Criterion) {
    let info_hash = make_test_hash(0xAA);
    c.bench_function("mse_handshake_full", |b| {
        b.iter(|| {
            let mut initiator = MseHandshake::new_initiator(black_box(info_hash));
            let mut responder = MseHandshake::new_responder(black_box(info_hash));
            let initiator_step1 = initiator.build_step1();
            let responder_step1 = responder.build_step1();
            let _ = initiator.receive_step1(&responder_step1);
            let _ = responder.receive_step1(&initiator_step1);
            let initiator_step2 = initiator
                .build_initiator_step2()
                .expect("build initiator step2");
            let _ = responder.receive_initiator_step2(&initiator_step2, &[info_hash]);
            let responder_step2 = responder
                .build_receiver_step2()
                .expect("build responder step2");
            let _ = initiator.receive_receiver_step2(&responder_step2);
            let initiator_state = initiator.finalize().expect("finalize initiator");
            let responder_state = responder.finalize().expect("finalize responder");
            black_box((
                initiator_state.is_encrypted(),
                responder_state.is_encrypted(),
            ));
        });
    });
}

fn bench_rc4_encrypt_1mb(c: &mut Criterion) {
    let key = [0x42u8; 20];
    let mut cipher = Arc4Cipher::new(&key);
    let data = vec![0x42u8; 1024 * 1024];
    c.bench_function("rc4_encrypt_1mb", |b| {
        b.iter(|| {
            let mut block = black_box(data.clone());
            cipher.encrypt(&mut block);
            black_box(block.len())
        });
    });
}

fn bench_mse_dh_key_exchange(c: &mut Criterion) {
    c.bench_function("mse_dh_key_exchange_single", |b| {
        b.iter(|| {
            let initiator = MseDhKeyExchange::new();
            let responder = MseDhKeyExchange::new();
            let initiator_public = initiator.generate_public_key();
            let responder_public = responder.generate_public_key();
            let initiator_secret = initiator.compute_shared_secret(&responder_public);
            let responder_secret = responder.compute_shared_secret(&initiator_public);
            black_box((initiator_secret, responder_secret));
        });
    });
}
// ====== Stream Decoder Benchmarks (3) ======

fn bench_gzip_decode_1mb(c: &mut Criterion) {
    let original = vec![0x41u8; 1024 * 1024];
    let compressed = compress_gzip(&original);

    c.bench_with_input(
        BenchmarkId::new("gzip_decode_1mb", compressed.len()),
        &compressed,
        |b, data| {
            b.iter(|| {
                let mut decoder = GZipDecoder::new();
                let result = decoder.filter(black_box(data));
                black_box(result.map(|d| d.len()))
            });
        },
    );
}

fn bench_chunked_decode_100chunks_8kb(c: &mut Criterion) {
    let chunked_data = build_chunked_data(100 * 8 * 1024, 8 * 1024);

    c.bench_with_input(
        BenchmarkId::new("chunked_decode_100chunks_8kb", chunked_data.len()),
        &chunked_data,
        |b, data| {
            b.iter(|| {
                let mut decoder = ChunkedDecoder::new();
                let result = decoder.filter(black_box(data));
                black_box(result.map(|d| d.len()))
            });
        },
    );
}

fn bench_filter_chain_gzip_then_chunked_512kb(c: &mut Criterion) {
    let original = vec![0x55u8; 512 * 1024];
    let gzip_compressed = compress_gzip(&original);
    let chunked_wrapped = build_chunked_data(gzip_compressed.len(), 4096);

    c.bench_function("filter_chain_gzip_then_chunked_512kb", |b| {
        b.iter(|| {
            let mut filters: Vec<Box<dyn StreamFilter>> = Vec::new();
            filters.push(Box::new(ChunkedDecoder::new()));
            filters.push(Box::new(GZipDecoder::new()));

            let result = process_filters(&mut filters, black_box(&chunked_wrapped));
            black_box(result.map(|d| d.len()))
        });
    });
}

// ====== BT Progress Persistence Benchmark (1) ======

fn bench_progress_save_load_1000pieces(c: &mut Criterion) {
    let tmp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let manager = BtProgressManager::new(tmp_dir.path()).expect("Failed to create manager");
    let progress = create_large_progress(1000);
    let info_hash = progress.info_hash;

    c.bench_function("progress_save_load_1000pieces", |b| {
        b.iter(|| {
            let save_result =
                black_box(manager.save_progress(black_box(&info_hash), black_box(&progress)));
            let load_result = black_box(manager.load_progress(black_box(&info_hash)));
            black_box(save_result.is_ok() && load_result.is_ok())
        });
    });
}

// ====== Registration ======
criterion_group!(
    p2_benches,
    bench_digest_md5_build_header,
    bench_digest_sha256_build_header,
    bench_credential_store_lookup_100domains,
    bench_www_authenticate_parse,
    bench_lpd_announce_serialize_50,
    bench_lpd_announce_deserialize_50,
    bench_lpd_manager_handle_packet,
    bench_mse_handshake_full,
    bench_rc4_encrypt_1mb,
    bench_mse_dh_key_exchange,
    bench_gzip_decode_1mb,
    bench_chunked_decode_100chunks_8kb,
    bench_filter_chain_gzip_then_chunked_512kb,
    bench_progress_save_load_1000pieces,
);

criterion_main!(p2_benches);

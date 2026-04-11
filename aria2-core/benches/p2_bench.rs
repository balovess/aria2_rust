//! P2 新模块性能基准测试
//!
//! 覆盖: 认证系统 / LPD 发现 / MSE 加密 / 流式解码器 / BT 进度持久化

use aria2_core::auth::credential_store::CredentialStore;
use aria2_core::auth::digest_auth::{
    AuthChallenge, DigestAlgorithm, DigestAuthProvider, parse_www_authenticate,
};
use aria2_core::engine::bt_mse_handshake::{CryptoMethod, MseCryptoContext, MseHandshakeManager};
use aria2_core::engine::bt_progress_info_file::{BtProgress, BtProgressManager, DownloadStats};
use aria2_core::engine::lpd_manager::{LpdAnnounce, LpdManager};
use aria2_core::http::stream_filter::{ChunkedDecoder, FilterChain, GZipDecoder, StreamFilter};
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write as _;
use std::net::Ipv4Addr;
use std::net::SocketAddrV4;

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
    let bitfield_len = ((num_pieces + 7) / 8) as usize;
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
        save_time: std::time::SystemTime::now(),
        version: 1,
    }
}

// ====== 认证系统 Benchmarks (4个) ======

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

// ====== LPD Benchmarks (3个) ======

fn bench_lpd_announce_serialize_50(c: &mut Criterion) {
    let announces: Vec<LpdAnnounce> = (0..50)
        .map(|i| LpdAnnounce {
            from_hash: make_test_hash(i as u8),
            to_hash: make_test_hash((i + 1) as u8),
            port: 6881 + i as u16,
        })
        .collect();

    c.bench_function("lpd_announce_serialize_50", |b| {
        b.iter(|| {
            let mut total = 0usize;
            for a in &announces {
                let bytes = black_box(a.to_bytes());
                total += bytes.len();
            }
            black_box(total)
        });
    });
}

fn bench_lpd_announce_deserialize_50(c: &mut Criterion) {
    let serialized: Vec<Vec<u8>> = (0..50)
        .map(|i| {
            let ann = LpdAnnounce {
                from_hash: make_test_hash(i as u8),
                to_hash: make_test_hash((i + 1) as u8),
                port: 6881 + i as u16,
            };
            ann.to_bytes()
        })
        .collect();

    c.bench_function("lpd_announce_deserialize_50", |b| {
        b.iter(|| {
            let mut count = 0usize;
            for data in &serialized {
                if LpdAnnounce::from_bytes(black_box(data)).is_some() {
                    count += 1;
                }
            }
            black_box(count)
        });
    });
}

fn bench_lpd_manager_handle_packet(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let manager = LpdManager::new(true, 6881);

    // Pre-register a download so packets match (must be inside tokio runtime context)
    let test_hash = make_test_hash(0x01);
    rt.block_on(async {
        manager.register_download(test_hash);
        // Give time for async registration
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    });

    let packet = {
        let ann = LpdAnnounce {
            from_hash: make_test_hash(0x02),
            to_hash: test_hash,
            port: 6881,
        };
        ann.to_bytes()
    };

    let src_addr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 100), 6881);

    c.bench_function("lpd_manager_handle_packet", |b| {
        b.to_async(&rt).iter(|| async {
            let mgr = &manager;
            mgr.handle_incoming_packet(black_box(&packet), black_box(src_addr))
                .await;
        });
    });
}

// ====== MSE 加密 Benchmarks (3个) ======

fn bench_mse_handshake_full_3phase(c: &mut Criterion) {
    let info_hash = make_test_hash(0xAA);

    c.bench_function("mse_handshake_full_3phase", |b| {
        b.iter(|| {
            // Phase 1: Method selection
            let mut mgr_a =
                MseHandshakeManager::new(black_box(info_hash), CryptoMethod::Rc4).unwrap();
            let mut mgr_b =
                MseHandshakeManager::new(black_box(info_hash), CryptoMethod::Rc4).unwrap();

            let method_sel_a = black_box(mgr_a.build_method_selection());
            let method_sel_b = black_box(mgr_b.build_method_selection());

            // Parse remote method selection
            let _ = MseHandshakeManager::parse_remote_method_selection(&method_sel_b);
            let _ = MseHandshakeManager::parse_remote_method_selection(&method_sel_a);

            // Phase 2: Key exchange
            let payload_a = black_box(
                mgr_a
                    .build_key_exchange_payload(&[CryptoMethod::Rc4])
                    .unwrap(),
            );
            let payload_b = black_box(
                mgr_b
                    .build_key_exchange_payload(&[CryptoMethod::Rc4])
                    .unwrap(),
            );

            let _ = mgr_a.process_remote_key_exchange(&payload_b);
            let _ = mgr_b.process_remote_key_exchange(&payload_a);

            // Phase 3: Verification
            let verify_a = black_box(mgr_a.build_verification_payload(CryptoMethod::Rc4).unwrap());
            let verify_b = black_box(mgr_b.build_verification_payload(CryptoMethod::Rc4).unwrap());

            let ctx_a = mgr_b.process_remote_verification(&verify_a);
            let ctx_b = mgr_a.process_remote_verification(&verify_b);

            black_box(ctx_a.is_ok() && ctx_b.is_ok())
        });
    });
}

fn bench_rc4_encrypt_1mb(c: &mut Criterion) {
    let mut ctx =
        MseCryptoContext::new(b"0123456789abcdef", b"fedcba9876543210", CryptoMethod::Rc4);
    let data_1mb = vec![0x42u8; 1024 * 1024];

    c.bench_function("rc4_encrypt_1mb", |b| {
        b.iter(|| {
            let encrypted = black_box(ctx.encrypt(black_box(&data_1mb)));
            black_box(encrypted.map(|d| d.len()))
        });
    });
}

fn bench_x25519_key_exchange(c: &mut Criterion) {
    c.bench_function("x25519_key_exchange_single", |b| {
        b.iter(|| {
            let hash = black_box(make_test_hash(0xBB));
            let mut mgr = MseHandshakeManager::new(hash, CryptoMethod::Rc4).unwrap();
            let mut peer_mgr = MseHandshakeManager::new(hash, CryptoMethod::Rc4).unwrap();

            let payload = mgr
                .build_key_exchange_payload(&[CryptoMethod::Rc4])
                .unwrap();
            let peer_payload = peer_mgr
                .build_key_exchange_payload(&[CryptoMethod::Rc4])
                .unwrap();

            let r1 = mgr.process_remote_key_exchange(&peer_payload);
            let r2 = peer_mgr.process_remote_key_exchange(&payload);

            black_box(r1.is_ok() && r2.is_ok())
        });
    });
}

// ====== 流式解码器 Benchmarks (3个) ======

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
            let mut chain = FilterChain::new();
            chain.push(Box::new(ChunkedDecoder::new()));
            chain.push(Box::new(GZipDecoder::new()));

            let result = chain.process(black_box(&chunked_wrapped));
            black_box(result.map(|d| d.len()))
        });
    });
}

// ====== BT 进度持久化 Benchmark (1个) ======

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
    bench_mse_handshake_full_3phase,
    bench_rc4_encrypt_1mb,
    bench_x25519_key_exchange,
    bench_gzip_decode_1mb,
    bench_chunked_decode_100chunks_8kb,
    bench_filter_chain_gzip_then_chunked_512kb,
    bench_progress_save_load_1000pieces,
);

criterion_main!(p2_benches);

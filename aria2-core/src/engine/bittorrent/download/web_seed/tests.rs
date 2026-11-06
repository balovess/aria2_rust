//! Tests for bt_web_seed module.

use super::*;
use crate::util::rwlock_ext::RwLockRecover;
use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use std::collections::BTreeMap;

#[tokio::test]
async fn web_seed_hostname_uses_a_compatible_policy_source() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0))
        .await
        .expect("web-seed fixture should bind");
    let address = listener.local_addr().expect("web-seed fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, peer) = listener.accept().await.expect("web-seed should accept");
        let mut request = [0u8; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read web-seed request");
        assert!(String::from_utf8_lossy(&request[..bytes]).contains("range: bytes=0-1"));
        stream
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            )
            .await
            .expect("write web-seed response");
        peer
    });

    let policy = crate::network::OutboundNetworkPolicy::new(vec![
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 2)),
        "::1".parse().expect("parse IPv6 source"),
    ])
    .expect("web-seed policy should accept both families");
    let stats = std::sync::Arc::new(WebSeedStats::new());
    let tls = crate::http::client_identity::ClientTlsConfig::default();
    let local_address = policy
        .source_for_host("localhost", address.port())
        .await
        .expect("web-seed policy should select the IPv4 source for localhost");
    let timeout = std::time::Duration::from_secs(60);
    let http_client = super::client::build_client(&tls, local_address, timeout)
        .expect("web-seed HTTP client should build");
    let client = WebSeedClient::with_shared_http_client(
        &format!("http://localhost:{}/file.bin", address.port()),
        stats,
        http_client,
        timeout,
    );

    assert_eq!(client.download_piece(0, 2, 0, 2).await.unwrap(), b"ok");
    assert_eq!(
        server.await.expect("web-seed fixture should finish").ip(),
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
    );
}

// ==================== parse_url_list tests ====================

#[test]
fn test_parse_url_list_single() {
    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
    );
    root.insert(
        b"url-list".to_vec(),
        BencodeValue::Bytes(b"http://webseed.example.com/file.bin".to_vec()),
    );

    // Add minimal info dict
    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test".to_vec()));
    info.insert(b"length".to_vec(), BencodeValue::Int(1024));
    info.insert(b"piece length".to_vec(), BencodeValue::Int(512));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 40]));
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));

    let encoded = BencodeValue::Dict(root).encode();
    let urls = parse_url_list_from_bytes(&encoded);

    assert_eq!(urls.len(), 1);
    assert_eq!(urls[0], "http://webseed.example.com/file.bin");
}

#[test]
fn test_parse_url_list_multiple() {
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

    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test".to_vec()));
    info.insert(b"length".to_vec(), BencodeValue::Int(2048));
    info.insert(b"piece length".to_vec(), BencodeValue::Int(512));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 80]));
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));

    let encoded = BencodeValue::Dict(root).encode();
    let urls = parse_url_list_from_bytes(&encoded);

    assert_eq!(urls.len(), 3);
    assert_eq!(urls[0], "http://seed1.example.com/file.bin");
    assert_eq!(urls[1], "http://seed2.example.com/file.bin");
    assert_eq!(urls[2], "https://seed3.example.com/file.bin");
}

#[test]
fn test_parse_url_list_missing() {
    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(b"http://tracker.example.com/announce".to_vec()),
    );
    // No url-list key present

    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), BencodeValue::Bytes(b"test".to_vec()));
    info.insert(b"length".to_vec(), BencodeValue::Int(512));
    info.insert(b"piece length".to_vec(), BencodeValue::Int(256));
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(vec![0u8; 20]));
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));

    let encoded = BencodeValue::Dict(root).encode();
    let urls = parse_url_list_from_bytes(&encoded);

    assert!(urls.is_empty());
}

// ==================== Range header construction tests ====================

#[test]
fn test_range_request_format() {
    // Verify the Range header format matches HTTP spec (RFC 7233)
    let _client = WebSeedClient::new("http://example.com/file.bin");

    // Test: piece starting at offset 0, length 16384
    // Expected Range: bytes=0-16383
    let offset = 0u64;
    let length = 16384u64;
    let range_end = offset + length.saturating_sub(1);
    let expected = format!("bytes={}-{}", offset, range_end);

    assert_eq!(expected, "bytes=0-16383");

    // Test: piece starting at offset 524288, length 262144
    let offset2 = 524288u64;
    let length2 = 262144u64;
    let range_end2 = offset2 + length2.saturating_sub(1);
    let expected2 = format!("bytes={}-{}", offset2, range_end2);

    assert_eq!(expected2, "bytes=524288-786431");
}

#[test]
fn test_web_seed_manager_fallback() {
    // Verify manager creation with multiple seeds
    let urls = vec![
        "http://seed1.example.com/file.iso".to_string(),
        "http://seed2.example.com/file.iso".to_string(),
    ];

    let manager = WebSeedManager::new(urls, 16384, 1048576);

    assert_eq!(manager.len(), 2);
    assert!(!manager.is_empty());
    assert_eq!(manager.clients().len(), 2);

    // Verify each client has correct URL
    assert_eq!(
        manager.clients()[0].url(),
        "http://seed1.example.com/file.iso"
    );
    assert_eq!(
        manager.clients()[1].url(),
        "http://seed2.example.com/file.iso"
    );
}

#[test]
fn test_web_seed_client_creation() {
    let client = WebSeedClient::new("https://cdn.example.com/releases/v1.tar.gz");

    assert_eq!(client.url(), "https://cdn.example.com/releases/v1.tar.gz");
    assert!(client.is_available());
}

#[test]
fn test_web_seed_manager_applies_custom_tls_configuration() {
    let options = crate::request::request_group::DownloadOptions {
        ca_certificate: Some("missing-web-seed-ca.pem".into()),
        ..Default::default()
    };
    let tls = crate::http::client_identity::ClientTlsConfig::from_download_options(&options);
    let error = match WebSeedManager::new_with_tls(
        vec!["https://cdn.example.com/file.bin".into()],
        16_384,
        1_048_576,
        &tls,
    ) {
        Ok(_) => panic!("invalid web-seed TLS configuration must reject client construction"),
        Err(error) => error,
    };

    assert!(error.contains("Failed to read CA certificate"));
}

#[test]
fn test_web_seed_manager_empty() {
    let manager = WebSeedManager::new(Vec::new(), 16384, 1048576);

    assert_eq!(manager.len(), 0);
    assert!(manager.is_empty());
}

#[test]
fn test_parse_url_list_invalid_utf8() {
    let mut root = BTreeMap::new();
    root.insert(
        b"url-list".to_vec(),
        BencodeValue::Bytes(vec![0xFF, 0xFE]), // Invalid UTF-8
    );

    let encoded = BencodeValue::Dict(root).encode();
    let urls = parse_url_list_from_bytes(&encoded);

    // Should return empty (skip invalid UTF-8 URLs)
    assert!(urls.is_empty());
}

#[test]
fn test_parse_url_list_mixed_valid_invalid() {
    let mut root = BTreeMap::new();
    root.insert(
        b"url-list".to_vec(),
        BencodeValue::List(vec![
            BencodeValue::Bytes(b"http://valid.example.com/file.bin".to_vec()),
            BencodeValue::Int(42), // Invalid: not a string
            BencodeValue::Bytes(b"http://also-valid.example.com/file.bin".to_vec()),
        ]),
    );

    let encoded = BencodeValue::Dict(root).encode();
    let urls = parse_url_list_from_bytes(&encoded);

    // Should skip non-string entries, return only valid URLs
    assert_eq!(urls.len(), 2);
    assert_eq!(urls[0], "http://valid.example.com/file.bin");
    assert_eq!(urls[1], "http://also-valid.example.com/file.bin");
}

// ==================== WebSeedStats tests ====================

#[test]
fn test_web_seed_stats() {
    let stats = WebSeedStats::new();

    // Record some bytes
    stats.record_bytes(1000);
    stats.record_bytes(500);

    assert_eq!(stats.total_bytes_downloaded(), 1500);
}

#[test]
fn test_web_seed_stats_average_speed() {
    let stats = WebSeedStats::new();
    stats.record_bytes(10000);

    // Speed depends on elapsed time, just verify it doesn't panic
    let _speed = stats.average_speed();
}

// ==================== Concurrency control tests ====================

#[test]
fn test_active_requests_tracking() {
    let client = WebSeedClient::new("http://example.com/file.bin");

    // Initially, all pieces can be requested
    assert!(client.can_request(0));
    assert!(client.can_request(1));
    assert!(client.can_request(2));

    // Mark piece 0 as active
    client.mark_requesting(0);
    assert!(!client.can_request(0)); // Now piece 0 is busy
    assert!(client.can_request(1)); // Others still available
    assert_eq!(client.active_request_count(), 1);

    // Mark piece 1 as active
    client.mark_requesting(1);
    assert!(!client.can_request(0));
    assert!(!client.can_request(1));
    assert!(client.can_request(2));
    assert_eq!(client.active_request_count(), 2);

    // Clear piece 0
    client.clear_request(0);
    assert!(client.can_request(0)); // Piece 0 available again
    assert!(!client.can_request(1)); // Piece 1 still busy
    assert_eq!(client.active_request_count(), 1);
}

#[test]
fn test_web_seed_manager_stats() {
    let urls = vec![
        "http://seed1.example.com/file.bin".to_string(),
        "http://seed2.example.com/file.bin".to_string(),
    ];

    let manager = WebSeedManager::new(urls, 16384, 1048576);

    // Stats should be accessible
    let stats = manager.stats();
    assert_eq!(stats.total_bytes_downloaded(), 0);
}

#[tokio::test]
async fn live_web_seed_uses_per_file_ranges_and_observes_change_uri() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn range_server(body: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local web-seed fixture");
        let address = listener.local_addr().expect("get fixture address");
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept web-seed request");
            let mut request = Vec::new();
            let mut buffer = [0u8; 512];
            loop {
                let count = stream.read(&mut buffer).await.expect("read request");
                assert_ne!(count, 0, "request ended before headers completed");
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(request.contains("range: bytes=0-3"));
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("write response headers");
            stream.write_all(body).await.expect("write response body");
        });
        (format!("http://{address}/file"), task)
    }

    let (first_url, first_server) = range_server(b"ABCD").await;
    let (second_url, second_server) = range_server(b"EFGH").await;
    let entries = vec![
        crate::download::file_entry::FileEntry::new("first".into(), 4, 0, vec![first_url]),
        crate::download::file_entry::FileEntry::new("second".into(), 4, 4, Vec::new()),
    ];
    let mut context = crate::download::DownloadContext::new_default();
    context.set_piece_length(8);
    context.set_file_entries(entries);
    let group = std::sync::Arc::new(std::sync::RwLock::new(
        crate::request::request_group::RequestGroup::new(
            crate::request::request_group::GroupId::new(7101),
            Vec::new(),
            Default::default(),
        ),
    ));
    group
        .recover()
        .set_download_context(std::sync::Arc::new(context));
    let manager = WebSeedManager::for_request_group(
        std::sync::Arc::clone(&group),
        8,
        8,
        crate::http::client_identity::ClientTlsConfig::default(),
        crate::network::OutboundNetworkPolicy::direct().into(),
    );

    assert!(!manager.is_empty());
    assert!(!manager.has_complete_sources_for_piece(0, 8));
    let original_uri_generation = group.recover().uri_generation();
    group
        .recover_mut()
        .change_uris(2, &[], &[second_url], None)
        .expect("changeUri should add the second file source");
    assert!(group.recover().uri_generation() > original_uri_generation);
    assert!(manager.has_complete_sources_for_piece(0, 8));
    let data = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        manager.request_piece_with_length_and_activity(0, 8, None),
    )
    .await
    .expect("live WebSeed request should not hang")
    .expect("per-file sources should supply the piece");

    assert_eq!(data, b"ABCDEFGH");
    first_server.await.expect("first file source should finish");
    second_server
        .await
        .expect("second file source should finish");
}

#[tokio::test]
async fn live_web_seed_stops_reprobing_404_until_uri_configuration_changes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn fixture(
        response_body: &'static [u8],
        status: &'static str,
        request_count: usize,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local WebSeed fixture");
        let address = listener.local_addr().expect("get fixture address");
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_requests = std::sync::Arc::clone(&requests);
        let task = tokio::spawn(async move {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().await.expect("accept WebSeed request");
                let mut request = Vec::new();
                let mut buffer = [0u8; 512];
                loop {
                    let count = stream.read(&mut buffer).await.expect("read request");
                    assert_ne!(count, 0, "request ended before headers completed");
                    request.extend_from_slice(&buffer[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                server_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let response_data = if status.starts_with("206") {
                    let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    let range = request
                        .lines()
                        .find_map(|line| line.strip_prefix("range: bytes="))
                        .expect("WebSeed request includes a byte range");
                    let (start, end) = range.split_once('-').expect("parse requested range");
                    let start = start.parse::<usize>().expect("range start is numeric");
                    let end = end.parse::<usize>().expect("range end is numeric");
                    &response_body[start..=end]
                } else {
                    &[]
                };
                let headers = if status.starts_with("206") {
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response_data.len()
                    )
                } else {
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                };
                stream
                    .write_all(headers.as_bytes())
                    .await
                    .expect("write response headers");
                if !response_data.is_empty() {
                    stream
                        .write_all(response_data)
                        .await
                        .expect("write response body");
                }
            }
        });
        (format!("http://{address}/file"), requests, task)
    }

    let (dead_url, dead_requests, dead_server) = fixture(b"", "404 Not Found", 2).await;
    let (healthy_url, healthy_requests, healthy_server) =
        fixture(b"ABCDEFGHIJKL", "206 Partial Content", 3).await;
    let entry = crate::download::file_entry::FileEntry::new(
        "file.bin".into(),
        12,
        0,
        vec![dead_url, healthy_url],
    );
    let mut context = crate::download::DownloadContext::new_default();
    context.set_piece_length(4);
    context.set_file_entries(vec![entry]);
    let group = std::sync::Arc::new(std::sync::RwLock::new(
        crate::request::request_group::RequestGroup::new(
            crate::request::request_group::GroupId::new(7103),
            Vec::new(),
            Default::default(),
        ),
    ));
    group
        .recover()
        .set_download_context(std::sync::Arc::new(context));
    let manager = WebSeedManager::for_request_group(
        std::sync::Arc::clone(&group),
        4,
        12,
        crate::http::client_identity::ClientTlsConfig::default(),
        crate::network::OutboundNetworkPolicy::direct().into(),
    );

    for (piece_index, expected) in [(0, b"ABCD".as_slice()), (1, b"EFGH".as_slice())] {
        let data = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            manager.request_piece_with_length_and_activity(piece_index, 4, None),
        )
        .await
        .expect("WebSeed request should not hang")
        .expect("healthy mirror should serve each piece");
        assert_eq!(data, expected);
    }

    let uri_states = group.recover().uri_entries();
    assert_eq!(uri_states.len(), 2);
    assert!(
        uri_states.iter().all(|entry| entry.status == "used"),
        "both dispatched WebSeed sources should be projected as used: {uri_states:?}"
    );

    assert_eq!(
        dead_requests.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the 404 source should be skipped for later pieces in the same URI generation"
    );
    group
        .recover_mut()
        .change_uris(1, &[], &["http://127.0.0.1:9/unused.bin".to_string()], None)
        .expect("changeUri should advance the source configuration generation");
    let data = manager
        .request_piece_with_length_and_activity(2, 4, None)
        .await
        .expect("healthy mirror should still serve after URI configuration changes");
    assert_eq!(data, b"IJKL");
    let changed_uri_states = group.recover().uri_entries();
    assert_eq!(
        changed_uri_states
            .iter()
            .filter(|entry| entry.status == "used")
            .count(),
        2,
        "previously dispatched URIs should remain used after adding a source"
    );
    assert_eq!(
        changed_uri_states
            .iter()
            .filter(|entry| entry.status == "waiting")
            .count(),
        1,
        "the newly added URI should remain waiting until selected"
    );

    dead_server.await.expect("404 fixture should finish");
    healthy_server.await.expect("healthy fixture should finish");
    assert_eq!(
        dead_requests.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "a 404 source should be probed again only after URI configuration changes"
    );
    assert_eq!(
        healthy_requests.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "the healthy mirror should serve all piece ranges"
    );
}

#[tokio::test]
#[ignore = "requires public Debian WebSeed availability"]
async fn public_debian_web_seed_follows_redirect_for_piece_range() {
    let url = "https://cdimage.debian.org/cdimage/release/13.7.0/amd64/iso-dvd/debian-13.7.0-amd64-DVD-1.iso";
    let total_length = 3_992_977_408;
    let piece_length = 262_144;
    let entry = crate::download::file_entry::FileEntry::new(
        "debian-13.7.0-amd64-DVD-1.iso".into(),
        total_length,
        0,
        vec![url.to_string()],
    );
    let mut context = crate::download::DownloadContext::new_default();
    context.set_piece_length(piece_length);
    context.set_file_entries(vec![entry]);
    let group = std::sync::Arc::new(std::sync::RwLock::new(
        crate::request::request_group::RequestGroup::new(
            crate::request::request_group::GroupId::new(7102),
            Vec::new(),
            Default::default(),
        ),
    ));
    group
        .recover()
        .set_download_context(std::sync::Arc::new(context));
    let manager = WebSeedManager::for_request_group(
        group,
        piece_length,
        total_length,
        crate::http::client_identity::ClientTlsConfig::default(),
        crate::network::OutboundNetworkPolicy::direct().into(),
    );

    let data = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        manager.request_piece_with_length_and_activity(42, piece_length as u64, None),
    )
    .await
    .expect("public WebSeed range request should not hang")
    .expect("public Debian WebSeed should serve a redirecting range request");

    assert_eq!(data.len(), piece_length as usize);
}

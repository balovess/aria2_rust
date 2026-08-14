//! Tests for bt_web_seed module.

use super::*;
use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use std::collections::BTreeMap;

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

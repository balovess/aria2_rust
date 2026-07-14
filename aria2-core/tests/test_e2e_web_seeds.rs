//! E2E tests for BitTorrent Web Seeds (BEP 19)
//!
//! Tests that aria2-rust can download torrent pieces from HTTP web seeds
//! when peer swarm is unavailable or insufficient.

mod e2e_helpers;

use e2e_helpers::mock_http_server::MockHttpServer;

// ===========================================================================
// Test 1: WebSeedClient basic piece download
// ===========================================================================

#[tokio::test]
async fn test_web_seed_client_basic() {
    use aria2_core::engine::bt_web_seed::WebSeedClient;

    // Create test data (4 pieces of 16KB each)
    let piece_length = 16384u64;
    let num_pieces = 4;
    let total_size = piece_length * num_pieces;
    let data: Vec<u8> = (0..total_size as usize).map(|i| (i % 256) as u8).collect();

    // Start mock server with Range support
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    server.register_range_response("/file.bin", &data);

    let base_url = server.base_url();

    // Create web seed client (ensure proper URL path separator)
    let url = format!("{}/file.bin", base_url);
    let client = WebSeedClient::new(&url);

    // Download piece 0
    let piece_data = client
        .download_piece(0, piece_length, 0, piece_length)
        .await
        .expect("Failed to download piece 0");

    assert_eq!(piece_data.len(), piece_length as usize);
    assert_eq!(piece_data, &data[0..piece_length as usize]);
}

// ===========================================================================
// Test 2: WebSeedManager multi-URL fallback
// ===========================================================================

#[tokio::test]
async fn test_web_seed_manager_fallback() {
    use aria2_core::engine::bt_web_seed::WebSeedManager;

    // Create test data
    let piece_length = 16384u32;
    let total_size = piece_length as u64 * 4;
    let data: Vec<u8> = (0..total_size as usize).map(|i| (i % 256) as u8).collect();

    // Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    server.register_range_response("/file.bin", &data);

    let base_url = server.base_url();

    // Create manager with single URL (ensure proper URL path separator)
    let manager = WebSeedManager::new(
        vec![format!("{}/file.bin", base_url)],
        piece_length,
        total_size,
    );

    // Request piece 2
    let piece_data = manager
        .request_piece(2)
        .await
        .expect("Failed to download piece 2");

    let expected_offset = 2 * piece_length as u64;
    let expected_end = expected_offset + piece_length as u64;
    assert_eq!(piece_data.len(), piece_length as usize);
    assert_eq!(
        piece_data,
        &data[expected_offset as usize..expected_end as usize]
    );
}

// ===========================================================================
// Test 3: WebSeedClient handles last piece (smaller than piece_length)
// ===========================================================================

#[tokio::test]
async fn test_web_seed_last_piece() {
    use aria2_core::engine::bt_web_seed::WebSeedClient;

    // Create data where last piece is smaller
    let piece_length = 16384u64;
    let total_size = piece_length * 3 + 8192; // 3 full pieces + 1 half piece
    let data: Vec<u8> = (0..total_size as usize).map(|i| (i % 256) as u8).collect();

    // Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    server.register_range_response("/file.bin", &data);

    let base_url = server.base_url();

    // Create web seed client (ensure proper URL path separator)
    let url = format!("{}/file.bin", base_url);
    let client = WebSeedClient::new(&url);

    // Download last piece (piece 3)
    let last_piece_offset = piece_length * 3;
    let last_piece_len = total_size - last_piece_offset;
    let piece_data = client
        .download_piece(3, piece_length, last_piece_offset, last_piece_len)
        .await
        .expect("Failed to download last piece");

    assert_eq!(piece_data.len(), last_piece_len as usize);
    assert_eq!(
        piece_data,
        &data[last_piece_offset as usize..total_size as usize]
    );
}

// ===========================================================================
// Test 4: WebSeedStats tracking
// ===========================================================================

#[tokio::test]
async fn test_web_seed_stats() {
    use aria2_core::engine::bt_web_seed::{WebSeedClient, WebSeedStats};
    use std::sync::Arc;

    // Create test data
    let piece_length = 16384u64;
    let data: Vec<u8> = (0..piece_length as usize)
        .map(|i| (i % 256) as u8)
        .collect();

    // Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    server.register_range_response("/file.bin", &data);

    let base_url = server.base_url();

    // Create shared stats
    let stats = Arc::new(WebSeedStats::new());
    let url = format!("{}/file.bin", base_url);
    let client = WebSeedClient::with_shared_stats(&url, stats.clone());

    // Download piece
    let _piece_data = client
        .download_piece(0, piece_length, 0, piece_length)
        .await
        .expect("Failed to download piece");

    // Verify stats were updated
    let total = stats.total_bytes_downloaded();
    assert!(total > 0, "Stats should have recorded bytes downloaded");
}

// ===========================================================================
// Test 5: WebSeedManager with multiple URLs
// ===========================================================================

#[tokio::test]
async fn test_web_seed_manager_multiple_urls() {
    use aria2_core::engine::bt_web_seed::WebSeedManager;

    // Create test data
    let piece_length = 16384u32;
    let total_size = piece_length as u64 * 2;
    let data: Vec<u8> = (0..total_size as usize).map(|i| (i % 256) as u8).collect();

    // Start two mock servers
    let server1 = MockHttpServer::start()
        .await
        .expect("Failed to start server 1");
    let server2 = MockHttpServer::start()
        .await
        .expect("Failed to start server 2");
    server1.register_range_response("/file.bin", &data);
    server2.register_range_response("/file.bin", &data);

    // Create manager with multiple URLs (ensure proper URL path separator)
    let manager = WebSeedManager::new(
        vec![
            format!("{}/file.bin", server1.base_url()),
            format!("{}/file.bin", server2.base_url()),
        ],
        piece_length,
        total_size,
    );

    // Request both pieces
    let piece0 = manager
        .request_piece(0)
        .await
        .expect("Failed to download piece 0");
    let piece1 = manager
        .request_piece(1)
        .await
        .expect("Failed to download piece 1");

    assert_eq!(piece0.len(), piece_length as usize);
    assert_eq!(piece1.len(), piece_length as usize);
}

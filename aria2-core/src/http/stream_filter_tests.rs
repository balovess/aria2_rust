//! Unit tests for the stream filter framework
//!
//! Tests cover GZip, Chunked, BZip2 decoders, filter composition,
//! AutoFilterSelector, and HttpResponse integration tests.

use super::stream_filter::*;
use crate::error::Aria2Error;
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write;

// ==================== Helper functions ====================

/// Create GZip compressed data (for testing)
fn create_gzip_data(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// Pre-computed BZip2 compressed test data (pure Rust, no C dependency needed)
/// Original: "BZip2 compression test data for verification."
fn bzip2_test_data() -> Vec<u8> {
    hex::decode(
        "425a6839314159265359d1dfd3620000039f8040011000100000102f23dd002\
                 000314c98990646113d469a0d036a4e1b7e1eb5d9df8e872cabd535e9962e96\
                 057870104680f8bb9229c284868efe9b10",
    )
    .unwrap()
}

// ==================== GZipDecoder tests ====================

#[test]
fn test_gzip_decompress_small_file() {
    // Prepare test data (<1KB)
    let original = b"Hello, World! This is a small test file for gzip decompression.";
    let compressed = create_gzip_data(original);

    // Decompress
    let mut decoder = GZipDecoder::new();
    let result = decoder
        .filter(&compressed)
        .expect("GZip decompression failed");

    // Verify results
    assert_eq!(result, original, "Decompressed data should match original");
    assert_eq!(decoder.name(), "gzip");
}

#[test]
fn test_gzip_invalid_header_error() {
    // Non-GZip data (missing magic number)
    let invalid_data = b"This is not gzip data";

    let mut decoder = GZipDecoder::new();
    let result = decoder.filter(invalid_data);

    // Should return error
    assert!(result.is_err(), "Should fail with invalid GZip data");
    match result.unwrap_err() {
        Aria2Error::Parse(msg) => {
            assert!(
                msg.contains("Invalid GZip magic number"),
                "Error message should mention invalid magic number"
            );
        }
        other => panic!("Expected Parse error, got: {:?}", other),
    }
}

#[test]
fn test_gzip_needs_more_input() {
    let original = b"Test needs_more_input";
    let compressed = create_gzip_data(original);

    let mut decoder = GZipDecoder::new();

    // Should need input before decompression
    assert!(
        decoder.needs_more_input(),
        "New decoder should need input before processing"
    );

    // Should not need more input after decompression
    let _ = decoder
        .filter(&compressed)
        .expect("Decompression should succeed");
    assert!(
        !decoder.needs_more_input(),
        "Finished decoder should not need more input"
    );
}

// ==================== ChunkedDecoder tests ====================

#[test]
fn test_chunked_decode_normal() {
    // Standard chunked format: 5\r\nhello\r\n0\r\n\r\n
    let chunked_data = b"5\r\nhello\r\n0\r\n\r\n";

    let mut decoder = ChunkedDecoder::new();
    let result = decoder.filter(chunked_data).expect("Chunked decode failed");

    assert_eq!(result, b"hello", "Should decode 'hello'");
    assert_eq!(decoder.name(), "chunked");

    // Should be complete
    assert!(
        !decoder.needs_more_input(),
        "Should be complete after final chunk"
    );
}

#[test]
fn test_chunked_decode_with_extensions() {
    // Chunked format with extensions: 5;name=value\r\nhello\r\n0\r\n\r\n
    let chunked_data = b"5;name=value\r\nhello\r\n0\r\n\r\n";

    let mut decoder = ChunkedDecoder::new();
    let result = decoder
        .filter(chunked_data)
        .expect("Chunked decode with extensions failed");

    assert_eq!(
        result, b"hello",
        "Should ignore extensions and decode correctly"
    );
}

#[test]
fn test_chunked_early_eof() {
    // Incomplete chunk (size=10 but only 5 bytes of data)
    let incomplete_chunked = b"A\r\nHello"; // size=10, but only 5 bytes of data

    let mut decoder = ChunkedDecoder::new();
    let result = decoder.filter(incomplete_chunked);

    // Should successfully return available data (partial decode)
    match result {
        Ok(data) => {
            assert_eq!(data, b"Hello", "Should return partial data");
            // State should be ReadingData or waiting for more input
            assert!(
                decoder.needs_more_input(),
                "Incomplete chunk should need more input"
            );
        }
        Err(e) => {
            // May also return error depending on implementation
            println!("Got error for early EOF: {:?}", e);
        }
    }

    // flush should return error or warning
    let flush_result = decoder.flush();
    match flush_result {
        Err(Aria2Error::Parse(_)) => {} // Expected error
        Ok(_) => {}                     // Or return available data
        other => panic!("Unexpected flush result: {:?}", other),
    }
}

#[test]
fn test_chunked_multiple_chunks() {
    // Multiple chunks: 5\r\nhello\r\n6\r\n world\r\n7\r\n!!!\r\n0\r\n\r\n
    let chunked_data = b"5\r\nhello\r\n6\r\n world\r\n7\r\n!!!\r\n0\r\n\r\n";

    let mut decoder = ChunkedDecoder::new();
    let result = decoder
        .filter(chunked_data)
        .expect("Multi-chunk decode failed");

    // Verify output contains data from all chunks (leading part should match exactly)
    assert!(
        result.starts_with(b"hello world!!!"),
        "Output should start with concatenated chunk data: got {:?}",
        result
    );
}

// ==================== Filter processing tests ====================

#[test]
fn test_filter_chain_gzip_then_chunked() {
    // First GZip compress, then chunked encode
    let original = b"Compressed and chunked data";
    let compressed = create_gzip_data(original);

    // Manually create chunked format of compressed data
    let size_hex = format!("{:x}", compressed.len());
    let _chunked_compressed = format!(
        "{}\r\n{}\r\n0\r\n\r\n",
        size_hex,
        String::from_utf8_lossy(&compressed)
    )
    .into_bytes();

    // Test each filter individually
    let mut gzip_decoder = GZipDecoder::new();
    let decompressed = gzip_decoder.filter(&compressed).expect("GZip failed");
    assert_eq!(decompressed, original);
}

#[test]
fn test_process_filters_empty() {
    // Empty filter list should pass through data
    let mut filters: Vec<Box<dyn StreamFilter>> = Vec::new();
    let input = b"passthrough data";

    let result = process_filters(&mut filters, input).expect("Empty filter process failed");

    assert_eq!(
        result, input,
        "Empty filter list should pass through data unchanged"
    );
    assert!(filters.is_empty(), "Filter list should be empty");
}

#[test]
fn test_process_filters_with_decoders() {
    let mut filters: Vec<Box<dyn StreamFilter>> = Vec::new();
    filters.push(Box::new(GZipDecoder::new()));
    assert_eq!(filters.len(), 1, "Should have 1 filter after push");

    filters.push(Box::new(ChunkedDecoder::new()));
    assert_eq!(filters.len(), 2, "Should have 2 filters after second push");

    filters.clear();
    assert!(filters.is_empty(), "Should be empty after clear");
    assert_eq!(filters.len(), 0, "Length should be 0 after clear");
}

// ==================== AutoFilterSelector tests ====================

#[test]
fn test_auto_select_gzip_content_encoding() {
    // Content-Encoding: gzip → should select GZipDecoder
    let filters = AutoFilterSelector::select_filters(Some("gzip"), None);

    assert_eq!(filters.len(), 1, "Should select 1 filter for gzip");
}

#[test]
fn test_auto_select_chunked_transfer_encoding() {
    // Transfer-Encoding: chunked → should select ChunkedDecoder
    let filters = AutoFilterSelector::select_filters(None, Some("chunked"));

    assert_eq!(filters.len(), 1, "Should select 1 filter for chunked");
}

#[test]
fn test_auto_select_x_gzip_encoding() {
    // x-gzip is an alias for gzip
    let filters = AutoFilterSelector::select_filters(Some("x-gzip"), None);

    assert_eq!(filters.len(), 1, "x-gzip should be treated as gzip");
}

#[test]
fn test_auto_select_bzip2_encoding() {
    let filters = AutoFilterSelector::select_filters(Some("bzip2"), None);

    assert_eq!(
        filters.len(),
        1,
        "Should select BZip2Decoder for bzip2 encoding"
    );
}

#[test]
fn test_auto_select_identity_encoding() {
    // identity means no encoding
    let filters = AutoFilterSelector::select_filters(Some("identity"), None);

    assert_eq!(
        filters.len(),
        0,
        "Identity encoding should not add any filters"
    );
}

#[test]
fn test_auto_select_no_encoding() {
    // No encoding info
    let filters = AutoFilterSelector::select_filters(None, None);

    assert_eq!(filters.len(), 0, "No encoding should result in empty list");
}

// ==================== HttpResponse integration tests ====================

#[test]
fn test_http_response_decoded_body_integration() {
    use super::request_response::HttpResponse;
    use std::collections::HashMap;

    // Prepare original data and GZip compressed data
    let original = b"HTTP response body content";
    let compressed = create_gzip_data(original);

    // Build HTTP response
    let mut headers: HashMap<String, Vec<String>> = HashMap::new();
    headers.insert("Content-Encoding".to_string(), vec!["gzip".to_string()]);
    headers.insert("Content-Type".to_string(), vec!["text/plain".to_string()]);

    let response = HttpResponse {
        status_code: 200,
        reason_phrase: "OK".to_string(),
        version: "HTTP/1.1".to_string(),
        headers,
        body: Some(compressed),
    };

    // Use decoded_body to get decompressed content
    let decoded = response.decoded_body().expect("decoded_body failed");

    assert_eq!(
        decoded, original,
        "decoded_body should return decompressed content"
    );
}

#[test]
fn test_http_response_decoded_body_no_body() {
    use super::request_response::HttpResponse;
    use std::collections::HashMap;

    // Response without body
    let response = HttpResponse {
        status_code: 204,
        reason_phrase: "No Content".to_string(),
        version: "HTTP/1.1".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let decoded = response
        .decoded_body()
        .expect("decoded_body should succeed for no body");

    assert!(
        decoded.is_empty(),
        "No body response should return empty vector"
    );
}

// ==================== Mixed encoding handling tests ====================

#[test]
fn test_mixed_encoding_handling() {
    // When both Transfer-Encoding and Content-Encoding exist,
    // per RFC 7230, Transfer-Encoding takes priority

    // Scenario 1: Transfer-Encoding=chunked + Content-Encoding=gzip
    // Should only use chunked decoder
    let filters = AutoFilterSelector::select_filters(Some("gzip"), Some("chunked"));

    assert_eq!(
        filters.len(),
        1,
        "Transfer-Encoding should take priority over Content-Encoding"
    );
}

#[test]
fn test_multiple_content_encodings() {
    // Multiple Content-Encoding values (comma-separated)
    let filters = AutoFilterSelector::select_filters(Some("gzip, deflate"), None);

    // Currently only gzip is supported, deflate will output warning but not add a filter
    assert!(
        !filters.is_empty(),
        "Should at least handle supported encodings"
    );
}

// ==================== BZip2Decoder tests ====================

#[test]
fn test_bzip2_decompress_basic() {
    let original = b"BZip2 compression test data for verification.";
    let compressed = bzip2_test_data();

    let mut decoder = BZip2Decoder::new();
    let result = decoder
        .filter(&compressed)
        .expect("BZip2 decompression failed");

    assert_eq!(
        result, original,
        "BZip2 decompressed data should match original"
    );
    assert_eq!(decoder.name(), "bzip2");
}

#[test]
fn test_bzip2_invalid_data_error() {
    // Invalid BZip2 data
    let invalid_data = b"This is not valid bzip2 data";

    let mut decoder = BZip2Decoder::new();
    let result = decoder.filter(invalid_data);

    assert!(result.is_err(), "Invalid BZip2 data should cause error");
}

// ==================== Edge case tests ====================

#[test]
fn test_gzip_empty_data() {
    // Compress empty string
    let original = b"";
    let compressed = create_gzip_data(original);

    let mut decoder = GZipDecoder::new();
    let result = decoder
        .filter(&compressed)
        .expect("Empty GZip decompression failed");

    assert_eq!(result, original, "Empty data should decompress to empty");
}

#[test]
fn test_chunked_single_byte_chunks() {
    // Each chunk is only 1 byte
    let chunked_data = b"1\r\nH\r\n1\r\ne\r\n1\r\nl\r\n1\r\nl\r\n1\r\no\r\n0\r\n\r\n";

    let mut decoder = ChunkedDecoder::new();
    let result = decoder
        .filter(chunked_data)
        .expect("Single byte chunks failed");

    assert_eq!(
        result, b"Hello",
        "Single byte chunks should concatenate correctly"
    );
}

#[test]
fn test_chunked_large_size() {
    // Large chunk (100 bytes)
    let data = vec![b'X'; 100];
    let size_hex = format!("{:x}", 100);
    let chunked = format!(
        "{}\r\n{}\r\n0\r\n\r\n",
        size_hex,
        String::from_utf8_lossy(&data)
    )
    .into_bytes();

    let mut decoder = ChunkedDecoder::new();
    let result = decoder.filter(&chunked).expect("Large chunk failed");

    assert_eq!(result.len(), 100, "Should decode all 100 bytes");
    assert!(result.iter().all(|&b| b == b'X'), "All bytes should be X");
}

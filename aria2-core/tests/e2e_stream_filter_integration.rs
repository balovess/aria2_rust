//! E2E Integration Tests for Stream Filters
//!
//! Tests the complete stream filter pipeline including GZip decoding,
//! Chunked transfer decoding, and passthrough mode.

mod e2e_helpers;

mod tests {
    use crate::e2e_helpers::MockHttpServer;
    use crate::e2e_helpers::mock_http_server::RequestLog;
    use aria2_core::http::stream_filter::*;
    use hyper::{Body, Request, Response};

    #[tokio::test]
    async fn test_gzip_download_full_roundtrip() {
        // Start mock server
        let server: MockHttpServer = MockHttpServer::start()
            .await
            .expect("Failed to start server");

        // Original data to compress
        let original_data: &[u8] = b"This is test data for GZip roundtrip testing. It should be compressed and then decompressed correctly.";

        // Register gzip response
        server.register_gzip_response("/compressed.gz", original_data);

        // Fetch using reqwest with auto-decompression
        let client = reqwest::Client::builder().gzip(true).build().unwrap();

        let url = format!("{}/compressed.gz", server.base_url());
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);

        // Get decompressed body
        let decompressed = resp.bytes().await.unwrap().to_vec();

        // Verify roundtrip
        assert_eq!(
            decompressed, original_data,
            "Decompressed data must match original"
        );

        // Also test the GZipDecoder directly
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        // Compress data
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(original_data)
            .expect("GZip encoding failed");
        let compressed = encoder.finish().expect("GZip finish failed");

        // Decompress using our GZipDecoder
        let mut decoder = GZipDecoder::new();
        let result = decoder.filter(&compressed).expect("GZip decoding failed");
        let flush_result = decoder.flush().expect("GZip flush failed");

        let mut full_result = result;
        full_result.extend_from_slice(&flush_result);

        assert_eq!(
            full_result, original_data,
            "GZipDecoder output must match original data"
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_chunked_transfer_decode() {
        // Start mock server
        let server: MockHttpServer = MockHttpServer::start()
            .await
            .expect("Failed to start server");

        // Prepare chunks of data
        let chunk1: Vec<u8> = b"Hello, ".to_vec();
        let chunk2: Vec<u8> = b"World! ".to_vec();
        let chunk3: Vec<u8> = b"This is ".to_vec();
        let chunk4: Vec<u8> = b"chunked data.".to_vec();

        let expected_data: Vec<u8> = [
            chunk1.clone(),
            chunk2.clone(),
            chunk3.clone(),
            chunk4.clone(),
        ]
        .concat();

        // Register chunked response
        server.register_chunked_response("/chunked", vec![chunk1, chunk2, chunk3, chunk4]);

        // Fetch using reqwest (handles chunked automatically)
        let client = reqwest::Client::new();
        let url = format!("{}/chunked", server.base_url());
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);

        // Get reassembled body
        let body = resp.bytes().await.unwrap().to_vec();

        // Verify reassembly
        assert_eq!(
            body, expected_data,
            "Chunked data must be correctly reassembled"
        );

        // Also test ChunkedDecoder directly
        // Build chunked-encoded data manually
        let chunks: [&[u8]; 4] = [b"Hello, ", b"World! ", b"This is ", b"chunked data."];
        let mut chunked_input: Vec<u8> = Vec::new();
        for chunk in &chunks {
            let len = chunk.len();
            chunked_input.extend_from_slice(format!("{:x}\r\n", len).as_bytes());
            chunked_input.extend_from_slice(chunk);
            chunked_input.extend_from_slice(b"\r\n");
        }
        chunked_input.extend_from_slice(b"0\r\n\r\n");

        // Decode using our ChunkedDecoder
        let mut decoder = ChunkedDecoder::new();
        let result = decoder
            .filter(&chunked_input)
            .expect("Chunked decoding failed");

        assert_eq!(
            result,
            b"Hello, World! This is chunked data.".as_ref(),
            "ChunkedDecoder must correctly decode chunked transfer encoding"
        );

        // Verify decoder state
        assert!(
            !decoder.needs_more_input(),
            "Decoder should be complete after final chunk"
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_no_encoding_passthrough() {
        // Start mock server
        let server: MockHttpServer = MockHttpServer::start()
            .await
            .expect("Failed to start server");

        // Original plain data
        let original_data: &[u8] =
            b"This is plain text without any encoding. It should pass through unchanged.";
        let data_vec: Vec<u8> = original_data.to_vec();

        // Register plain response (no Content-Encoding)
        server.on_get("/plain", move |_req: &Request<Body>| -> Response<Body> {
            Response::builder()
                .status(200)
                .header("Content-Type", "text/plain")
                .body(Body::from(data_vec.clone()))
                .unwrap()
        });

        // Fetch the data
        let client = reqwest::Client::new();
        let url = format!("{}/plain", server.base_url());
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);

        // Verify content type header (before consuming response)
        if let Some(content_type) = resp.headers().get("content-type") {
            assert!(
                content_type.to_str().unwrap().contains("text/plain"),
                "Content-Type should be text/plain"
            );
        }

        // Verify no transformation occurred
        let body = resp.bytes().await.unwrap().to_vec();
        assert_eq!(
            body, original_data,
            "Plain data must pass through unchanged"
        );

        // Verify request was logged
        let log: Vec<RequestLog> = server.take_request_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].path, "/plain");

        server.shutdown().await;
    }
}

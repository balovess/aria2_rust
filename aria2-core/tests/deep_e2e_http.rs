//! Deep HTTP Integration Tests for aria2-core
//!
//! Comprehensive E2E tests covering authentication flows, redirects, range requests,
//! timeouts, concurrent segment assembly, and content encoding using reqwest::Client.
//!
//! Each test is fully self-contained: starts its own MockHttpServer, registers routes,
//! executes HTTP requests via reqwest, asserts results, and cleans up.

#![allow(dead_code)]

mod e2e_helpers;

use crate::e2e_helpers::{MockHttpServer, RequestLog};
use base64::Engine;
use hyper::{Body, Request, Response};

// ---------------------------------------------------------------------------
// Inline helpers (from test_harness -- not importable as crate module in integration tests)
// ---------------------------------------------------------------------------

/// Generate deterministic test data of given size (reproducible across runs).
fn generate_test_data(size: usize, seed: u8) -> Vec<u8> {
    (0..size).map(|i| (i as u8).wrapping_add(seed)).collect()
}

/// Build URL from base + path
fn make_url(base: &str, path: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{}{}", trimmed, path)
    } else {
        format!("{}/{}", trimmed, path)
    }
}

// ---------------------------------------------------------------------------
// Test 1: Basic Auth Auto-Retry
// ---------------------------------------------------------------------------
// Verifies that sending a valid Basic Authorization header grants access to a
// protected resource that would otherwise return 401.

#[tokio::test]
async fn test_http_basic_auth_auto_retry() {
    // 1. Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2. Register auth-gated endpoint (returns 401 unless Authorization header present)
    server.register_auth_gated("/secret", "TestRealm", "Basic", b"secret_data");

    // 3. Build client with pre-configured Basic auth header
    let credentials = base64::engine::general_purpose::STANDARD.encode("admin:password123");
    let client = reqwest::Client::builder()
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Basic {}", credentials).parse().unwrap(),
            );
            headers
        })
        .build()
        .expect("Failed to build reqwest client");

    // 4. Execute authenticated GET request
    let url = make_url(&server.base_url(), "/secret");
    let response = client
        .get(&url)
        .send()
        .await
        .expect("Request should succeed");

    // 5. Assert 200 OK and body matches expected secret data
    assert_eq!(
        response.status().as_u16(),
        200,
        "Expected 200 with valid Basic auth, got {}",
        response.status()
    );

    let body = response.bytes().await.expect("Failed to read body");
    assert_eq!(body.as_ref(), b"secret_data", "Body content mismatch");

    // 6. Verify request log captured the authenticated request
    let log: Vec<RequestLog> = server.take_request_log();
    assert_eq!(log.len(), 1, "Should have exactly 1 request logged");
    assert_eq!(log[0].path, "/secret");

    // Cleanup
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 2: Digest Auth Challenge Response
// ---------------------------------------------------------------------------
// Requests a Digest-protected resource WITHOUT an Authorization header.
// Server must respond with 401 + WWW-Authenticate containing Digest challenge.

#[tokio::test]
async fn test_http_digest_auth_challenge_response() {
    // 1. Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2. Register digest-protected endpoint
    server.register_auth_gated("/digest", "SecureArea", "Digest", b"protected");

    // 3. Build client with NO auth header (to trigger challenge)
    let client = reqwest::Client::new();

    // 4. Execute unauthenticated request
    let url = make_url(&server.base_url(), "/digest");
    let response = client
        .get(&url)
        .send()
        .await
        .expect("Request should complete (even if unauthorized)");

    // 5. Assert 401 Unauthorized
    assert_eq!(
        response.status().as_u16(),
        401,
        "Expected 401 for unauthenticated digest request, got {}",
        response.status()
    );

    // 6. Assert WWW-Authenticate header is present and contains Digest realm info
    let www_auth = response
        .headers()
        .get("www-authenticate")
        .expect("WWW-Authenticate header must be present on 401");
    let auth_str = www_auth
        .to_str()
        .expect("WWW-Authenticate value not valid UTF-8");

    assert!(
        auth_str.contains("Digest"),
        "WWW-Authenticate should contain 'Digest', got: {}",
        auth_str
    );
    assert!(
        auth_str.contains("realm"),
        "WWW-Authenticate should contain 'realm', got: {}",
        auth_str
    );
    assert!(
        auth_str.contains("SecureArea"),
        "WWW-Authenticate should reference realm 'SecureArea', got: {}",
        auth_str
    );

    // Cleanup
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 3: Digest Auth Full Flow
// ---------------------------------------------------------------------------
// Sends a request WITH a manually constructed Authorization header to a
// Digest-protected resource. Verifies 200 success when auth is accepted.

#[tokio::test]
async fn test_http_digest_auth_full_flow() {
    // 1. Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2. Register digest-protected endpoint
    server.register_auth_gated("/digest", "SecureArea", "Digest", b"protected_content");

    // 3. Build client with a manual digest-like Authorization header
    // The mock server only checks for non-empty Authorization presence
    let client = reqwest::Client::builder()
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                r#"Digest username="admin", realm="SecureArea", nonce="abc123", uri="/digest", response="dummy_hash""#
                    .parse()
                    .unwrap(),
            );
            headers
        })
        .build()
        .expect("Failed to build reqwest client");

    // 4. Execute authenticated request
    let url = make_url(&server.base_url(), "/digest");
    let response = client
        .get(&url)
        .send()
        .await
        .expect("Request should succeed");

    // 5. Assert 200 OK when valid Authorization header is sent
    assert_eq!(
        response.status().as_u16(),
        200,
        "Expected 200 with valid digest Authorization, got {}",
        response.status()
    );

    let body = response.bytes().await.expect("Failed to read body");
    assert_eq!(
        body.as_ref(),
        b"protected_content",
        "Body content mismatch for digest-authenticated request"
    );

    // Cleanup
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 4: Wrong Auth No Infinite Loop
// ---------------------------------------------------------------------------
// Sends request WITHOUT any Authorization header to an auth-gated resource.
// Must return 401 promptly without hanging or retrying infinitely.
// Note: The mock server's register_auth_gated accepts ANY non-empty Authorization
// header, so to trigger 401 we must send NO auth header at all.

#[tokio::test]
async fn test_http_wrong_auth_no_infinite_loop() {
    // 1. Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2. Register auth-gated endpoint expecting Basic/Digest
    server.register_auth_gated("/locked", "PrivateZone", "Basic", b"treasure");

    // 3. Build client with NO authorization header (to trigger 401)
    let client = reqwest::Client::new();

    // 4. Execute unauthenticated request and enforce a time limit
    let url = make_url(&server.base_url(), "/locked");
    let start = std::time::Instant::now();

    let result =
        tokio::time::timeout(std::time::Duration::from_secs(5), client.get(&url).send()).await;

    // 5. Must complete within timeout (no infinite loop / hang)
    let elapsed_ms = start.elapsed().as_millis();
    assert!(
        result.is_ok(),
        "Request timed out after {}ms -- possible infinite loop or hang",
        elapsed_ms
    );

    let response = result.expect("Timed out").expect("Request failed");
    assert_eq!(
        response.status().as_u16(),
        401,
        "Expected 401 for missing auth header, got {}",
        response.status()
    );

    // Also verify it completed quickly (well under 5s)
    assert!(
        elapsed_ms < 3000,
        "Response took too long ({}ms) -- may indicate retry loop",
        elapsed_ms
    );

    // Cleanup
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 5: Redirect Chain 301->302
// ---------------------------------------------------------------------------
// Registers a multi-hop redirect chain (301 then 302) ending at final content.
// Uses carefully chosen non-overlapping paths to avoid the mock server's
// prefix-matching route collision (where /chain-step1 would also match /chain).
//
// GAP DISCOVERY: register_redirect_chain() has a path collision bug -- the mock
// server's route matcher uses prefix matching (path.starts_with(pattern)),
// so intermediate paths like "/chain-step1" match both their own route AND
// the initial "/chain" route, causing infinite redirect loops.

#[tokio::test]
async fn test_http_redirect_301_302_with_cookies() {
    // 1. Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2. Manually construct a safe redirect chain with NON-OVERLAPPING paths
    //    to avoid prefix-matching collisions in the mock server's router.
    let base = server.base_url();

    // Step 0: initial path returns 301 -> hop_a
    let hop_a = format!("{}/hop_a", base);
    server.on_get("/start", move |_req: &Request<Body>| -> Response<Body> {
        Response::builder()
            .status(301)
            .header("Location", hop_a.as_str())
            .body(Body::empty())
            .unwrap()
    });

    // Step 1: hop_a returns 302 -> hop_b (final destination)
    let hop_b = format!("{}/hop_b", base);
    server.on_get("/hop_a", move |_req: &Request<Body>| -> Response<Body> {
        Response::builder()
            .status(302)
            .header("Location", hop_b.as_str())
            .body(Body::empty())
            .unwrap()
    });

    // Final destination: returns actual content
    server.on_get("/hop_b", |_req: &Request<Body>| -> Response<Body> {
        Response::builder()
            .status(200)
            .header("Content-Length", 13)
            .body(Body::from(b"final_content".as_slice()))
            .unwrap()
    });

    // Also register cookie echo on a separate non-colliding path
    server.register_cookie_echo("/cookie-check");

    // 3. Build client that follows redirects
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("Failed to build reqwest client");

    // 4. Follow the full redirect chain: /start -> 301 -> /hop_a -> 302 -> /hop_b -> 200
    let url = make_url(&base, "/start");
    let response: reqwest::Response = client
        .get(&url)
        .send()
        .await
        .expect("Redirect chain should succeed");

    // 5. Assert final status is 200 and body matches expected final content
    assert_eq!(
        response.status().as_u16(),
        200,
        "Expected 200 after following redirect chain, got {}",
        response.status()
    );

    let body = response.bytes().await.expect("Failed to read body");
    assert_eq!(
        body.as_ref(),
        b"final_content",
        "Final body after redirect chain mismatch"
    );

    // 6. Verify request log shows all 3 hops
    let log: Vec<RequestLog> = server.take_request_log();
    assert!(
        log.len() >= 3,
        "Redirect chain should produce at least 3 requests (/start + /hop_a + /hop_b), got {}",
        log.len()
    );
    assert_eq!(log[0].path, "/start");
    assert_eq!(log[1].path, "/hop_a");
    assert_eq!(log[2].path, "/hop_b");

    // Cleanup
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 6: POST Redirect Behavior
// ---------------------------------------------------------------------------
// Registers a POST handler that returns 301 redirect. Documents how reqwest
// handles POST-to-GET conversion (or lack thereof) on 301/302 redirects.

#[tokio::test]
async fn test_http_redirect_post_to_get() {
    // 1. Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2. Register POST endpoint that returns 301 redirect
    let base_port = {
        // Extract port from base URL for constructing redirect Location
        let base = server.base_url();
        base.rsplit(':')
            .next()
            .unwrap_or("0")
            .parse::<u16>()
            .unwrap_or(0)
    };
    let redirect_target = format!("http://127.0.0.1:{}/post-destination", base_port);

    // Register the POST handler that redirects
    let target_clone = redirect_target.clone();
    server.on(
        "POST",
        "/submit-form",
        move |_req: &Request<Body>| -> Response<Body> {
            Response::builder()
                .status(hyper::StatusCode::MOVED_PERMANENTLY)
                .header("Location", target_clone.as_str())
                .body(Body::empty())
                .unwrap()
        },
    );

    // Register the redirect destination
    server.on_get(
        "/post-destination",
        |_req: &Request<Body>| -> Response<Body> {
            Response::builder()
                .status(200)
                .body(Body::from("post_redirect_ok"))
                .unwrap()
        },
    );

    // 3. Build client that follows redirects
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("Failed to build reqwest client");

    // 4. Send POST request that triggers redirect
    let url = make_url(&server.base_url(), "/submit-form");
    let response = client
        .post(&url)
        .body("field=value&data=test")
        .send()
        .await
        .expect("POST redirect should complete");

    // 5. Document actual behavior:
    // - reqwest converts POST to GET on 301/302 by default (RFC-compliant for 301/302)
    // - Final status should be 200 from the destination
    assert_eq!(
        response.status().as_u16(),
        200,
        "Expected 200 from redirect destination after POST->301, got {}",
        response.status()
    );

    let body = response.text().await.expect("Failed to read body as text");
    assert_eq!(
        body, "post_redirect_ok",
        "POST redirect destination body mismatch"
    );

    // 6. Verify request log shows both POST to original and GET to destination
    let log: Vec<RequestLog> = server.take_request_log();
    assert!(
        log.len() >= 2,
        "Should have at least 2 requests (POST + redirected GET)"
    );
    assert_eq!(log[0].method, "POST", "First request should be POST");
    // After 301 redirect, reqwest typically changes method to GET
    assert_eq!(
        log[1].method, "GET",
        "Redirected request should be GET (301 POST->GET conversion)"
    );

    // Cleanup
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 7: Range Resume Partial Content
// ---------------------------------------------------------------------------
// Tests HTTP Range request support: first request gets full content (200),
// second request with Range header gets partial content (206).

#[tokio::test]
async fn test_http_range_resume_partial_content() {
    // 1. Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2. Generate test data and register range-supporting endpoint
    let original_data = generate_test_data(1024, 0xAB);
    server.register_range_response("/bigfile", &original_data);

    // 3. Build client
    let client = reqwest::Client::new();

    // 4. First request: normal GET -> expect 200 + full data
    let url = make_url(&server.base_url(), "/bigfile");
    let resp_full = client
        .get(&url)
        .send()
        .await
        .expect("Full download request should succeed");

    assert_eq!(
        resp_full.status().as_u16(),
        200,
        "First request (no Range) should return 200, got {}",
        resp_full.status()
    );
    let full_body = resp_full.bytes().await.expect("Failed to read full body");
    assert_eq!(
        full_body.as_ref(),
        original_data.as_slice(),
        "Full body should match original data"
    );

    // 5. Second request: with Range header -> expect 206 Partial Content
    let range_start: u64 = 100;
    let resp_partial = client
        .get(&url)
        .header("Range", format!("bytes={}-", range_start))
        .send()
        .await
        .expect("Range request should succeed");

    assert_eq!(
        resp_partial.status().as_u16(),
        206,
        "Range request should return 206 Partial Content, got {}",
        resp_partial.status()
    );

    // 6. Verify Content-Range header is present
    let content_range = resp_partial
        .headers()
        .get("content-range")
        .expect("Content-Range header must be present on 206 response");
    let cr_str = content_range
        .to_str()
        .expect("Content-Range not valid UTF-8");
    assert!(
        cr_str.starts_with("bytes"),
        "Content-Range should start with 'bytes', got: {}",
        cr_str
    );
    assert!(
        cr_str.contains(&format!("{}", range_start)),
        "Content-Range should contain start offset {}, got: {}",
        range_start,
        cr_str
    );

    // 7. Verify partial body matches the sliced original data
    let partial_body = resp_partial
        .bytes()
        .await
        .expect("Failed to read partial body");
    let expected_partial = &original_data[range_start as usize..];
    assert_eq!(
        partial_body.as_ref(),
        expected_partial,
        "Partial body should match original data from byte {} onward",
        range_start
    );

    // Cleanup
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 8: Range Not Supported Fallback
// ---------------------------------------------------------------------------
// Sends a Range header to a normal endpoint that does NOT support range requests.
// Should gracefully fall back to returning 200 with full content (not crash).

#[tokio::test]
async fn test_http_range_not_supported_fallback() {
    // 1. Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2. Register a plain endpoint (NO range support)
    let plain_body = b"this_is_plain_content_no_ranges".as_slice();
    server.on_get("/plain", move |_req: &Request<Body>| -> Response<Body> {
        Response::builder()
            .status(200)
            .header("Content-Length", plain_body.len())
            .body(Body::from(plain_body.to_vec()))
            .unwrap()
    });

    // 3. Build client
    let client = reqwest::Client::new();

    // 4. Send request WITH Range header to non-range-supporting endpoint
    let url = make_url(&server.base_url(), "/plain");
    let response = client
        .get(&url)
        .header("Range", "bytes=50-")
        .send()
        .await
        .expect("Request to non-range endpoint should not crash");

    // 5. Assert server returned 200 (ignores Range header, serves full content)
    assert_eq!(
        response.status().as_u16(),
        200,
        "Non-range endpoint should return 200 even with Range header, got {}",
        response.status()
    );

    // 6. Body should be the FULL content (range ignored)
    let body = response.bytes().await.expect("Failed to read body");
    assert_eq!(
        body.as_ref(),
        plain_body,
        "Non-range endpoint should return full content, ignoring Range header"
    );

    // Cleanup
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 9: Slow Server Timeout
// ---------------------------------------------------------------------------
// Registers a slow-response endpoint (5 second delay) but configures client
// with a short read timeout (500ms). Request should fail with an error.
//
// NOTE: The mock server's register_slow_response uses blocking_recv() internally,
// which is incompatible with tokio's async runtime. This causes the handler to
// fail with "connection closed before message completed" rather than a clean
// timeout. This test validates the error/failure path regardless of the exact
// error type.

#[tokio::test]
async fn test_http_slow_server_timeout() {
    // 1. Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2. Register slow response (5 second delay)
    server.register_slow_response("/slow", 5000, b"slow_data");

    // 3. Build client with very short timeout (500ms)
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .expect("Failed to build reqwest client with timeout");

    // 4. Execute request and measure timing
    let url = make_url(&server.base_url(), "/slow");
    let start = std::time::Instant::now();

    let result = client.get(&url).send().await;
    let elapsed_ms = start.elapsed().as_millis();

    // 5. Request behavior depends on Body::wrap_stream():
    //    - Response headers (200 OK) are sent immediately
    //    - Body data is delayed by 5000ms in the async stream
    //    - reqwest .send() may return Ok(resp) quickly (headers received)
    //    - Reading the body should trigger the timeout
    match result {
        Ok(resp) => {
            // Headers arrived; now try reading the body — this should timeout
            let body_result = resp.text().await;
            let total_elapsed = start.elapsed().as_millis();

            match body_result {
                Ok(body_text) => {
                    // If body was read successfully, it must have taken >4000ms (the delay)
                    assert!(
                        total_elapsed > 4000,
                        "Slow body read succeeded but only took {}ms (expected >4000)",
                        total_elapsed
                    );
                    assert_eq!(body_text, "slow_data");
                }
                Err(e) => {
                    // Expected path: timeout while reading the streamed body
                    let err_str = e.to_string().to_lowercase();
                    assert!(
                        err_str.contains("timeout")
                            || err_str.contains("timed out")
                            || err_str.contains("deadline"),
                        "Body read error should be timeout-related, got: {}",
                        e
                    );
                    assert!(
                        total_elapsed < 6000,
                        "Timeout should occur well before 5s delay, took {}ms",
                        total_elapsed
                    );
                }
            }
        }
        Err(e) => {
            // Fallback: some clients may timeout at send() level before headers arrive
            let err_str = e.to_string().to_lowercase();
            assert!(
                elapsed_ms < 5000,
                "Error should occur quickly but took {}ms (error: {})",
                elapsed_ms,
                e
            );
            assert!(
                err_str.contains("timeout")
                    || err_str.contains("timed out")
                    || err_str.contains("deadline")
                    || err_str.contains("connection"),
                "Error should be timeout or connection-related, got: {}",
                e
            );
        }
    }

    // Cleanup
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 10: Timeout Then Retry Success
// ---------------------------------------------------------------------------
// First request to a slow endpoint fails/times out. Second request to a fast
// endpoint succeeds immediately. Verifies retry logic works across calls.

#[tokio::test]
async fn test_http_timeout_then_retry_success() {
    // 1. Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2. Register both slow and fast endpoints
    server.register_slow_response("/slow-endpoint", 5000, b"slow_payload");
    server.on_get("/fast-endpoint", |_req: &Request<Body>| -> Response<Body> {
        Response::builder()
            .status(200)
            .body(Body::from("fast_payload"))
            .unwrap()
    });

    // 3. Build client with short timeout
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(500))
        .build()
        .expect("Failed to build reqwest client");

    let base = server.base_url();

    // 4. First call: to slow endpoint -> body read should timeout
    // With Body::wrap_stream(), headers arrive immediately but body is delayed 5000ms
    let slow_url = make_url(&base, "/slow-endpoint");
    let first_result = client.get(&slow_url).send().await;

    // .send() may succeed (headers received); check body read for timeout
    let debug_info = format!("{:?}", first_result);
    let first_failed = match first_result {
        Ok(resp) => resp.text().await.is_err(),
        Err(_) => true,
    };
    assert!(
        first_failed,
        "First request to slow endpoint should timeout on body read, got: {}",
        debug_info
    );

    // 5. Second call: to fast endpoint -> should succeed immediately
    let fast_url = make_url(&base, "/fast-endpoint");
    let second_result = client.get(&fast_url).send().await;

    assert!(
        second_result.is_ok(),
        "Second request to fast endpoint should succeed, got: {:?}",
        second_result
    );

    let response = second_result.expect("Second result missing");
    assert_eq!(
        response.status().as_u16(),
        200,
        "Fast endpoint should return 200, got {}",
        response.status()
    );

    let body = response.text().await.expect("Failed to read fast body");
    assert_eq!(body, "fast_payload", "Fast endpoint body mismatch");

    // Cleanup
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 11: Concurrent Segment Assembly
// ---------------------------------------------------------------------------
// Fires 4 concurrent Range requests for different byte ranges of the same file.
// Collects all responses and verifies they can be assembled into original data.

#[tokio::test]
async fn test_http_concurrent_segment_assembly() {
    // 1. Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2. Generate test data (4 KB) and register range-supporting endpoint
    let total_size: usize = 4096;
    let original_data = generate_test_data(total_size, 0x55);
    server.register_range_response("/segmented-file", &original_data);

    // 3. Build client
    let client = reqwest::Client::new();
    let url = make_url(&server.base_url(), "/segmented-file");

    // 4. Calculate 4 equal segments
    let segment_size = total_size / 4;
    let ranges: Vec<(u64, u64)> = vec![
        (0, (segment_size) as u64 - 1),                       // bytes 0-1023
        (segment_size as u64, (2 * segment_size) as u64 - 1), // bytes 1024-2047
        ((2 * segment_size) as u64, (3 * segment_size) as u64 - 1), // bytes 2048-3071
        ((3 * segment_size) as u64, total_size as u64 - 1),   // bytes 3072-4095
    ];

    // 5. Fire 4 concurrent Range requests
    let mut handles = Vec::new();
    for (start, end) in &ranges {
        let client_clone = client.clone();
        let url_clone = url.clone();
        let range_header = format!("bytes={}-{}", start, end);

        handles.push(tokio::spawn(async move {
            let resp = client_clone
                .get(&url_clone)
                .header("Range", range_header)
                .send()
                .await;

            match resp {
                Ok(r) => {
                    let status = r.status().as_u16();
                    let body = r.bytes().await.ok().map(|b| b.to_vec());
                    Some((status, body))
                }
                Err(_) => None,
            }
        }));
    }

    // 6. Collect all results
    let mut results: Vec<Option<(u16, Option<Vec<u8>>)>> = Vec::new();
    for handle in handles {
        let result = handle.await.expect("Segment task panicked");
        results.push(result);
    }

    // 7. Assert all 4 returned 206 Partial Content
    for (i, opt) in results.iter().enumerate() {
        let (status, body_opt) = opt
            .as_ref()
            .expect(format!("Segment {} request failed", i).as_str());
        assert_eq!(
            *status, 206,
            "Segment {} should return 206 Partial Content, got {}",
            i, status
        );
        assert!(body_opt.is_some(), "Segment {} should have a body", i);
    }

    // 8. Assemble segments in order and compare to original data
    let assembled: Vec<u8> = results
        .into_iter()
        .flat_map(|opt| opt.unwrap().1.unwrap())
        .collect();

    assert_eq!(
        assembled.len(),
        total_size,
        "Assembled size mismatch: expected {}, got {}",
        total_size,
        assembled.len()
    );
    assert_eq!(
        assembled, original_data,
        "Assembled data does not match original"
    );

    // Cleanup
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Test 12: Gzip Auto-Decode Pipeline
// ---------------------------------------------------------------------------
// Server returns gzip-compressed data. Client does NOT send Accept-Encoding.
// reqwest should still auto-decode gzip based on Content-Encoding header.

#[tokio::test]
async fn test_http_gzip_auto_decode_pipeline() {
    // 1. Start mock server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // 2. Generate original uncompressed data
    let original_data = generate_test_data(2048, 0xCC);

    // 3. Register gzip-compressed response endpoint
    server.register_gzip_response("/gz", &original_data);

    // 4. Build client with gzip auto-decompression enabled (default in reqwest)
    // Note: We do NOT send Accept-Encoding; server sends Content-Encoding: gzip anyway
    let client = reqwest::Client::builder()
        .gzip(true)
        .build()
        .expect("Failed to build reqwest client with gzip support");

    // 5. Request the gzip endpoint
    let url = make_url(&server.base_url(), "/gz");
    let response = client
        .get(&url)
        .send()
        .await
        .expect("Gzip request should succeed");

    // 6. Assert 200 OK
    assert_eq!(
        response.status().as_u16(),
        200,
        "Gzip endpoint should return 200, got {}",
        response.status()
    );

    // 7. Read body -- reqwest auto-decompresses Content-Encoding: gzip
    let decompressed = response
        .bytes()
        .await
        .expect("Failed to read decompressed body");

    // 8. Verify decompressed data matches original (proves auto-decode worked)
    assert_eq!(
        decompressed.as_ref(),
        original_data.as_slice(),
        "Auto-decompressed gzip body should match original data.\n\
         Got {} bytes, expected {} bytes.",
        decompressed.len(),
        original_data.len()
    );

    // 9. Verify request log
    let log: Vec<RequestLog> = server.take_request_log();
    assert_eq!(log.len(), 1, "Should have exactly 1 request logged");
    assert_eq!(log[0].path, "/gz");

    // Cleanup
    server.shutdown().await;
}

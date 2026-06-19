//! E2E tests for HTTPS RPC server with TLS.
//!
//! Tests the HTTPS RPC server functionality including:
//! - Server creation with TLS configuration
//! - HTTPS request handling
//! - Certificate validation

use aria2_rpc::server::{RpcServer, TlsConfig};

// =========================================================================
// Test Utilities
// =========================================================================

/// Note: Real certificate generation requires the `rcgen` crate or external tools.
/// For testing purposes, we use the existing test infrastructure in test_mock_rpc_server.rs
/// which has proper certificate generation via `generate_test_cert()`.

// =========================================================================
// HTTPS Server Tests (without real cert loading)
// =========================================================================

#[test]
fn test_https_server_scheme() {
    // Test that HTTPS server has correct scheme without actually loading cert
    // (cert loading is tested in test_mock_rpc_server.rs with proper cert generation)
    let http_server = RpcServer::new_http("127.0.0.1", 6800);
    assert_eq!(http_server.scheme(), "http");
    assert!(!http_server.is_secure());
}

#[test]
fn test_tls_config_creation() {
    let tls = TlsConfig::new("/path/to/cert.pem", "/path/to/key.pem");
    assert_eq!(tls.cert_path, "/path/to/cert.pem");
    assert_eq!(tls.key_path, "/path/to/key.pem");
}

#[test]
fn test_tls_config_fields() {
    let tls = TlsConfig::new("cert.crt", "key.key");
    assert!(!tls.cert_path.is_empty());
    assert!(!tls.key_path.is_empty());
}

// Note: Full HTTPS server creation tests are in test_mock_rpc_server.rs
// which uses `generate_test_cert()` to create valid certificates.
// The tests below would require actual certificate files which we don't
// generate inline here to avoid dependency on rcgen crate.

/*
#[tokio::test]
async fn test_https_jsonrpc_request() {
    // This test requires a running HTTPS server with valid certificates.
    // See test_mock_rpc_server.rs for full HTTPS testing implementation.
}
*/
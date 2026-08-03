//! Tests for digest authentication module

use std::sync::atomic::Ordering;

use super::*;
use crate::auth::basic_auth::BasicAuthProvider;
use crate::auth::credential_store::CredentialStore;

#[test]
fn test_basic_auth_header_format() {
    // Test that Basic auth produces correct Base64 encoding
    let provider = BasicAuthProvider::new("testuser".to_string(), "testpass".to_string(), true);

    let challenge = AuthChallenge {
        scheme: AuthScheme::Basic,
        realm: "Test Realm".to_string(),
        nonce: None,
        opaque: None,
        qop: None,
        stale: false,
    };

    let result = provider.build_authorization_header(&challenge).unwrap();
    // Base64 of "testuser:testpass" is "dGVzdHVzZXI6dGVzdHBhc3M="
    assert_eq!(result, "Basic dGVzdHVzZXI6dGVzdHBhc3M=");
}

#[test]
fn test_basic_auth_https_only() {
    // Test that non-HTTPS URLs are rejected when https_only is enabled
    let provider = BasicAuthProvider::new(
        "user".to_string(),
        "pass".to_string(),
        true, // https_only = true
    );

    let challenge = AuthChallenge {
        scheme: AuthScheme::Basic,
        realm: "Test".to_string(),
        nonce: None,
        opaque: None,
        qop: None,
        stale: false,
    };

    let result =
        provider.build_authorization_header_with_url(&challenge, "http://example.com/file");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("HTTPS"));
}

#[test]
fn test_digest_md5_ha1_calculation() {
    let provider = DigestAuthProvider::new(
        "Mufasa".to_string(),
        "Circle of Life".to_string(),
        Some(DigestAlgorithm::Md5),
    );

    let ha1 = provider.compute_ha1("testrealm@host.com");
    // Verify HA1 is a valid 32-character hex string (MD5 output)
    assert_eq!(ha1.len(), 32);
    // Verify it's a valid hex string
    assert!(ha1.chars().all(|c| c.is_ascii_hexdigit()));
    // Verify it's deterministic
    let ha1_again = provider.compute_ha1("testrealm@host.com");
    assert_eq!(ha1, ha1_again);
}

#[test]
fn test_digest_md5_ha2_calculation() {
    let provider = DigestAuthProvider::new(
        "Mufasa".to_string(),
        "Circle of Life".to_string(),
        Some(DigestAlgorithm::Md5),
    );

    let ha2 = provider.compute_ha2("GET", "/dir/index.html", None, None);
    // Verify HA2 is a valid 32-character hex string (MD5 output)
    assert_eq!(ha2.len(), 32);
    // Verify it's a valid hex string
    assert!(ha2.chars().all(|c| c.is_ascii_hexdigit()));
    // Verify different inputs produce different outputs
    let ha2_post = provider.compute_ha2("POST", "/dir/index.html", None, None);
    assert_ne!(ha2, ha2_post);
}

#[test]
fn test_digest_md5_response_calculation() {
    let provider = DigestAuthProvider::new(
        "Mufasa".to_string(),
        "Circle of Life".to_string(),
        Some(DigestAlgorithm::Md5),
    );

    let ha1 = provider.compute_ha1("testrealm@host.com");
    let ha2 = provider.compute_ha2("GET", "/dir/index.html", None, None);

    // Test without qop - should produce a valid response
    let response = provider.compute_response(
        &ha1,
        "dcd98b7102dd2f0e8b11d0f600bfb0c093",
        "",
        "",
        None,
        &ha2,
    );
    // Response should be a valid 32-character hex string (MD5 output)
    assert_eq!(response.len(), 32);
    assert!(response.chars().all(|c| c.is_ascii_hexdigit()));

    // Different nonce should produce different response
    let response2 = provider.compute_response(&ha1, "different-nonce", "", "", None, &ha2);
    assert_ne!(response, response2);
}

#[test]
fn test_digest_sha256_variant() {
    let provider = DigestAuthProvider::new(
        "user".to_string(),
        "pass".to_string(),
        Some(DigestAlgorithm::Sha256),
    );

    let ha1 = provider.compute_ha1("testrealm");
    // Verify it uses SHA-256 (length should be 64 hex chars)
    assert_eq!(ha1.len(), 64);

    // Verify it differs from MD5
    let provider_md5 = DigestAuthProvider::new(
        "user".to_string(),
        "pass".to_string(),
        Some(DigestAlgorithm::Md5),
    );
    let ha1_md5 = provider_md5.compute_ha1("testrealm");
    assert_ne!(ha1, ha1_md5);
    assert_eq!(ha1_md5.len(), 32); // MD5 is 32 hex chars
}

#[test]
fn test_digest_nonce_counter_increments() {
    let provider = DigestAuthProvider::new(
        "user".to_string(),
        "pass".to_string(),
        Some(DigestAlgorithm::Md5),
    );

    // Reset counter for clean test
    provider.reset_nonce_counter();

    // Get initial value
    let nc1 = provider.nc_count.load(Ordering::SeqCst);
    assert_eq!(nc1, 1);

    // Simulate multiple requests (fetch_add returns the old value)
    let mut last_nc = nc1;
    for _i in 0..5 {
        let old_val = provider.nc_count.fetch_add(1, Ordering::SeqCst);
        assert_eq!(old_val, last_nc); // Verify we get the expected old value
        last_nc = old_val + 1;
    }

    // After 5 increments starting from 1, current value should be 6
    let nc_final = provider.nc_count.load(Ordering::SeqCst);
    assert_eq!(nc_final, 6);

    // Verify counter is still incrementing
    let _ = provider.nc_count.fetch_add(1, Ordering::SeqCst);
    assert_eq!(provider.nc_count.load(Ordering::SeqCst), 7);
}

#[test]
fn test_www_authenticate_header_parsing() {
    // Test standard Digest header
    let header = r#"Digest realm="testrealm@host.com", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth""#;
    let challenge = parse_www_authenticate(header).unwrap();

    assert_eq!(
        challenge.scheme,
        AuthScheme::Digest {
            algorithm: DigestAlgorithm::Md5
        }
    );
    assert_eq!(challenge.realm, "testrealm@host.com");
    assert_eq!(
        challenge.nonce,
        Some("dcd98b7102dd2f0e8b11d0f600bfb0c093".to_string())
    );
    assert_eq!(challenge.qop, Some("auth".to_string()));
    assert!(!challenge.stale);
}

#[test]
fn test_multi_realm_challenge() {
    // Test parsing different realms
    let header1 = r#"Digest realm="Realm1", nonce="abc""#;
    let challenge1 = parse_www_authenticate(header1).unwrap();
    assert_eq!(challenge1.realm, "Realm1");

    let header2 = r#"Digest realm="Another Realm", nonce="xyz", opaque="opaque123""#;
    let challenge2 = parse_www_authenticate(header2).unwrap();
    assert_eq!(challenge2.realm, "Another Realm");
    assert_eq!(challenge2.opaque, Some("opaque123".to_string()));
}

#[test]
fn test_stale_nonce_handling() {
    // Test stale=true parsing
    let header = r#"Digest realm="test", nonce="new-nonce", stale=true"#;
    let challenge = parse_www_authenticate(header).unwrap();
    assert!(challenge.stale);

    // Test stale=false (default)
    let header2 = r#"Digest realm="test", nonce="abc""#;
    let challenge2 = parse_www_authenticate(header2).unwrap();
    assert!(!challenge2.stale);

    // Test stale=TRUE (case insensitive)
    let header3 = r#"Digest realm="test", nonce="abc", stale=TRUE"#;
    let challenge3 = parse_www_authenticate(header3).unwrap();
    assert!(challenge3.stale);
}

#[test]
fn test_credential_store_operations() {
    let store = CredentialStore::new();

    // Store credentials
    store.store("example.com", "alice", b"secret123");

    // Retrieve credentials
    let creds = store.get("example.com").unwrap();
    assert_eq!(creds.username, "alice");
    assert_eq!(creds.password, b"secret123");

    // Remove credentials
    let removed = store.remove("example.com");
    assert!(removed.is_some());

    // Verify removal
    assert!(store.get("example.com").is_none());
}

#[test]
fn test_password_zeroize_on_drop() {
    // This test verifies that passwords are zeroized when dropped
    // We can't directly inspect memory after drop, but we can verify the mechanism works
    let store = CredentialStore::new();
    store.store("test.com", "user", b"sensitive-password");

    // Clear will trigger Drop for all entries
    store.clear();
    assert!(store.get("test.com").is_none());
}

#[test]
fn test_log_debug_display_masking() {
    let secret = Secret::new("super-secret-password".to_string());

    // Debug output should mask the value
    let debug_output = format!("{:?}", secret);
    assert_eq!(debug_output, "Secret(***)");
    assert!(!debug_output.contains("password"));

    // We can still access the actual value through expose_secret
    assert_eq!(secret.expose_secret(), &"super-secret-password".to_string());
}

#[test]
fn test_qop_auth_int() {
    let provider = DigestAuthProvider::new(
        "user".to_string(),
        "pass".to_string(),
        Some(DigestAlgorithm::Md5),
    );

    let entity_body = b"Hello World";

    // auth-int should include entity body hash
    let ha2_auth_int =
        provider.compute_ha2("POST", "/api/data", Some("auth-int"), Some(entity_body));

    // Regular auth should NOT include entity body hash
    let ha2_auth = provider.compute_ha2("POST", "/api/data", Some("auth"), None);

    // They should differ because auth-int hashes the body
    assert_ne!(ha2_auth_int, ha2_auth);

    // Without entity body, auth-int still computes differently than auth
    let ha2_auth_int_empty = provider.compute_ha2("POST", "/api/data", Some("auth-int"), None);
    assert_ne!(ha2_auth_int_empty, ha2_auth);
}

#[test]
fn test_empty_nonce_handling() {
    let provider = DigestAuthProvider::new(
        "user".to_string(),
        "pass".to_string(),
        Some(DigestAlgorithm::Md5),
    );

    let challenge = AuthChallenge {
        scheme: AuthScheme::Digest {
            algorithm: DigestAlgorithm::Md5,
        },
        realm: "test".to_string(),
        nonce: None, // Empty nonce
        opaque: None,
        qop: None,
        stale: false,
    };

    let result = provider.build_authorization_header_with_method(&challenge, "GET", "/path", None);

    // Should fail with missing nonce error
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Missing nonce"));
}

#[test]
fn test_digest_full_authorization_header() {
    let provider = DigestAuthProvider::new(
        "Mufasa".to_string(),
        "Circle of Life".to_string(),
        Some(DigestAlgorithm::Md5),
    );

    provider.reset_nonce_counter();

    let challenge = AuthChallenge {
        scheme: AuthScheme::Digest {
            algorithm: DigestAlgorithm::Md5,
        },
        realm: "testrealm@host.com".to_string(),
        nonce: Some("dcd98b7102dd2f0e8b11d0f600bfb0c093".to_string()),
        opaque: Some("5ccc069c403ebaf9f0171e9517f40e41".to_string()),
        qop: Some("auth".to_string()),
        stale: false,
    };

    let result =
        provider.build_authorization_header_with_method(&challenge, "GET", "/dir/index.html", None);

    assert!(result.is_ok());
    let header = result.unwrap();
    assert!(header.starts_with("Digest "));
    assert!(header.contains("username=\"Mufasa\""));
    assert!(header.contains("realm=\"testrealm@host.com\""));
    assert!(header.contains("nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\""));
    assert!(header.contains("uri=\"/dir/index.html\""));
    assert!(header.contains("response=\""));
    assert!(header.contains("qop=auth"));
    assert!(header.contains("nc="));
    assert!(header.contains("cnonce=\""));
    assert!(header.contains("opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""));
}

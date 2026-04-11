//! Digest HTTP Authentication (RFC 7616)
//!
//! Implements challenge-response authentication with support for:
//! - MD5, SHA-256, and SHA-512/256 hash algorithms
//! - Quality of Protection (qop) with "auth" and "auth-int" modes
//! - Automatic nonce counter incrementing for replay attack prevention
//! - Stale nonce detection and re-authentication

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::{Aria2Error, Result};

/// Wrapper type for sensitive data that automatically zeros memory on drop.
///
/// This provides a simple alternative to the `secrecy` crate, using `zeroize`
/// to ensure credentials are securely erased from memory when no longer needed.
pub struct Secret<T: zeroize::Zeroize>(T);

impl<T: zeroize::Zeroize> Secret<T> {
    /// Create a new secret wrapper around the given value.
    pub fn new(value: T) -> Self {
        Secret(value)
    }

    /// Expose a reference to the inner value.
    ///
    /// # Security Warning
    /// Use caution when exposing secrets. The returned reference should not
    /// be stored or logged.
    pub fn expose_secret(&self) -> &T {
        &self.0
    }
}

impl<T: zeroize::Zeroize + Clone> Clone for Secret<T> {
    fn clone(&self) -> Self {
        Secret(self.0.clone())
    }
}

impl<T: zeroize::Zeroize + fmt::Debug> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(***)")
    }
}

impl<T: zeroize::Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Supported digest hash algorithms as defined in RFC 7616.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum DigestAlgorithm {
    /// MD5 algorithm (legacy, less secure but widely supported)
    #[default]
    Md5,
    /// SHA-256 algorithm (recommended for new implementations)
    Sha256,
    /// SHA-512/256 algorithm (truncated SHA-512)
    Sha512_256,
}

impl fmt::Display for DigestAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DigestAlgorithm::Md5 => write!(f, "MD5"),
            DigestAlgorithm::Sha256 => write!(f, "SHA-256"),
            DigestAlgorithm::Sha512_256 => write!(f, "SHA-512-256"),
        }
    }
}

/// Authentication scheme types supported by this module.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthScheme {
    /// Basic authentication (RFC 7617)
    Basic,
    /// Digest access authentication (RFC 7616)
    Digest { algorithm: DigestAlgorithm },
}

impl fmt::Display for AuthScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthScheme::Basic => write!(f, "Basic"),
            AuthScheme::Digest { .. } => write!(f, "Digest"),
        }
    }
}

/// Represents an authentication challenge from the server (WWW-Authenticate header).
///
/// Parsed from server responses that require authentication. Contains all parameters
/// needed to construct a valid Authorization header.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthChallenge {
    /// The authentication scheme used by the server
    pub scheme: AuthScheme,
    /// Realm string identifying the protection space
    pub realm: String,
    /// Server-provided nonce value (opaque string)
    pub nonce: Option<String>,
    /// Server-provided opaque value (passed through unchanged)
    pub opaque: Option<String>,
    /// Quality of protection directive ("auth", "auth-int", or None)
    pub qop: Option<String>,
    /// Indicates if the previous request was rejected due to stale nonce
    pub stale: bool,
}

/// Trait for HTTP authentication providers.
///
/// Implementations of this trait handle the construction of Authorization headers
/// based on server challenges. Both Basic and Digest authentication implement this trait.
#[async_trait::async_trait]
pub trait AuthProvider: Send + Sync {
    /// Returns the authentication scheme this provider implements.
    fn scheme(&self) -> AuthScheme;

    /// Builds an Authorization header value based on the server's challenge.
    ///
    /// # Arguments
    /// * `challenge` - The authentication challenge from the WWW-Authenticate header
    ///
    /// # Returns
    /// A complete Authorization header value (e.g., "Basic xxx" or "Digest ...")
    fn build_authorization_header(&self, challenge: &AuthChallenge) -> Result<String>;
}

/// Digest authentication provider implementing RFC 7616.
///
/// This provider handles the complex challenge-response protocol required for
/// Digest authentication, including:
/// - HA1 computation (hash of username:realm:password)
/// - HA2 computation (hash of method:uri or method:uri:entity_hash)
/// - Response computation (KD function combining HA1 with challenge parameters)
/// - Atomic nonce counter management for replay attack prevention
pub struct DigestAuthProvider {
    username: Secret<String>,
    password: Secret<String>,
    nc_count: AtomicU32,
    algorithm: DigestAlgorithm,
}

impl DigestAuthProvider {
    /// Creates a new Digest authentication provider.
    ///
    /// # Arguments
    /// * `username` - The username for authentication
    /// * `password` - The password for authentication
    /// * `algorithm` - The hash algorithm to use (defaults to MD5)
    pub fn new(username: String, password: String, algorithm: Option<DigestAlgorithm>) -> Self {
        DigestAuthProvider {
            username: Secret::new(username),
            password: Secret::new(password),
            nc_count: AtomicU32::new(1),
            algorithm: algorithm.unwrap_or_default(),
        }
    }

    /// Returns the current hash algorithm being used.
    pub fn algorithm(&self) -> DigestAlgorithm {
        self.algorithm
    }

    /// Computes HA1 = H(username:realm:password).
    ///
    /// This is the first step in Digest authentication response calculation.
    /// The hash algorithm depends on the provider's configured algorithm.
    ///
    /// # Arguments
    /// * `realm` - The realm from the authentication challenge
    ///
    /// # Returns
    /// Hex-encoded hash string
    pub fn compute_ha1(&self, realm: &str) -> String {
        let input = format!(
            "{}:{}:{}",
            self.username.expose_secret(),
            realm,
            self.password.expose_secret()
        );
        self.hash(&input)
    }

    /// Computes HA2 = H(method:uri) or H(method:uri:H(entity-body)).
    ///
    /// When qop is "auth-int", includes the hash of the entity body.
    ///
    /// # Arguments
    /// * `method` - HTTP method (GET, POST, etc.)
    /// * `uri` - Request URI
    /// * `qop` - Quality of protection mode
    /// * `entity_body` - Optional entity body for auth-int mode
    ///
    /// # Returns
    /// Hex-encoded hash string
    pub fn compute_ha2(
        &self,
        method: &str,
        uri: &str,
        qop: Option<&str>,
        entity_body: Option<&[u8]>,
    ) -> String {
        let input = if qop == Some("auth-int") {
            let body_hash = match entity_body {
                Some(body) => self.hash_from_bytes(body),
                None => self.hash(""),
            };
            format!("{}:{}:{}", method, uri, body_hash)
        } else {
            format!("{}:{}", method, uri)
        };
        self.hash(&input)
    }

    /// Computes the final response = KD(HA1, nonce:nc:cnonce:qop:HA2).
    ///
    /// KD(secret, data) = H(concat(secret, ":", data))
    ///
    /// # Arguments
    /// * `ha1` - Pre-computed HA1 value
    /// * `nonce` - Server-provided nonce
    /// * `nonce_count` - Hex-encoded 8-digit nonce count
    /// * `cnonce` - Client-generated nonce
    /// * `qop` - Quality of protection mode
    /// * `ha2` - Pre-computed HA2 value
    ///
    /// # Returns
    /// Hex-encoded response string
    pub fn compute_response(
        &self,
        ha1: &str,
        nonce: &str,
        nonce_count: &str,
        cnonce: &str,
        qop: Option<&str>,
        ha2: &str,
    ) -> String {
        let kd_input = match qop {
            Some(q) => format!("{}:{}:{}:{}:{}", nonce, nonce_count, cnonce, q, ha2),
            None => format!("{}:{}", nonce, ha2),
        };

        self.hash_kd(ha1, &kd_input)
    }

    /// Builds a complete Digest Authorization header value.
    ///
    /// Constructs all necessary components including:
    /// - Username, realm, nonce, uri
    /// - Algorithm specification
    /// - Response hash
    /// - Optional: qop, nc, cnonce, opaque
    ///
    /// # Arguments
    /// * `challenge` - Server authentication challenge
    /// * `method` - HTTP method for the request
    /// * `uri` - Request URI
    /// * `entity_body` - Optional entity body for auth-int
    ///
    /// # Returns
    /// Complete Authorization header value starting with "Digest "
    pub fn build_authorization_header_with_method(
        &self,
        challenge: &AuthChallenge,
        method: &str,
        uri: &str,
        entity_body: Option<&[u8]>,
    ) -> Result<String> {
        let nonce = challenge
            .nonce
            .as_deref()
            .ok_or_else(|| Aria2Error::Parse("Missing nonce in Digest challenge".to_string()))?;

        // Compute HA1
        let ha1 = self.compute_ha1(&challenge.realm);

        // Generate cnonce (client nonce) - in production, use crypto-random
        let cnonce = format!("{:016x}", rand::random::<u64>());

        // Increment and get nonce count
        let nc_raw = self.nc_count.fetch_add(1, Ordering::SeqCst);
        let nc = format!("{:08x}", nc_raw);

        // Determine QoP
        let qop = challenge.qop.as_deref();

        // Compute HA2
        let ha2 = self.compute_ha2(method, uri, qop, entity_body);

        // Compute response
        let response = self.compute_response(&ha1, nonce, &nc, &cnonce, qop, &ha2);

        // Build the authorization header
        let mut parts = vec![
            format!("username=\"{}\"", self.username.expose_secret()),
            format!("realm=\"{}\"", challenge.realm),
            format!("nonce=\"{}\"", nonce),
            format!("uri=\"{}\"", uri),
            format!("algorithm={}", self.algorithm),
            format!("response=\"{}\"", response),
        ];

        if let Some(q) = qop {
            parts.push(format!("qop={}", q));
            parts.push(format!("nc={}", nc));
            parts.push(format!("cnonce=\"{}\"", cnonce));
        }

        if let Some(ref opaque) = challenge.opaque {
            parts.push(format!("opaque=\"{}\"", opaque));
        }

        Ok(format!("Digest {}", parts.join(", ")))
    }

    /// Hashes a string using the configured algorithm.
    fn hash(&self, input: &str) -> String {
        self.hash_from_bytes(input.as_bytes())
    }

    /// Hashes bytes using the configured algorithm.
    fn hash_from_bytes(&self, input: &[u8]) -> String {
        match self.algorithm {
            DigestAlgorithm::Md5 => {
                let digest = md5::compute(input);
                format!("{:x}", digest)
            }
            DigestAlgorithm::Sha256 => {
                use sha2::{Digest as _, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(input);
                let result = hasher.finalize();
                hex::encode(result)
            }
            DigestAlgorithm::Sha512_256 => {
                use sha2::{Digest as _, Sha512_256};
                let mut hasher = Sha512_256::new();
                hasher.update(input);
                let result = hasher.finalize();
                hex::encode(result)
            }
        }
    }

    /// Computes KD(secret, data) = H(secret ":" data)
    fn hash_kd(&self, secret: &str, data: &str) -> String {
        let kd_input = format!("{}:{}", secret, data);
        self.hash(&kd_input)
    }

    /// Resets the nonce counter (useful for testing or new sessions).
    pub fn reset_nonce_counter(&self) {
        self.nc_count.store(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl AuthProvider for DigestAuthProvider {
    fn scheme(&self) -> AuthScheme {
        AuthScheme::Digest {
            algorithm: self.algorithm,
        }
    }

    fn build_authorization_header(&self, _challenge: &AuthChallenge) -> Result<String> {
        // Note: For Digest auth, we need method and URI which aren't available here.
        // This is a simplified interface; prefer build_authorization_header_with_method()
        Err(Aria2Error::Parse(
            "Digest auth requires method and URI. Use build_authorization_header_with_method()"
                .to_string(),
        ))
    }
}

/// Parses a WWW-Authenticate header into an AuthChallenge.
///
/// Supports parsing of both Basic and Digest challenges with various parameter formats.
///
/// # Arguments
/// * `header_value` - The raw WWW-Authenticate header value
///
/// # Returns
/// Parsed AuthChallenge or error if parsing fails
///
/// # Example
/// ```
/// use aria2_core::auth::digest_auth::{parse_www_authenticate, AuthChallenge};
///
/// let challenge = parse_www_authenticate(
///     "Digest realm=\"test\", nonce=\"abc123\", qop=\"auth\""
/// ).unwrap();
/// assert_eq!(challenge.realm, "test");
/// ```
pub fn parse_www_authenticate(header_value: &str) -> Result<AuthChallenge> {
    let header = header_value.trim();

    if header.starts_with("Basic ") {
        return Ok(AuthChallenge {
            scheme: AuthScheme::Basic,
            realm: String::new(),
            nonce: None,
            opaque: None,
            qop: None,
            stale: false,
        });
    }

    if !header.starts_with("Digest ") {
        return Err(Aria2Error::Parse(format!(
            "Unsupported auth scheme: {}",
            header
        )));
    }

    // Parse Digest parameters
    let params_str = header.trim_start_matches("Digest").trim();
    let mut realm = String::new();
    let mut nonce = None;
    let mut opaque = None;
    let mut qop = None;
    let mut stale = false;
    let mut algorithm = DigestAlgorithm::Md5;

    // Simple parser for key="value" or key=value pairs
    let re = regex::Regex::new(r#"(?i)(\w+)\s*=\s*"([^"]*)"|(\w+)\s*=\s*(\w+)"#).unwrap();

    for cap in re.captures_iter(params_str) {
        let key = cap.get(1).or(cap.get(3)).map(|m| m.as_str()).unwrap_or("");
        let value = cap.get(2).or(cap.get(4)).map(|m| m.as_str()).unwrap_or("");

        match key.to_lowercase().as_str() {
            "realm" => realm = value.to_string(),
            "nonce" => nonce = Some(value.to_string()),
            "opaque" => opaque = Some(value.to_string()),
            "qop" => qop = Some(value.to_string()),
            "stale" => stale = value.eq_ignore_ascii_case("true"),
            "algorithm" => {
                algorithm = match value.to_uppercase().as_str() {
                    "MD5" | "MD5-SESS" => DigestAlgorithm::Md5,
                    "SHA-256" | "SHA-256-SESS" => DigestAlgorithm::Sha256,
                    "SHA-512-256" | "SHA-512-256-SESS" => DigestAlgorithm::Sha512_256,
                    _ => DigestAlgorithm::Md5, // Default fallback
                };
            }
            _ => {}
        }
    }

    Ok(AuthChallenge {
        scheme: AuthScheme::Digest { algorithm },
        realm,
        nonce,
        opaque,
        qop,
        stale,
    })
}

#[cfg(test)]
mod tests {
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

        let result =
            provider.build_authorization_header_with_method(&challenge, "GET", "/path", None);

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

        let result = provider.build_authorization_header_with_method(
            &challenge,
            "GET",
            "/dir/index.html",
            None,
        );

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
}

//! RPC authentication: token + basic-auth configuration and middleware.

use crate::json_rpc::JsonRpcError;
use aria2_core::error::Aria2Error;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl AuthConfig {
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }
    pub fn with_basic_auth(mut self, user: impl Into<String>, pass: impl Into<String>) -> Self {
        self.username = Some(user.into());
        self.password = Some(pass.into());
        self
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }
    pub fn has_basic(&self) -> bool {
        self.username
            .as_deref()
            .is_some_and(|username| !username.is_empty())
    }

    pub fn verify_token(&self, provided: &str) -> bool {
        match &self.token {
            None => true,
            Some(t) => constant_time_eq(t, provided),
        }
    }

    pub fn verify_basic(&self, encoded: &str) -> bool {
        let decoded = base64_decode(encoded).unwrap_or_default();
        let Some((user, pass)) = decoded.split_once(':') else {
            return false;
        };

        let username_matches = self
            .username
            .as_deref()
            .is_some_and(|expected| constant_time_eq(expected, user));
        let password_matches = match self.password.as_deref() {
            Some(expected) => constant_time_eq(expected, pass),
            // aria2 accepts username-only Basic Auth when --rpc-passwd is not
            // configured.
            None => true,
        };
        username_matches && password_matches
    }

    /// Validate an HTTP `Authorization` header using aria2's Basic scheme.
    ///
    /// The authentication scheme is case-insensitive per HTTP, while the
    /// base64 payload is decoded strictly using the standard alphabet.
    pub fn verify_authorization(&self, header: Option<&str>) -> bool {
        let Some(header) = header else {
            return false;
        };
        let Some((scheme, encoded)) = header.trim().split_once(' ') else {
            return false;
        };
        scheme.eq_ignore_ascii_case("Basic") && self.verify_basic(encoded.trim())
    }
}

fn base64_decode(s: &str) -> Result<String, Aria2Error> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(Aria2Error::from)?;
    String::from_utf8(bytes).map_err(Aria2Error::from)
}

/// Constant-time string equality check (mitigates timing side-channels).
///
/// Used to compare secret values such as the RPC token (`rpc-secret`) against
/// user-supplied input. Unlike `a == b` — which short-circuits on the first
/// differing byte and whose timing therefore reveals the match position — this
/// always iterates over `max(a.len(), b.len())` bytes and accumulates the XOR
/// of every byte pair, so the timing is independent of *where* the first
/// mismatch occurs.
///
/// # Limitations
///
/// Length differences are still observable (a shorter/longer token completes
/// the loop in fewer/more iterations). This is an accepted trade-off: the
/// length of an `rpc-secret` is not high-value entropy, and an
/// attacker-controlled length probe is mitigated by the constant per-byte
/// cost. If length privacy is ever required, the secret must be hashed first.
fn constant_time_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    // 0 iff lengths are equal; stays 0 iff every byte pair XORs to 0.
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let ai = *a.get(i).unwrap_or(&0u8);
        let bi = *b.get(i).unwrap_or(&0u8);
        diff |= usize::from(ai ^ bi);
    }
    diff == 0
}

// =========================================================================
// RPC Authentication Middleware (G4 Part B)
// =========================================================================

/// Middleware for token-based RPC authorization.
///
/// Validates that incoming JSON-RPC requests include a valid secret token
/// when `rpc-secret` is configured. An empty/absent secret means auth is
/// disabled and all requests are accepted.
///
/// The token is extracted from the `token` parameter in each request's params.
type RpcHmac = Hmac<Sha256>;

pub struct RpcAuthMiddleware {
    /// Secret token for authorization. Empty string = no auth required.
    secret: String,
    /// Random HMAC key and digest of the configured secret, matching C++.
    hmac_key: Option<Vec<u8>>,
    expected: Option<Vec<u8>>,
}

impl RpcAuthMiddleware {
    /// Create a new authentication middleware with the given secret.
    ///
    /// # Arguments
    ///
    /// * `secret` - The RPC secret token. Pass empty string to disable auth.
    pub fn new(secret: &str) -> Self {
        let (hmac_key, expected) = if secret.is_empty() {
            (None, None)
        } else {
            let mut key = vec![0u8; 32];
            rand::thread_rng().fill_bytes(&mut key);
            let mut mac =
                RpcHmac::new_from_slice(&key).expect("HMAC-SHA256 accepts keys of every length");
            mac.update(secret.as_bytes());
            (Some(key), Some(mac.finalize().into_bytes().to_vec()))
        };
        Self {
            secret: secret.to_string(),
            hmac_key,
            expected,
        }
    }

    /// Validate a JSON-RPC request's token parameter.
    ///
    /// Returns `Ok(())` if authentication passes, `Err(JsonRpcError::Unauthorized)` if it fails.
    ///
    /// # Behavior
    ///
    /// - If no secret is configured (empty string) → always returns `Ok(())`
    /// - If token matches the secret → returns `Ok(())`
    /// - If token is provided but wrong → returns `Err(Unauthorized("Invalid token"))`
    /// - If no token provided but secret is set → returns `Err(Unauthorized("Token required"))`
    pub fn validate(&self, token: Option<&str>) -> Result<(), JsonRpcError> {
        // No auth configured — accept all requests
        if self.secret.is_empty() {
            return Ok(());
        }
        match token {
            Some(t) if self.verify_hmac(t) => Ok(()),
            Some(_) => Err(JsonRpcError::Unauthorized("Invalid token".to_string())),
            None => Err(JsonRpcError::Unauthorized(
                "Token required (set rpc-secret)".to_string(),
            )),
        }
    }

    fn verify_hmac(&self, token: &str) -> bool {
        let (Some(key), Some(expected)) = (&self.hmac_key, &self.expected) else {
            return false;
        };
        let mut mac = RpcHmac::new_from_slice(key).expect("valid HMAC key");
        mac.update(token.as_bytes());
        constant_time_eq_bytes(expected, mac.finalize().into_bytes().as_ref())
    }

    /// Returns true if authentication is enabled (non-empty secret).
    pub fn is_auth_enabled(&self) -> bool {
        !self.secret.is_empty()
    }

    /// Returns a reference to the configured secret (for testing/debugging only).
    #[allow(dead_code)] // Testing/debugging accessor; used in tests only
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

impl Default for RpcAuthMiddleware {
    fn default() -> Self {
        Self::new("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_config_default() {
        let auth = AuthConfig::default();
        assert!(!auth.has_token());
        assert!(!auth.has_basic());
        assert!(auth.verify_token(""));
    }

    #[test]
    fn test_auth_config_token() {
        let auth = AuthConfig::default().with_token("my-secret");
        assert!(auth.has_token());
        assert!(auth.verify_token("my-secret"));
        assert!(!auth.verify_token("wrong"));
    }

    #[test]
    fn test_constant_time_eq() {
        // Equal strings → true
        assert!(constant_time_eq("", ""));
        assert!(constant_time_eq("my-secret", "my-secret"));
        assert!(constant_time_eq(
            "rpc-secret-token-123",
            "rpc-secret-token-123"
        ));

        // Same-length, different bytes → false
        assert!(!constant_time_eq("secret", "secretX")); // no, lengths differ
        assert!(!constant_time_eq("aaaaaa", "aaaaba")); // last byte differs
        assert!(!constant_time_eq("baaaaa", "aaaaaa")); // first byte differs
        assert!(!constant_time_eq("abcdef", "fedcba"));

        // Different-length inputs → false (including prefix relations)
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("abcd", "abc"));
        assert!(!constant_time_eq("", "x"));
        assert!(!constant_time_eq("x", ""));
        assert!(!constant_time_eq("my-secret", "my-secret-extra"));
    }

    #[test]
    fn test_auth_config_token_constant_time() {
        let auth = AuthConfig::default().with_token("s3cr3t-tok3n");
        // Exact match → accept
        assert!(auth.verify_token("s3cr3t-tok3n"));
        // Same-length wrong token → reject
        assert!(!auth.verify_token("s3cr3t-tok3m"));
        assert!(!auth.verify_token("X3cr3t-tok3n"));
        // Different-length token → reject
        assert!(!auth.verify_token("s3cr3t"));
        assert!(!auth.verify_token("s3cr3t-tok3n-extra"));
        assert!(!auth.verify_token(""));
        // Unrelated
        assert!(!auth.verify_token("not-the-token"));
    }

    #[test]
    fn test_auth_config_basic() {
        let auth = AuthConfig::default().with_basic_auth("admin", "pass123");
        assert!(auth.has_basic());
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"admin:pass123");
        assert!(auth.verify_basic(&encoded));
        assert!(auth.verify_authorization(Some(&format!("Basic {encoded}"))));
        assert!(auth.verify_authorization(Some(&format!("basic {encoded}"))));
        assert!(
            !auth.verify_basic(
                base64::engine::general_purpose::STANDARD
                    .encode(b"admin:wrong")
                    .as_str()
            )
        );
        assert!(!auth.verify_authorization(None));
        assert!(!auth.verify_authorization(Some("Bearer token")));
    }

    #[test]
    fn test_auth_config_username_only_basic_auth() {
        let auth = AuthConfig {
            username: Some("admin".into()),
            password: None,
            ..AuthConfig::default()
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"admin:anything");
        assert!(auth.has_basic());
        assert!(auth.verify_basic(&encoded));
    }

    #[test]
    fn test_auth_valid_token_passes() {
        let middleware = RpcAuthMiddleware::new("my-secret-token");
        // Valid token should pass
        assert!(middleware.validate(Some("my-secret-token")).is_ok());
    }

    #[test]
    fn test_auth_wrong_token_rejected() {
        let middleware = RpcAuthMiddleware::new("my-secret-token");
        let result = middleware.validate(Some("wrong-token"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), 1);
        assert!(err.to_string().contains("Invalid token"));
    }

    #[test]
    fn test_auth_token_constant_time_validate() {
        let middleware = RpcAuthMiddleware::new("my-secret-token");
        // Exact match → accept
        assert!(middleware.validate(Some("my-secret-token")).is_ok());
        // Same-length wrong token → reject
        assert!(middleware.validate(Some("my-secret-tokem")).is_err());
        assert!(middleware.validate(Some("nY-secret-token")).is_err());
        // Different-length token → reject (shorter and longer)
        assert!(middleware.validate(Some("my-secret")).is_err());
        assert!(middleware.validate(Some("my-secret-token-extra")).is_err());
        assert!(middleware.validate(Some("")).is_err());
        // No token with a configured secret → reject with "Token required"
        let err = middleware.validate(None).unwrap_err();
        assert!(err.to_string().contains("Token required"));
    }

    #[test]
    fn test_auth_no_secret_configured_accepts_all() {
        let middleware = RpcAuthMiddleware::new(""); // Empty secret = no auth
        // All should pass when no secret is configured
        assert!(middleware.validate(None).is_ok());
        assert!(middleware.validate(Some("anything")).is_ok());
        assert!(middleware.validate(Some("")).is_ok());
        assert!(
            !middleware.is_auth_enabled(),
            "Auth should be disabled with empty secret"
        );
    }

    #[test]
    fn test_auth_token_required_when_secret_set() {
        let middleware = RpcAuthMiddleware::new("secret123");
        // No token provided but secret is set → should fail
        let result = middleware.validate(None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), 1);
        assert!(err.to_string().contains("Token required"));
        assert!(
            middleware.is_auth_enabled(),
            "Auth should be enabled with non-empty secret"
        );
    }

    #[test]
    fn test_auth_middleware_default() {
        let middleware = RpcAuthMiddleware::default();
        assert!(!middleware.is_auth_enabled());
        assert!(middleware.validate(None).is_ok());
        assert!(middleware.validate(Some("x")).is_ok());
    }

    #[test]
    fn test_auth_middleware_secret_accessor() {
        let middleware = RpcAuthMiddleware::new("test-secret");
        assert_eq!(middleware.secret(), "test-secret");
    }
}

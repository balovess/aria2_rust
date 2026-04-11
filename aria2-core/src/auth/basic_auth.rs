//! Basic HTTP Authentication (RFC 7617)
//!
//! Implements the simple username/password authentication scheme where credentials
//! are Base64-encoded and sent in the Authorization header.
//!
//! # Security Considerations
//! - Basic auth transmits credentials in an easily decoded format (Base64)
//! - Always use HTTPS to prevent credential interception
//! - This module provides an `https_only` option to enforce secure connections
//! - All credentials are wrapped in `Secret<T>` for automatic memory zeroing

use base64::Engine;
use std::fmt;
use url::Url;

use crate::auth::digest_auth::{AuthChallenge, AuthProvider, AuthScheme, Secret};
use crate::error::{Aria2Error, Result};

/// Basic authentication provider implementing RFC 7617.
///
/// Provides simple username/password authentication with optional HTTPS enforcement
/// to prevent credential leakage over insecure connections.
///
/// # Example
/// ```rust
/// use aria2_core::auth::basic_auth::BasicAuthProvider;
///
/// let provider = BasicAuthProvider::new(
///     "alice".to_string(),
///     "secret-password".to_string(),
///     true, // Enforce HTTPS
/// );
/// ```
#[derive(Clone)]
pub struct BasicAuthProvider {
    /// Username for authentication (stored securely)
    username: Secret<String>,
    /// Password for authentication (stored securely)
    password: Secret<String>,
    /// When true, only allow authentication over HTTPS connections
    https_only: bool,
}

impl BasicAuthProvider {
    /// Creates a new Basic authentication provider.
    ///
    /// # Arguments
    /// * `username` - The username for authentication
    /// * `password` - The password for authentication
    /// * `https_only` - If true, rejects non-HTTPS URLs for security
    ///
    /// # Security Note
    /// Setting `https_only` to `true` is strongly recommended to prevent
    /// credential interception. Basic auth credentials are trivially decoded
    /// from Base64, making HTTPS essential for security.
    pub fn new(username: String, password: String, https_only: bool) -> Self {
        BasicAuthProvider {
            username: Secret::new(username),
            password: Secret::new(password),
            https_only,
        }
    }

    /// Returns whether this provider enforces HTTPS-only mode.
    pub fn is_https_only(&self) -> bool {
        self.https_only
    }

    /// Builds an Authorization header value with URL validation.
    ///
    /// This is the recommended method as it validates the URL scheme before
    /// generating credentials when `https_only` is enabled.
    ///
    /// # Arguments
    /// * `challenge` - The authentication challenge (realm info)
    /// * `url` - The request URL (used for scheme validation)
    ///
    /// # Returns
    /// Complete Authorization header value or error if:
    /// - URL scheme is not HTTPS and `https_only` is enabled
    /// - Base64 encoding fails (unlikely but possible)
    ///
    /// # Errors
    /// Returns `Aria2Error::Parse` if the URL cannot be parsed
    /// Returns `Aria2Error::DownloadFailed` if HTTPS is required but not used
    pub fn build_authorization_header_with_url(
        &self,
        challenge: &AuthChallenge,
        url: &str,
    ) -> Result<String> {
        if self.https_only {
            let parsed_url =
                Url::parse(url).map_err(|e| Aria2Error::Parse(format!("Invalid URL: {}", e)))?;

            if parsed_url.scheme() != "https" {
                return Err(Aria2Error::DownloadFailed(
                    "Basic authentication requires HTTPS connection to prevent credential \
                     interception"
                        .to_string(),
                ));
            }
        }

        self.build_authorization_header(challenge)
    }

    /// Generates the Base64-encoded credential string.
    ///
    /// Creates "username:password" and encodes it using standard Base64.
    fn encode_credentials(&self) -> String {
        let creds = format!(
            "{}:{}",
            self.username.expose_secret(),
            self.password.expose_secret()
        );
        base64::engine::general_purpose::STANDARD.encode(creds)
    }
}

impl fmt::Debug for BasicAuthProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BasicAuthProvider")
            .field("username", &self.username)
            .field("password", &self.password)
            .field("https_only", &self.https_only)
            .finish()
    }
}

#[async_trait::async_trait]
impl AuthProvider for BasicAuthProvider {
    fn scheme(&self) -> AuthScheme {
        AuthScheme::Basic
    }

    /// Builds a Basic Authorization header value.
    ///
    /// Format: `Basic Base64(username:password)`
    ///
    /// # Arguments
    /// * `_challenge` - The authentication challenge (not used for Basic auth,
    ///   but included for interface consistency)
    ///
    /// # Returns
    /// Complete Authorization header starting with "Basic "
    fn build_authorization_header(&self, _challenge: &AuthChallenge) -> Result<String> {
        let encoded = self.encode_credentials();
        Ok(format!("Basic {}", encoded))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_auth_construction() {
        let provider =
            BasicAuthProvider::new("testuser".to_string(), "testpass".to_string(), false);

        assert!(!provider.is_https_only());
        assert_eq!(provider.scheme(), AuthScheme::Basic);
    }

    #[test]
    fn test_basic_auth_with_https_enforcement() {
        let provider =
            BasicAuthProvider::new("secure_user".to_string(), "secure_pass".to_string(), true);

        assert!(provider.is_https_only());
    }

    #[test]
    fn test_basic_auth_debug_masking() {
        let provider =
            BasicAuthProvider::new("admin".to_string(), "super-secret".to_string(), true);

        let debug_output = format!("{:?}", provider);

        // Verify that sensitive data is masked
        assert!(debug_output.contains("Secret(***)"));
        assert!(!debug_output.contains("super-secret"));
        assert!(!debug_output.contains("admin"));
    }

    #[test]
    fn test_basic_auth_clone() {
        let provider =
            BasicAuthProvider::new("original".to_string(), "password".to_string(), false);

        let cloned = provider.clone();

        // Both should work independently
        let challenge = AuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "Test".to_string(),
            nonce: None,
            opaque: None,
            qop: None,
            stale: false,
        };

        let result1 = provider.build_authorization_header(&challenge).unwrap();
        let result2 = cloned.build_authorization_header(&challenge).unwrap();

        assert_eq!(result1, result2);
    }

    #[test]
    fn test_basic_auth_special_characters_in_credentials() {
        // Test with special characters that might affect Base64 encoding
        let provider = BasicAuthProvider::new(
            "user@domain.com".to_string(),
            "p@ss:w0rd!".to_string(),
            false,
        );

        let challenge = AuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "Test".to_string(),
            nonce: None,
            opaque: None,
            qop: None,
            stale: false,
        };

        let result = provider.build_authorization_header(&challenge).unwrap();
        assert!(result.starts_with("Basic "));

        // Verify we can decode it back
        let encoded_part = result.trim_start_matches("Basic ");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded_part)
            .unwrap();
        let decoded_str = String::from_utf8(decoded).unwrap();
        assert_eq!(decoded_str, "user@domain.com:p@ss:w0rd!");
    }

    #[test]
    fn test_basic_auth_empty_password() {
        // Edge case: empty password should still work
        let provider = BasicAuthProvider::new("user".to_string(), "".to_string(), false);

        let challenge = AuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "Test".to_string(),
            nonce: None,
            opaque: None,
            qop: None,
            stale: false,
        };

        let result = provider.build_authorization_header(&challenge).unwrap();
        // Should be Base64 of "user:"
        assert_eq!(result, "Basic dXNlcjo=");
    }

    #[test]
    fn test_basic_auth_unicode_credentials() {
        // Test Unicode characters in credentials
        let provider = BasicAuthProvider::new("用户名".to_string(), "密码".to_string(), false);

        let challenge = AuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "Test".to_string(),
            nonce: None,
            opaque: None,
            qop: None,
            stale: false,
        };

        let result = provider.build_authorization_header(&challenge).unwrap();
        assert!(result.starts_with("Basic "));

        // Verify encoding/decoding round-trip
        let encoded_part = result.trim_start_matches("Basic ");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded_part)
            .unwrap();
        let decoded_str = String::from_utf8(decoded).unwrap();
        assert_eq!(decoded_str, "用户名:密码");
    }
}

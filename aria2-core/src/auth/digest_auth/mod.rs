//! Digest HTTP Authentication (RFC 7616)
//!
//! Implements challenge-response authentication with support for:
//! - MD5, SHA-256, and SHA-512/256 hash algorithms
//! - Quality of Protection (qop) with "auth" and "auth-int" modes
//! - Automatic nonce counter incrementing for replay attack prevention
//! - Stale nonce detection and re-authentication

mod challenge;
mod qop;

#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::{Aria2Error, Result};

pub use challenge::parse_www_authenticate;

/// Wrapper type for sensitive data that automatically zeros memory on drop.
///
/// This provides a simple alternative to the `secrecy` crate, using `zeroize`
/// to ensure credentials are securely erased from memory when no longer needed.
pub struct Secret<T: zeroize::Zeroize>(pub(crate) T);

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
    /// MD5 algorithm (less secure but widely supported)
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
    /// Username for authentication (secret-wrapped for zeroize on drop)
    pub(crate) username: Secret<String>,
    /// Password for authentication (secret-wrapped for zeroize on drop)
    pub(crate) password: Secret<String>,
    /// Atomic nonce counter for replay attack prevention
    pub(crate) nc_count: AtomicU32,
    /// Configured hash algorithm
    pub(crate) algorithm: DigestAlgorithm,
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

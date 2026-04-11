//! HTTP Authentication module
//!
//! Provides implementations for HTTP authentication schemes:
//! - **Basic** (RFC 7617) - Simple username/password authentication
//! - **Digest** (RFC 7616) - Challenge-response authentication with MD5/SHA256/SHA512
//!
//! # Security Features
//! - All credentials are wrapped in `Secret<T>` for automatic memory zeroing on drop
//! - HTTPS-only enforcement option to prevent credential leakage over insecure connections
//! - Atomic nonce counters to prevent replay attacks in Digest authentication
//!
//! # Examples
//!
//! ## Basic Authentication
//!
//! ```rust,no_run
//! use aria2_core::auth::basic_auth::BasicAuthProvider;
//! use aria2_core::auth::digest_auth::{AuthChallenge, AuthProvider, AuthScheme};
//!
//! let provider = BasicAuthProvider::new("admin".to_string(), "secret123".to_string(), true);
//! assert_eq!(provider.scheme(), AuthScheme::Basic);
//! let challenge = AuthChallenge { scheme: AuthScheme::Basic, realm: String::new(), nonce: None, opaque: None, qop: None, stale: false };
//! let header = provider.build_authorization_header(&challenge).unwrap();
//! // header contains Base64-encoded credentials
//! ```
//!
//! ## Digest Authentication
//!
//! ```rust,no_run
//! use aria2_core::auth::digest_auth::{DigestAuthProvider, AuthProvider, AuthScheme, DigestAlgorithm, parse_www_authenticate};
//!
//! let provider = DigestAuthProvider::new("alice".to_string(), "p@ssw0rd".to_string(), None);
//! let scheme = provider.scheme();
//! assert!(matches!(scheme, AuthScheme::Digest { .. }));
//!
//! // Parse server challenge from WWW-Authenticate header
//! let challenge = parse_www_authenticate(
//!     "Digest realm=\"secure\", nonce=\"abc123\", qop=\"auth\", algorithm=MD5"
//! ).unwrap();
//!
//! let _response = provider.build_authorization_header_with_method(&challenge, "GET", "/protected", None);
//! // response contains the computed Authorization header value
//! ```
//!
//! ## Credential Store Integration
//!
//! ```rust,no_run
//! use aria2_core::auth::credential_store::{CredentialStore, PasswordEntry};
//!
//! let store = CredentialStore::new();
//! store.store("api.example.com", "user", b"pass");
//!
//! if let Some(cred) = store.get("api.example.com") {
//!     println!("Found credentials for user: {}", cred.username);
//! }
//! ```

pub mod basic_auth;
pub mod credential_store;
pub mod digest_auth;

pub use digest_auth::{AuthChallenge, AuthProvider, AuthScheme};

//! HTTP authentication challenge handler.
//!
//! Bridges the gap between the skip_response module's `AuthChallenge` detection
//! and the auth module's credential resolution. When a 401/407 response is
//! received, this module:
//!
//! 1. Resolves credentials from `AuthConfigFactory` (URL-embedded, BasicCred,
//!    Netrc, CLI options)
//! 2. Activates BasicCred for future requests (matching C++ `activateBasicCred`)
//! 3. Computes the `Authorization` / `Proxy-Authorization` header value for
//!    Basic and Digest schemes
//! 4. Returns a retry-ready result that the caller can use to rebuild the
//!    request with the correct auth headers
//!
//! # C++ Mapping
//!
//! This module combines the logic of:
//! - `HttpSkipResponseCommand::processResponse()` — 401 → activateBasicCred → retry
//! - `AuthConfigFactory::activateBasicCred()` — resolve credentials and create BasicCred
//! - `HttpRequest::createRequest()` — add `Authorization: Basic ...` header
//!
//! # Digest Auth (surpassing C++)
//!
//! The original C++ aria2 only supports Basic authentication. This Rust port
//! additionally supports Digest auth (RFC 7616) by using the existing
//! `DigestAuthProvider` to compute the response hash and build the full
//! `Authorization: Digest ...` header value.

mod basic_auth;
mod digest_auth;
mod handler;
#[cfg(test)]
mod tests;
mod types;

// Re-export all public items so that external consumers can access them
// at the same paths as before (e.g. `auth_challenge_handler::AuthChallengeResult`).
pub use handler::handle_auth_challenge;
pub use types::AuthChallengeResult;

//! Proxy authentication header builders.
//!
//! Constructs Proxy-Authorization headers for Basic and Digest schemes,
//! and selects the appropriate scheme from Proxy-Authenticate challenges.

use base64::{Engine, engine::general_purpose};
use tracing::{debug, warn};

use crate::http::digest_auth::{DigestAuthChallenge, DigestAuthResponse};
use crate::http::header_processor::HttpResponseHead;

/// Build a Proxy-Authorization: Basic ... header value.
pub(crate) fn proxy_basic_auth(username: &str, password: &str) -> String {
    let credentials = format!("{}:{}", username, password);
    let encoded = general_purpose::STANDARD.encode(credentials.as_bytes());
    format!("Basic {}", encoded)
}

/// Build a Proxy-Authorization: Digest ... header value using the existing
/// [DigestAuthResponse] infrastructure.
pub(crate) fn proxy_digest_auth(
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    challenge: &DigestAuthChallenge,
    nc: u32,
) -> String {
    let response = DigestAuthResponse::compute(username, password, method, uri, challenge, nc);
    response.to_header_value()
}

/// Parse Proxy-Authenticate headers from a response and build the
/// appropriate Proxy-Authorization header value for retry.
///
/// Returns None if no supported scheme is found or if credentials are missing.
pub(crate) fn build_proxy_auth_header(
    head: &HttpResponseHead,
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    nc: u32,
) -> Option<String> {
    // Check for Digest first (more secure), then Basic
    for (_, value) in head.iter_headers() {
        if value.starts_with("Digest ") || value.starts_with("digest ") {
            if let Ok(challenge) = DigestAuthChallenge::parse(value) {
                let auth = proxy_digest_auth(username, password, method, uri, &challenge, nc);
                debug!(
                    "Using Digest proxy authentication for realm='{}'",
                    challenge.realm
                );
                return Some(auth);
            }
        }
    }

    // Fall back to Basic
    for (_, value) in head.iter_headers() {
        if value.starts_with("Basic ") || value.starts_with("basic ") {
            let auth = proxy_basic_auth(username, password);
            debug!("Using Basic proxy authentication");
            return Some(auth);
        }
    }

    // If Proxy-Authenticate exists but scheme is unknown, try Basic as last resort
    if head.header("proxy-authenticate").is_some() {
        warn!("Unknown Proxy-Authenticate scheme, falling back to Basic");
        return Some(proxy_basic_auth(username, password));
    }

    None
}

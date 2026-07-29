//! WWW-Authenticate header parsing (RFC 7616)
//!
//! Handles parsing of server authentication challenges from WWW-Authenticate
//! headers into structured AuthChallenge values.

use crate::error::{Aria2Error, Result};

use super::{AuthChallenge, AuthScheme, DigestAlgorithm};

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
    let re = regex::Regex::new(r#"(?i)(\w+)\s*=\s*"([^"]*)"|(\w+)\s*=\s*(\w+)"#)
        .map_err(|e| Aria2Error::Parse(format!("Failed to compile regex: {}", e)))?;

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

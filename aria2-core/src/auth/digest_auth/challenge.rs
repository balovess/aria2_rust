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

    let (scheme, _) = header
        .split_once(char::is_whitespace)
        .unwrap_or((header, ""));
    if scheme.eq_ignore_ascii_case("Basic") {
        return Ok(AuthChallenge {
            scheme: AuthScheme::Basic,
            realm: String::new(),
            nonce: None,
            opaque: None,
            qop: None,
            stale: false,
        });
    }

    if !scheme.eq_ignore_ascii_case("Digest") {
        return Err(Aria2Error::Parse(format!(
            "Unsupported auth scheme: {}",
            header
        )));
    }
    let parameters = aria2_protocol::http::digest::parse_challenge_parameters(header)
        .map_err(|error| Aria2Error::Parse(error.to_string()))?;
    let algorithm = match parameters
        .algorithm
        .as_deref()
        .unwrap_or("MD5")
        .to_ascii_uppercase()
        .as_str()
    {
        "SHA-256" | "SHA-256-SESS" => DigestAlgorithm::Sha256,
        "SHA-512-256" | "SHA-512-256-SESS" => DigestAlgorithm::Sha512_256,
        _ => DigestAlgorithm::Md5,
    };

    Ok(AuthChallenge {
        scheme: AuthScheme::Digest { algorithm },
        realm: parameters.realm.unwrap_or_default(),
        nonce: parameters.nonce,
        opaque: parameters.opaque,
        qop: parameters.qop,
        stale: parameters.stale,
    })
}

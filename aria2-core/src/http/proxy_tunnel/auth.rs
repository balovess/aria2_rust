//! Proxy authentication helpers (Basic and Digest)

use tracing::debug;

use crate::http::digest_auth::DigestAuthChallenge;
use crate::http::request_response::basic_auth;

use super::HttpProxyTunnelConfig;

// ---------------------------------------------------------------------------
// MD5 helper
// ---------------------------------------------------------------------------

/// Compute MD5 hex digest of the input string.
pub(crate) fn md5_hex(input: &str) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Pre-emptive Basic auth
// ---------------------------------------------------------------------------

/// Return pre-emptive Basic auth header if credentials are configured.
pub(crate) fn maybe_preemptive_basic_auth(config: &HttpProxyTunnelConfig) -> Option<String> {
    let username = config.username.as_deref()?;
    if username.is_empty() {
        return None;
    }
    Some(basic_auth(
        username,
        config.password.as_deref().unwrap_or(""),
    ))
}

// ---------------------------------------------------------------------------
// Digest auth
// ---------------------------------------------------------------------------

/// Build a Digest Proxy-Authorization header value.
pub(crate) fn build_digest_auth_header(
    username: &str,
    password: &str,
    challenge_header: &str,
    uri: &str,
) -> String {
    let challenge = match DigestAuthChallenge::parse(challenge_header) {
        Ok(c) => c,
        Err(e) => {
            debug!("Failed to parse Digest challenge, fallback to Basic: {}", e);
            return basic_auth(username, password);
        }
    };
    let ha1 = md5_hex(&format!("{}:{}:{}", username, challenge.realm, password));
    let ha1 = if challenge.algorithm.eq_ignore_ascii_case("MD5-sess") {
        md5_hex(&format!("{}:{}:{}", ha1, challenge.nonce, "00000001"))
    } else {
        ha1
    };
    let ha2 = md5_hex(&format!("CONNECT:{}", uri));
    let qop_value = challenge.qop.as_deref().unwrap_or("");
    let cnonce = "aria2rustcnonce";
    let response_hash = if qop_value.is_empty() {
        md5_hex(&format!("{}:{}:{}", ha1, challenge.nonce, ha2))
    } else {
        md5_hex(&format!(
            "{}:{}:00000001:{}:{}:{}",
            ha1, challenge.nonce, cnonce, qop_value, ha2
        ))
    };
    let mut header = format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\"",
        username, challenge.realm, challenge.nonce, uri, response_hash
    );
    if let Some(ref opaque) = challenge.opaque {
        header.push_str(&format!(", opaque=\"{}\"", opaque));
    }
    if !qop_value.is_empty() {
        header.push_str(&format!(
            ", qop={}, nc=00000001, cnonce=\"{}\"",
            qop_value, cnonce
        ));
    }
    header.push_str(&format!(", algorithm={}", challenge.algorithm));
    header
}

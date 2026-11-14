//! Proxy authentication header builders.
//!
//! Constructs Proxy-Authorization headers for Basic and Digest schemes,
//! and selects the appropriate scheme from Proxy-Authenticate challenges.

use crate::http::header_processor::HttpResponseHead;

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
    let challenges = head
        .iter_headers()
        .filter(|(name, _)| name.eq_ignore_ascii_case("proxy-authenticate"))
        .map(|(_, value)| value)
        .collect::<Vec<_>>();
    aria2_protocol::http::auth::proxy_authorization(
        &challenges,
        username,
        password,
        method,
        uri,
        nc,
    )
}

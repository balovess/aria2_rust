//! Main entry point for HTTP authentication challenge handling.
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

use tracing::warn;

use crate::http::auth::{AuthConfigFactory, AuthResolveOptions};
use crate::http::request_response::HttpMethod;
use crate::http::skip_response::{AuthScheme, HttpAuthChallenge};

use super::basic_auth::handle_basic_challenge;
use super::digest_auth::handle_digest_challenge;
use super::types::AuthChallengeResult;

/// Process an HTTP authentication challenge and resolve credentials.
///
/// This is the main entry point that bridges challenge detection and
/// credential resolution. It matches the C++ flow:
///
/// ```text
/// 401 received
///   → check http_auth_challenge enabled
///   → check !authenticationUsed (prevents infinite retry loop)
///   → activateBasicCred(host, port, path, options)
///   → if success: prepareForRetry(0)
///   → if fail: throw DL_ABORT_EX2(EX_AUTH_FAILED)
/// ```
///
/// # Arguments
///
/// * `challenge` - The parsed `HttpAuthChallenge` from the skip_response handler
/// * `auth_factory` - The `AuthConfigFactory` for credential resolution
/// * `url` - The request URL (for extracting host/port/path and URI for Digest)
/// * `auth_opts` - Per-request auth resolution options
/// * `method` - The HTTP method of the original request (needed for Digest HA2)
/// * `authentication_used` - Whether auth was already attempted (prevents loops)
/// * `nc` - Nonce count for Digest auth (incremented per request with same nonce)
///
/// # Returns
///
/// An `AuthChallengeResult` indicating what the caller should do next.
#[allow(clippy::too_many_arguments)]
pub fn handle_auth_challenge(
    challenge: &HttpAuthChallenge,
    auth_factory: &mut AuthConfigFactory,
    url: &url::Url,
    auth_opts: &AuthResolveOptions,
    method: HttpMethod,
    authentication_used: bool,
    nc: u32,
) -> AuthChallengeResult {
    let status_code = if challenge.is_proxy { 407 } else { 401 };

    // Prevent infinite retry loops: if we already tried auth and still got 401,
    // the credentials are wrong — don't retry.
    if authentication_used {
        return AuthChallengeResult::NoCredentials {
            status_code,
            message: "Authentication failed: credentials already tried".to_string(),
        };
    }

    let host = url.host_str().unwrap_or("");
    let port = url
        .port_or_known_default()
        .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
    let path = url.path();

    match challenge.scheme {
        AuthScheme::Basic => handle_basic_challenge(
            auth_factory,
            host,
            port,
            path,
            auth_opts,
            challenge.is_proxy,
        ),
        AuthScheme::Digest => handle_digest_challenge(
            auth_factory,
            host,
            port,
            path,
            auth_opts,
            url,
            method,
            challenge,
            nc,
        ),
        AuthScheme::Ntlm => {
            warn!("NTLM authentication is not supported");
            AuthChallengeResult::UnsupportedScheme {
                scheme: "NTLM".to_string(),
                status_code,
            }
        }
        AuthScheme::Negotiate => {
            warn!("SPNEGO/Kerberos Negotiate authentication is not supported");
            AuthChallengeResult::UnsupportedScheme {
                scheme: "Negotiate".to_string(),
                status_code,
            }
        }
    }
}

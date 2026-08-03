//! Digest authentication challenge handler.
//!
//! Surpasses the original C++ aria2 which only supports Basic auth.
//! Implements RFC 7616 Digest auth using the existing `DigestAuthProvider`.

use tracing::{info, warn};

use crate::http::auth::{AuthConfigFactory, AuthResolveOptions};
use crate::http::digest_auth::DigestAuthResponse;
use crate::http::request_response::HttpMethod;
use crate::http::skip_response::HttpAuthChallenge;

use super::types::AuthChallengeResult;

/// Handle a Digest authentication challenge by resolving credentials and
/// computing the Digest response hash.
///
/// This surpasses the original C++ aria2 which only supports Basic auth.
pub fn handle_digest_challenge(
    auth_factory: &mut AuthConfigFactory,
    host: &str,
    port: u16,
    path: &str,
    auth_opts: &AuthResolveOptions,
    url: &url::Url,
    method: HttpMethod,
    challenge: &HttpAuthChallenge,
    nc: u32,
) -> AuthChallengeResult {
    let digest_challenge = match &challenge.digest_challenge {
        Some(dc) => dc,
        None => {
            warn!("Digest challenge received but parsing failed — missing challenge parameters");
            return AuthChallengeResult::NoCredentials {
                status_code: 401,
                message: "Digest challenge parsing failed".to_string(),
            };
        }
    };

    // Resolve credentials (try challenge mode first, then non-challenge).
    // If the host:port:path combination is invalid for URL construction,
    // fall back to a placeholder that will still allow credential resolution.
    let mut url_for_resolve = url::Url::parse(&format!("http://{}:{}{}", host, port, path))
        .unwrap_or_else(|_| {
            url::Url::parse("http://localhost/").expect("fallback URL must be valid")
        });

    // Copy username/password from original URL if present
    if !url.username().is_empty() {
        let _ = url_for_resolve.set_username(url.username());
    }
    if let Some(pwd) = url.password() {
        let _ = url_for_resolve.set_password(Some(pwd));
    }

    let auth_config = {
        // First try challenge mode (which checks BasicCred cache)
        let mut opts_challenge = auth_opts.clone();
        opts_challenge.http_auth_challenge = true;
        auth_factory.resolve(&url_for_resolve, false, &opts_challenge)
    }
    .or_else(|| {
        // Fall back to non-challenge mode (Netrc / CLI options)
        let mut opts_no_challenge = auth_opts.clone();
        opts_no_challenge.http_auth_challenge = false;
        auth_factory.resolve(&url_for_resolve, false, &opts_no_challenge)
    });

    let ac = match auth_config {
        Some(ac) => ac,
        None => {
            warn!(
                "No credentials available for Digest auth at {}:{}{}",
                host, port, path
            );
            return AuthChallengeResult::NoCredentials {
                status_code: 401,
                message: "No credentials available for Digest authentication".to_string(),
            };
        }
    };

    // Compute the Digest auth response
    let method_str = match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Head => "HEAD",
        HttpMethod::Put => "PUT",
        HttpMethod::Delete => "DELETE",
    };

    // Build the URI path for Digest computation (path + query)
    let uri_for_digest = match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    };

    let response = DigestAuthResponse::compute(
        ac.user(),
        ac.password(),
        method_str,
        &uri_for_digest,
        digest_challenge,
        nc,
    );

    let auth_header = response.to_header_value();
    info!(
        "Computed Digest auth for {}:{}{} (user={}, nc={})",
        host,
        port,
        path,
        ac.user(),
        nc
    );

    AuthChallengeResult::RetryWithAuth {
        authorization_header: auth_header,
        is_proxy: challenge.is_proxy,
    }
}

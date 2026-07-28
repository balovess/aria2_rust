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

use tracing::{info, warn};

use crate::http::auth::{AuthConfigFactory, AuthResolveOptions};
use crate::http::digest_auth::DigestAuthResponse;
use crate::http::request_response::{basic_auth, HttpMethod};
use crate::http::skip_response::{AuthScheme, HttpAuthChallenge};

// ---------------------------------------------------------------------------
// AuthChallengeResult
// ---------------------------------------------------------------------------

/// Outcome of processing an authentication challenge.
///
/// Returned by [`handle_auth_challenge`] to tell the caller what to do next.
#[derive(Debug)]
pub enum AuthChallengeResult {
    /// Authentication resolved successfully; the caller should retry the
    /// request with the provided `Authorization` header value.
    RetryWithAuth {
        /// Complete `Authorization` header value (e.g. `Basic dXNlcjpwYXNz`
        /// or `Digest username="...", realm="...", ...`).
        authorization_header: String,
        /// Whether this is a proxy auth challenge (407).
        is_proxy: bool,
    },

    /// No credentials could be resolved; the download should fail with
    /// an authentication error.
    NoCredentials {
        /// HTTP status code (401 or 407).
        status_code: u16,
        /// Human-readable error description.
        message: String,
    },

    /// The authentication scheme is not supported (e.g. NTLM, Negotiate).
    /// The download should fail with an appropriate error.
    UnsupportedScheme {
        /// The unsupported scheme name.
        scheme: String,
        /// HTTP status code (401 or 407).
        status_code: u16,
    },
}

// ---------------------------------------------------------------------------
// handle_auth_challenge
// ---------------------------------------------------------------------------

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
    let port = url.port_or_known_default().unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
    let path = url.path();

    match challenge.scheme {
        AuthScheme::Basic => {
            handle_basic_challenge(auth_factory, host, port, path, auth_opts, challenge.is_proxy)
        }
        AuthScheme::Digest => {
            handle_digest_challenge(
                auth_factory,
                host,
                port,
                path,
                auth_opts,
                url,
                method,
                challenge,
                nc,
            )
        }
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

// ---------------------------------------------------------------------------
// Basic Auth Handler
// ---------------------------------------------------------------------------

/// Handle a Basic authentication challenge by activating BasicCred and
/// resolving credentials.
///
/// Mirrors C++ `AuthConfigFactory::activateBasicCred()` + retry logic.
fn handle_basic_challenge(
    auth_factory: &mut AuthConfigFactory,
    host: &str,
    port: u16,
    path: &str,
    auth_opts: &AuthResolveOptions,
    is_proxy: bool,
) -> AuthChallengeResult {
    // Activate BasicCred — this either activates an existing entry or
    // creates a new one from the credential resolution chain.
    let activated = auth_factory.activate_basic_cred(host, port, path, auth_opts);

    if !activated {
        warn!(
            "Cannot activate BasicCred for {}:{}{} — no credentials found",
            host, port, path
        );
        return AuthChallengeResult::NoCredentials {
            status_code: if is_proxy { 407 } else { 401 },
            message: "No credentials available for Basic authentication".to_string(),
        };
    }

    // Resolve the activated credentials
    let url_for_resolve = url::Url::parse(&format!("http://{}:{}{}", host, port, path))
        .unwrap_or_else(|_| url::Url::parse("http://localhost/").unwrap());

    // In challenge mode, resolve returns the activated BasicCred
    let mut opts_with_challenge = auth_opts.clone();
    opts_with_challenge.http_auth_challenge = true;

    let auth_config = auth_factory.resolve(&url_for_resolve, false, &opts_with_challenge);

    match auth_config {
        Some(ac) => {
            let auth_header = basic_auth(ac.user(), ac.password());
            info!(
                "Resolved Basic auth for {}:{}{} (user={})",
                host, port, path, ac.user()
            );

            if is_proxy {
                // For proxy auth, the header name is Proxy-Authorization,
                // but the value format is the same.
                AuthChallengeResult::RetryWithAuth {
                    authorization_header: auth_header,
                    is_proxy: true,
                }
            } else {
                AuthChallengeResult::RetryWithAuth {
                    authorization_header: auth_header,
                    is_proxy: false,
                }
            }
        }
        None => {
            warn!(
                "BasicCred activated but resolve returned no credentials for {}:{}{}",
                host, port, path
            );
            AuthChallengeResult::NoCredentials {
                status_code: if is_proxy { 407 } else { 401 },
                message: "No credentials available after BasicCred activation".to_string(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Digest Auth Handler (surpassing C++)
// ---------------------------------------------------------------------------

/// Handle a Digest authentication challenge by resolving credentials and
/// computing the Digest response hash.
///
/// This surpasses the original C++ aria2 which only supports Basic auth.
fn handle_digest_challenge(
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

    // Resolve credentials (try challenge mode first, then non-challenge)
    let mut url_for_resolve = url::Url::parse(&format!("http://{}:{}{}", host, port, path))
        .unwrap_or_else(|_| url::Url::parse("http://localhost/").unwrap());

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
    }.or_else(|| {
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
        host, port, path, ac.user(), nc
    );

    AuthChallengeResult::RetryWithAuth {
        authorization_header: auth_header,
        is_proxy: challenge.is_proxy,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use crate::http::skip_response::HttpAuthChallenge;

    fn make_url(url_str: &str) -> url::Url {
        url::Url::parse(url_str).unwrap()
    }

    fn default_auth_opts() -> AuthResolveOptions {
        AuthResolveOptions::default()
    }

    fn auth_opts_with_challenge() -> AuthResolveOptions {
        AuthResolveOptions {
            http_auth_challenge: true,
            ..AuthResolveOptions::default()
        }
    }

    fn auth_opts_with_cli_creds() -> AuthResolveOptions {
        AuthResolveOptions {
            http_user: Some("testuser".to_string()),
            http_passwd: Some("testpass".to_string()),
            ..AuthResolveOptions::default()
        }
    }

    // --- Basic Auth Tests ---

    #[test]
    fn test_basic_auth_with_cli_credentials() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/protected");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "TestRealm".to_string(),
            is_proxy: false,
            digest_challenge: None,
        };
        // Need both http_auth_challenge=true AND CLI credentials
        let opts = AuthResolveOptions {
            http_auth_challenge: true,
            http_user: Some("testuser".to_string()),
            http_passwd: Some("testpass".to_string()),
            ..AuthResolveOptions::default()
        };

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::RetryWithAuth {
                authorization_header,
                is_proxy,
            } => {
                assert!(authorization_header.starts_with("Basic "));
                assert!(!is_proxy);
                // Verify the Base64-decoded value is "testuser:testpass"
                let encoded = authorization_header.strip_prefix("Basic ").unwrap();
                let decoded = String::from_utf8(
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(decoded, "testuser:testpass");
            }
            _ => panic!("Expected RetryWithAuth, got {:?}", result),
        }
    }

    #[test]
    fn test_basic_auth_with_url_credentials() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://user:pass@example.com/protected");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "SecureArea".to_string(),
            is_proxy: false,
            digest_challenge: None,
        };
        let opts = auth_opts_with_challenge();

        // Simulate real flow: first resolve credentials from the URL,
        // which populates the BasicCred cache (this is what happens when
        // the initial request is built in the C++ flow).
        let _initial_auth = factory.resolve(&url, false, &opts);

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::RetryWithAuth {
                authorization_header,
                ..
            } => {
                assert!(authorization_header.starts_with("Basic "));
                let encoded = authorization_header.strip_prefix("Basic ").unwrap();
                let decoded = String::from_utf8(
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(decoded, "user:pass");
            }
            _ => panic!("Expected RetryWithAuth, got {:?}", result),
        }
    }

    #[test]
    fn test_basic_auth_no_credentials_fails() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/protected");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "SecureArea".to_string(),
            is_proxy: false,
            digest_challenge: None,
        };
        // No CLI creds, no URL creds, no Netrc
        let opts = auth_opts_with_challenge();

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::NoCredentials { status_code, .. } => {
                assert_eq!(status_code, 401);
            }
            _ => panic!("Expected NoCredentials, got {:?}", result),
        }
    }

    #[test]
    fn test_basic_auth_already_used_prevents_loop() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://user:pass@example.com/protected");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "SecureArea".to_string(),
            is_proxy: false,
            digest_challenge: None,
        };
        let opts = auth_opts_with_challenge();

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            true, // Already tried auth
            1,
        );

        match result {
            AuthChallengeResult::NoCredentials { message, .. } => {
                assert!(message.contains("already tried"));
            }
            _ => panic!("Expected NoCredentials, got {:?}", result),
        }
    }

    #[test]
    fn test_proxy_auth_challenge() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/file");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Basic,
            realm: "ProxyRealm".to_string(),
            is_proxy: true,
            digest_challenge: None,
        };
        let opts = AuthResolveOptions {
            http_auth_challenge: true,
            http_user: Some("proxyuser".to_string()),
            http_passwd: Some("proxypass".to_string()),
            ..AuthResolveOptions::default()
        };

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::RetryWithAuth { is_proxy, .. } => {
                assert!(is_proxy);
            }
            _ => panic!("Expected RetryWithAuth with is_proxy=true, got {:?}", result),
        }
    }

    // --- Digest Auth Tests ---

    #[test]
    fn test_digest_auth_with_cli_credentials() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/protected/data");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Digest,
            realm: "Downloads".to_string(),
            is_proxy: false,
            digest_challenge: Some(
                crate::http::digest_auth::DigestAuthChallenge::parse(
                    r#"Digest realm="Downloads", nonce="abc123def", qop="auth", algorithm="MD5""#,
                )
                .unwrap(),
            ),
        };
        let opts = auth_opts_with_cli_creds();

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::RetryWithAuth {
                authorization_header,
                is_proxy,
            } => {
                assert!(authorization_header.starts_with("Digest "));
                assert!(authorization_header.contains(r#"username="testuser""#));
                assert!(authorization_header.contains(r#"realm="Downloads""#));
                assert!(authorization_header.contains(r#"nonce="abc123def""#));
                assert!(authorization_header.contains(r#"uri="/protected/data""#));
                assert!(authorization_header.contains("response="));
                assert!(!is_proxy);
            }
            _ => panic!("Expected RetryWithAuth, got {:?}", result),
        }
    }

    #[test]
    fn test_digest_auth_no_credentials_fails() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/protected");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Digest,
            realm: "Secure".to_string(),
            is_proxy: false,
            digest_challenge: Some(
                crate::http::digest_auth::DigestAuthChallenge::parse(
                    r#"Digest realm="Secure", nonce="xyz""#,
                )
                .unwrap(),
            ),
        };
        let opts = default_auth_opts();

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &opts,
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::NoCredentials { status_code, .. } => {
                assert_eq!(status_code, 401);
            }
            _ => panic!("Expected NoCredentials, got {:?}", result),
        }
    }

    // --- Unsupported Scheme Tests ---

    #[test]
    fn test_ntlm_unsupported() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/file");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Ntlm,
            realm: String::new(),
            is_proxy: false,
            digest_challenge: None,
        };

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &default_auth_opts(),
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::UnsupportedScheme { scheme, status_code } => {
                assert_eq!(scheme, "NTLM");
                assert_eq!(status_code, 401);
            }
            _ => panic!("Expected UnsupportedScheme, got {:?}", result),
        }
    }

    #[test]
    fn test_negotiate_unsupported() {
        let mut factory = AuthConfigFactory::new();
        let url = make_url("http://example.com/file");
        let challenge = HttpAuthChallenge {
            scheme: AuthScheme::Negotiate,
            realm: String::new(),
            is_proxy: false,
            digest_challenge: None,
        };

        let result = handle_auth_challenge(
            &challenge,
            &mut factory,
            &url,
            &default_auth_opts(),
            HttpMethod::Get,
            false,
            1,
        );

        match result {
            AuthChallengeResult::UnsupportedScheme { scheme, .. } => {
                assert_eq!(scheme, "Negotiate");
            }
            _ => panic!("Expected UnsupportedScheme, got {:?}", result),
        }
    }
}

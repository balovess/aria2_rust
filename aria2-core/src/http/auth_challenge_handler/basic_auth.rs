//! Basic authentication challenge handler.
//!
//! Mirrors C++ `AuthConfigFactory::activateBasicCred()` + retry logic.

use tracing::{info, warn};

use crate::http::auth::{AuthConfigFactory, AuthResolveOptions};
use crate::http::request_response::basic_auth;

use super::types::AuthChallengeResult;

/// Handle a Basic authentication challenge by activating BasicCred and
/// resolving credentials.
///
/// Mirrors C++ `AuthConfigFactory::activateBasicCred()` + retry logic.
pub fn handle_basic_challenge(
    auth_factory: &mut AuthConfigFactory,
    host: &str,
    port: u16,
    path: &str,
    auth_opts: &AuthResolveOptions,
    is_proxy: bool,
) -> AuthChallengeResult {
    // Activate BasicCred — this either activates an existing entry or
    // creates a new one from the credential resolution chain.
    let activated = is_proxy || auth_factory.activate_basic_cred(host, port, path, auth_opts);

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

    // Resolve the activated credentials.
    // If the host:port:path combination is invalid for URL construction,
    // fall back to a placeholder that will still allow credential resolution.
    let url_for_resolve = url::Url::parse(&format!("http://{}:{}{}", host, port, path))
        .unwrap_or_else(|_| {
            url::Url::parse("http://localhost/").expect("fallback URL must be valid")
        });

    // In challenge mode, resolve returns the activated BasicCred
    let mut opts_with_challenge = auth_opts.clone();
    opts_with_challenge.http_auth_challenge = true;

    let auth_config = if is_proxy {
        auth_factory.resolve_proxy(auth_opts)
    } else {
        auth_factory.resolve(&url_for_resolve, false, &opts_with_challenge)
    };

    match auth_config {
        Some(ac) => {
            let auth_header = basic_auth(ac.user(), ac.password());
            info!(
                "Resolved Basic auth for {}:{}{} (user={})",
                host,
                port,
                path,
                ac.user()
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

// Auth challenge retry logic for sequential downloads.
//
// Mirrors the C++ `HttpSkipResponseCommand::processResponse()` flow
// for 401/407 responses: resolve credentials, retry with Authorization.

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::resume_helper::ResumeState;
use crate::http::auth::{AuthConfigFactory, AuthResolveOptions};
use crate::http::auth_challenge_handler::{self, AuthChallengeResult};
use crate::http::skip_response::AuthScheme;
use crate::util::rwlock_ext::RwLockRecover;

use super::SequentialDownloader;

impl SequentialDownloader {
    /// Attempt an authentication retry when a 401/407 response is received.
    ///
    /// Returns `Some(Ok(()))` if the auth retry succeeded and the download
    /// completed. Returns `Some(Err(...))` if the auth retry failed.
    /// Returns `None` if auth retry is not possible (no credentials,
    /// unsupported scheme, auth already used).
    ///
    /// This mirrors the C++ `HttpSkipResponseCommand::processResponse()` flow
    /// for the 401 case: activate BasicCred → prepareForRetry.
    pub(in crate::engine::sequential_download) async fn try_auth_retry(
        &mut self,
        response: &reqwest::Response,
        uri: &str,
        url_parsed: &Option<reqwest::Url>,
        status_code: u16,
        authentication_used: bool,
        resume_state: &ResumeState,
    ) -> Option<Result<()>> {
        let is_proxy = status_code == 407;
        let header_name = if is_proxy {
            "proxy-authenticate"
        } else {
            "www-authenticate"
        };

        // Extract the auth challenge header
        let auth_header = response
            .headers()
            .get(header_name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        // Parse the auth scheme
        let scheme = match &auth_header {
            Some(h) => AuthScheme::from_header(h),
            None => {
                // No auth header — if http_auth_challenge is enabled,
                // treat as Basic challenge (matches C++ behavior)
                if !authentication_used {
                    Some(AuthScheme::Basic)
                } else {
                    None
                }
            }
        };

        let scheme = match scheme {
            Some(s) => s,
            None => {
                tracing::warn!(
                    status_code,
                    "Auth challenge received but no supported scheme found"
                );
                return None;
            }
        };

        // Build HttpAuthChallenge from the response
        let challenge = crate::http::skip_response::HttpAuthChallenge {
            scheme: scheme.clone(),
            realm: auth_header
                .as_deref()
                .map(crate::http::skip_response::HttpSkipResponseHandler::extract_realm)
                .unwrap_or_default(),
            is_proxy,
            digest_challenge: if scheme == AuthScheme::Digest {
                auth_header
                    .as_deref()
                    .and_then(|h| crate::http::digest_auth::DigestAuthChallenge::parse(h).ok())
            } else {
                None
            },
        };

        // Resolve auth options from the RequestGroup
        let auth_opts = {
            let g = self.group.recover();
            let opts = g.options();
            AuthResolveOptions {
                http_auth_challenge: opts.http_auth_challenge,
                no_netrc: opts.no_netrc,
                http_user: opts.http_user.clone(),
                http_passwd: opts.http_passwd.clone(),
                ftp_user: opts.ftp_user.clone(),
                ftp_passwd: opts.ftp_passwd.clone(),
            }
        };

        // Only attempt auth if http_auth_challenge is enabled (matches C++ behavior)
        if !auth_opts.http_auth_challenge && scheme != AuthScheme::Digest {
            tracing::debug!(
                status_code,
                "Auth challenge received but http_auth_challenge not enabled"
            );
            return None;
        }

        // Use the URL for credential resolution
        let url = match url_parsed {
            Some(u) => url::Url::parse(u.as_ref()).ok()?,
            None => return None,
        };

        // Resolve credentials via AuthConfigFactory
        let mut auth_factory = AuthConfigFactory::new();
        // Pre-populate from netrc if available
        {
            let g = self.group.recover();
            let opts = g.options();
            if let Some(ref netrc_path) = opts.netrc_path
                && let Err(e) = auth_factory.load_netrc_file(std::path::Path::new(netrc_path))
            {
                tracing::debug!("Failed to load netrc file {}: {}", netrc_path, e);
            }
        }

        let result = auth_challenge_handler::handle_auth_challenge(
            &challenge,
            &mut auth_factory,
            &url,
            &auth_opts,
            crate::http::request_response::HttpMethod::Get,
            authentication_used,
            1, // nc
        );

        match result {
            AuthChallengeResult::RetryWithAuth {
                authorization_header,
                is_proxy,
            } => {
                // Build the retry request with Authorization header
                // Re-apply the same Range header if we had a resume
                let retry_request = if let Some(range_header) =
                    crate::filesystem::resume_helper::ResumeHelper::build_range_header(resume_state)
                {
                    tracing::debug!("Auth retry: re-applying Range header: {}", range_header);
                    self.client.get(uri).header("Range", range_header)
                } else {
                    self.client.get(uri)
                };

                // Add the Authorization or Proxy-Authorization header
                let header_name = if is_proxy {
                    "Proxy-Authorization"
                } else {
                    "Authorization"
                };
                let cookie_header = url_parsed
                    .as_ref()
                    .map(|url| self.cookie_helper.build_cookie_header_from_url(url));
                let retry_request = self.request_policy.apply(
                    retry_request,
                    cookie_header.as_deref().filter(|value| !value.is_empty()),
                    &[(header_name.to_string(), authorization_header.clone())],
                );

                tracing::info!(
                    status_code,
                    scheme = ?scheme,
                    "Retrying HTTP request with {} authentication",
                    if is_proxy { "proxy " } else { "" }
                );

                // Send the retry request
                let retry_response = match retry_request.send().await {
                    Ok(r) => r,
                    Err(e) => {
                        return Some(Err(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: format!("Auth retry request failed: {}", e),
                            },
                        )));
                    }
                };

                self.cookie_helper
                    .extract_and_store_cookies(uri, &retry_response);

                let retry_status = retry_response.status();
                let mut effective_resume_state = resume_state.clone();
                if retry_status.as_u16() == 200 && Self::resume_requested(&effective_resume_state) {
                    effective_resume_state = match self
                        .resume_state_after_failed_request(&effective_resume_state)
                        .await
                    {
                        Ok(state) => state,
                        Err(error) => return Some(Err(error)),
                    };
                }
                if retry_status.is_success() || retry_status.as_u16() == 206 {
                    // Auth retry succeeded — proceed with the download using
                    // the retry response
                    return Some(
                        self.download_response_body(retry_response, uri, &effective_resume_state)
                            .await,
                    );
                }

                // Auth retry still failed
                if retry_status.as_u16() == 401 || retry_status.as_u16() == 407 {
                    tracing::warn!(
                        status_code = retry_status.as_u16(),
                        "Auth retry still failed — credentials may be incorrect"
                    );
                    return Some(Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                        "Authentication failed".to_string(),
                    ))));
                }

                Some(Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                    format!("HTTP error after auth retry: {}", retry_status),
                ))))
            }
            AuthChallengeResult::NoCredentials {
                status_code,
                message,
            } => {
                tracing::warn!(
                    status_code,
                    "Auth challenge but no credentials: {}",
                    message
                );
                None // Fall through to normal error handling
            }
            AuthChallengeResult::UnsupportedScheme {
                scheme,
                status_code,
            } => {
                tracing::warn!(status_code, scheme, "Unsupported authentication scheme");
                None // Fall through to normal error handling
            }
        }
    }
}

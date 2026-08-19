// Auth challenge retry logic for sequential downloads.
//
// Mirrors the C++ `HttpSkipResponseCommand::processResponse()` flow
// for 401/407 responses: resolve credentials, retry with Authorization.

use crate::constants;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::resume_helper::ResumeState;
use crate::http::auth::{AuthConfigFactory, AuthResolveOptions};
use crate::http::auth_challenge_handler::{self, AuthChallengeResult};
use crate::http::response::is_redirect_status;
use crate::http::skip_response::AuthScheme;
use crate::util::rwlock_ext::RwLockRecover;

use super::SequentialDownloader;

fn classify_auth_retry_status(status_code: u16) -> Aria2Error {
    if status_code >= 500 || constants::RETRYABLE_HTTP_CODES.contains(&status_code) {
        Aria2Error::Recoverable(RecoverableError::ServerError { code: status_code })
    } else {
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
            message: format!("HTTP error after auth retry: {status_code}"),
        })
    }
}

pub(super) enum AuthRetryOutcome {
    Completed(Result<()>),
    Redirect(String),
}

pub(super) struct AuthRetryRequest<'a> {
    pub response: &'a reqwest::Response,
    pub uri: &'a str,
    pub url_parsed: &'a Option<reqwest::Url>,
    pub status_code: u16,
    pub authentication_used: bool,
    pub resume_state: &'a ResumeState,
    pub auth_factory: &'a mut AuthConfigFactory,
    pub auth_opts: &'a AuthResolveOptions,
}

impl SequentialDownloader {
    pub(super) fn auth_context(&self, scheme: &str) -> (AuthConfigFactory, AuthResolveOptions) {
        let (auth_opts, netrc_path) = {
            let group = self.group.recover();
            let options = group.options();
            let (proxy_user, proxy_passwd) = options.proxy_credentials_for_scheme(scheme);
            (
                AuthResolveOptions {
                    http_auth_challenge: options.http_auth_challenge,
                    no_netrc: options.no_netrc,
                    http_user: options.http_user.clone(),
                    http_passwd: options.http_passwd.clone(),
                    ftp_user: options.ftp_user.clone(),
                    ftp_passwd: options.ftp_passwd.clone(),
                    proxy_user,
                    proxy_passwd,
                },
                options.netrc_path.clone(),
            )
        };

        let mut auth_factory = AuthConfigFactory::new();
        if let Some(netrc_path) = netrc_path
            && let Err(error) = auth_factory.load_netrc_file(std::path::Path::new(&netrc_path))
        {
            tracing::debug!("Failed to load netrc file {}: {}", netrc_path, error);
        }
        (auth_factory, auth_opts)
    }

    /// Attempt an authentication retry when a 401/407 response is received.
    ///
    /// Returns a completed result or a redirect target after the auth retry.
    /// Returns `Some(Err(...))` if the auth retry failed.
    /// Returns `None` if auth retry is not possible (no credentials,
    /// unsupported scheme, auth already used).
    ///
    /// This mirrors the C++ `HttpSkipResponseCommand::processResponse()` flow
    /// for the 401 case: activate BasicCred → prepareForRetry.
    pub(in crate::engine::sequential_download) async fn try_auth_retry(
        &mut self,
        request: AuthRetryRequest<'_>,
    ) -> Option<Result<AuthRetryOutcome>> {
        let AuthRetryRequest {
            response,
            uri,
            url_parsed,
            status_code,
            authentication_used,
            resume_state,
            auth_factory,
            auth_opts,
        } = request;
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

        // Origin 401 retries are opt-in. Proxy credentials are an explicit
        // proxy contract and must work independently of that origin option.
        if !is_proxy && !auth_opts.http_auth_challenge {
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

        let result = auth_challenge_handler::handle_auth_challenge(
            &challenge,
            auth_factory,
            &url,
            auth_opts,
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
                    return Some(Ok(AuthRetryOutcome::Completed(
                        self.download_response_body(retry_response, uri, &effective_resume_state)
                            .await,
                    )));
                }

                if is_redirect_status(retry_status.as_u16()) {
                    let location = retry_response
                        .headers()
                        .get("location")
                        .and_then(|value| value.to_str().ok());
                    let location = match location {
                        Some(location) => location,
                        None => {
                            return Some(Err(Aria2Error::Recoverable(
                                RecoverableError::HttpProtocolError {
                                    message: format!(
                                        "HTTP {} redirect without Location header",
                                        retry_status.as_u16()
                                    ),
                                },
                            )));
                        }
                    };
                    let base_url = match url_parsed {
                        Some(url) => url,
                        None => {
                            return Some(Err(Aria2Error::Recoverable(
                                RecoverableError::HttpProtocolError {
                                    message: format!(
                                        "Cannot resolve HTTP redirect from invalid URL: {uri}"
                                    ),
                                },
                            )));
                        }
                    };
                    let target_url = match base_url.join(location) {
                        Ok(url) => url.to_string(),
                        Err(error) => {
                            return Some(Err(Aria2Error::Recoverable(
                                RecoverableError::HttpProtocolError {
                                    message: format!(
                                        "Failed to resolve redirect URL '{location}': {error}"
                                    ),
                                },
                            )));
                        }
                    };
                    return Some(Ok(AuthRetryOutcome::Redirect(target_url)));
                }

                // Auth retry still failed
                if retry_status.as_u16() == 401 || retry_status.as_u16() == 407 {
                    tracing::warn!(
                        status_code = retry_status.as_u16(),
                        "Auth retry still failed — credentials may be incorrect"
                    );
                    return Some(Err(Aria2Error::Recoverable(
                        RecoverableError::HttpAuthFailed {
                            message: format!("Authentication failed: HTTP {}", retry_status),
                        },
                    )));
                }

                let error = if retry_status.as_u16() == 404 {
                    self.classify_file_not_found()
                } else {
                    classify_auth_retry_status(retry_status.as_u16())
                };
                Some(Err(error))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_auth_retry_5xx_as_server_error() {
        assert!(matches!(
            classify_auth_retry_status(503),
            Aria2Error::Recoverable(RecoverableError::ServerError { code: 503 })
        ));
    }

    #[test]
    fn classifies_configured_4xx_transients_as_server_errors() {
        for status_code in [408, 429] {
            assert!(matches!(
                classify_auth_retry_status(status_code),
                Aria2Error::Recoverable(RecoverableError::ServerError { code })
                    if code == status_code
            ));
        }
    }

    #[test]
    fn classifies_auth_retry_4xx_as_protocol_error() {
        assert!(matches!(
            classify_auth_retry_status(404),
            Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message })
                if message.contains("404")
        ));
    }
}

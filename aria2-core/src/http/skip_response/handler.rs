//! HTTP skip response handler — the main processing logic.
//!
//! Contains `HttpSkipResponseHandler` which consumes and discards HTTP response
//! bodies via NullSinkFilter, then classifies the response as a redirect,
//! authentication challenge, error, or consumed-ok.

use url::Url;

use crate::error::Aria2Error;
use crate::http::digest_auth::DigestAuthChallenge;
use crate::http::request_response::HttpMethod;
use crate::http::stream_filter::{AutoFilterSelector, NullSinkFilter, process_filters};

use super::types::{
    AuthScheme, HttpAuthChallenge, HttpRedirectInfo, RedirectType, SkipResponseResult,
};

// Re-export HttpResponse from aria2-protocol so handler methods can use it
// without callers needing to import two different crates.
pub use aria2_protocol::http::response::HttpResponse;

/// Handler for HTTP responses that need to be skipped (non-file-data responses).
///
/// This is the Rust equivalent of C++ `HttpSkipResponseCommand`.
/// It consumes the response body (discarding it via NullSinkFilter),
/// then classifies the response as a redirect, auth challenge, error,
/// or consumed-ok.
///
/// # Configuration
///
/// The handler is configured via builder-style methods:
/// - [`with_http_auth_challenge`](Self::with_http_auth_challenge): enable 401 basic auth retry
/// - [`with_max_file_not_found`](Self::with_max_file_not_found): 404 retry count
/// - [`with_retry_wait`](Self::with_retry_wait): 502/503 retry threshold
///
/// # Example
///
/// ```rust,ignore
/// use aria2_core::http::skip_response::{HttpSkipResponseHandler, MAX_REDIRECT_COUNT};
///
/// let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT)
///     .with_http_auth_challenge(true)
///     .with_max_file_not_found(3)
///     .with_retry_wait(5);
///
/// let result = handler.handle(&response, HttpMethod::Get, &url, 0)?;
/// ```
pub struct HttpSkipResponseHandler {
    /// Maximum number of redirects allowed
    max_redirects: u32,
    /// Whether HTTP auth challenge is enabled (maps to C++ PREF_HTTP_AUTH_CHALLENGE)
    http_auth_challenge_enabled: bool,
    /// Maximum "file not found" (404) retries before aborting;
    /// 0 means abort immediately on 404
    max_file_not_found: u32,
    /// Retry wait in seconds; 0 means do not retry 502/503
    retry_wait_secs: u64,
}

impl HttpSkipResponseHandler {
    /// Create a new handler with the given max-redirect limit.
    pub fn new(max_redirects: u32) -> Self {
        Self {
            max_redirects,
            http_auth_challenge_enabled: false,
            max_file_not_found: 0,
            retry_wait_secs: 0,
        }
    }

    /// Set whether HTTP auth challenge handling is enabled.
    ///
    /// When enabled, a 401 response without a WWW-Authenticate header
    /// will still return `AuthChallenge` (Basic) instead of `FatalError`,
    /// allowing the caller to retry with credentials (matches C++ behavior
    /// where `activateBasicCred` is tried).
    pub fn with_http_auth_challenge(mut self, enabled: bool) -> Self {
        self.http_auth_challenge_enabled = enabled;
        self
    }

    /// Set the maximum number of 404 retries before aborting.
    ///
    /// When > 0, 404 responses produce `RetryableError`; when 0 (default),
    /// 404 responses produce `FatalError` (matches C++ `max-file-not-found` option).
    pub fn with_max_file_not_found(mut self, count: u32) -> Self {
        self.max_file_not_found = count;
        self
    }

    /// Set the retry wait in seconds. When > 0, 502/503 errors are retryable.
    ///
    /// Matches C++ `retry-wait` option: hammering a busy server is not ideal,
    /// so 502/503 are only retryable when there is a non-zero wait between attempts.
    pub fn with_retry_wait(mut self, secs: u64) -> Self {
        self.retry_wait_secs = secs;
        self
    }

    /// Consume and discard the response body, then classify the response.
    ///
    /// This mirrors the C++ `executeInternal()` + `processResponse()` flow:
    /// 1. Consume the body through stream filters (handling chunked/content-encoding)
    ///    using NullSinkFilter to discard data.
    /// 2. Classify the status code and return the appropriate `SkipResponseResult`.
    ///
    /// # Arguments
    /// * `response` - The parsed HTTP response (body may already be populated)
    /// * `request_method` - The HTTP method of the original request
    /// * `current_url` - The URL of the current request (for resolving relative redirects)
    /// * `redirect_count` - Number of redirects already followed
    ///
    /// # Returns
    /// A `SkipResponseResult` indicating what action the caller should take.
    pub fn handle(
        &self,
        response: &HttpResponse,
        request_method: HttpMethod,
        current_url: &Url,
        redirect_count: u32,
    ) -> Result<SkipResponseResult, Aria2Error> {
        // Consume the body through stream filters (discard output)
        self.consume_body(response)?;

        // Process the response status
        self.process_response(response, request_method, current_url, redirect_count)
    }

    /// Consume the response body using NullSinkFilter + encoding filters.
    ///
    /// Equivalent to the C++ body-consumption loop in `executeInternal()`.
    /// The body data is read, decoded (if chunked/content-encoded), and
    /// discarded via NullSinkFilter.
    fn consume_body(&self, response: &HttpResponse) -> Result<(), Aria2Error> {
        // If body is empty, nothing to consume
        if response.body.is_empty() {
            tracing::trace!("Empty response body, nothing to consume");
            return Ok(());
        }

        // Build decoding pipeline: encoding filters -> null sink
        let content_encoding = response.header("Content-Encoding").map(|s| s.as_str());
        let transfer_encoding = response.header("Transfer-Encoding").map(|s| s.as_str());

        let mut filters = AutoFilterSelector::select_filters(content_encoding, transfer_encoding);
        filters.push(Box::new(NullSinkFilter::new()));

        // Process the entire body through the filter chain (data is discarded)
        let _ = process_filters(&mut filters, &response.body)?;

        tracing::debug!(
            "Consumed {} bytes of response body (discarded)",
            response.body.len()
        );
        Ok(())
    }

    /// Classify the HTTP response and determine the appropriate action.
    ///
    /// Mirrors C++ `HttpSkipResponseCommand::processResponse()`.
    fn process_response(
        &self,
        response: &HttpResponse,
        request_method: HttpMethod,
        current_url: &Url,
        redirect_count: u32,
    ) -> Result<SkipResponseResult, Aria2Error> {
        let status = response.status_code;

        // 3xx redirect handling
        if response.is_redirect() {
            return self.handle_redirect(response, request_method, current_url, redirect_count);
        }

        // 4xx/5xx error handling
        if status >= 400 {
            return Ok(self.handle_error_status(response, status));
        }

        // 1xx informational or 2xx success that ended up here — just consumed
        tracing::debug!("Response status {} consumed, no special action", status);
        Ok(SkipResponseResult::Consumed)
    }

    /// Handle 3xx redirect responses.
    ///
    /// Extracts the Location header, resolves it against the current URL,
    /// determines method change rules per RFC 7231, and checks redirect limits.
    ///
    /// # 300 Multiple Choices
    ///
    /// Per C++ aria2 and aria2-next, 300 is treated as a redirect when a
    /// Location header is present. If no Location header exists, it is
    /// treated as a fatal error (the server is offering a choice but not
    /// telling us which one to pick).
    fn handle_redirect(
        &self,
        response: &HttpResponse,
        request_method: HttpMethod,
        current_url: &Url,
        redirect_count: u32,
    ) -> Result<SkipResponseResult, Aria2Error> {
        // Check redirect count limit
        if redirect_count >= self.max_redirects {
            return Err(Aria2Error::Network(format!(
                "Too many redirects: count={}",
                redirect_count
            )));
        }

        // Extract Location header
        // For 300 Multiple Choices, Location may be absent → treat as error
        let location = response.location();
        let location = match location {
            Some(loc) => loc,
            None => {
                if response.status_code == 300 {
                    // 300 without Location: server is offering choices but
                    // not telling us which one. Treat as a fatal error.
                    tracing::warn!("300 Multiple Choices without Location header");
                    return Ok(SkipResponseResult::FatalError {
                        status_code: 300,
                        message: "Multiple choices without Location header".to_string(),
                    });
                }
                return Err(Aria2Error::Parse(
                    "Redirect response missing Location header".to_string(),
                ));
            }
        };

        // Resolve relative URLs against the current URL
        let target_url = current_url.join(location).map_err(|e| {
            Aria2Error::Parse(format!(
                "Failed to resolve redirect URL '{}': {}",
                location, e
            ))
        })?;

        // Determine redirect type and method change rules per RFC 7231
        let redirect_type = match response.status_code {
            300 | 301 => RedirectType::Permanent,
            303 => RedirectType::SeeOther,
            307 | 308 => RedirectType::PreserveMethod,
            _ => RedirectType::Temporary, // 302 and other 3xx
        };

        let change_method = redirect_type.should_change_method(request_method);
        let new_count = redirect_count + 1;

        tracing::info!(
            "HTTP redirect: {} -> {} (type={:?}, change_method={}, count={}/{})",
            current_url,
            target_url,
            redirect_type,
            change_method,
            new_count,
            self.max_redirects
        );

        Ok(SkipResponseResult::Redirect(HttpRedirectInfo {
            target_url,
            change_method,
            redirect_type,
            redirect_count: new_count,
        }))
    }

    /// Handle 4xx/5xx error status codes.
    ///
    /// Mirrors the C++ switch statement in `processResponse()` plus
    /// aria2-next enhancements:
    /// - 401: auth challenge (if enabled) or fatal
    /// - 407: proxy auth challenge
    /// - 404: retryable or fatal depending on max_file_not_found
    /// - 413: retryable when Retry-After header is present (aria2-next)
    /// - 502/503: retryable if retry_wait > 0, else fatal
    /// - 504: always retryable (gateway timeout)
    /// - other 4xx/5xx: fatal
    fn handle_error_status(&self, response: &HttpResponse, status: u16) -> SkipResponseResult {
        match status {
            401 => self.handle_auth_challenge(response, false),
            407 => self.handle_auth_challenge(response, true),
            404 => {
                if self.max_file_not_found == 0 {
                    SkipResponseResult::FatalError {
                        status_code: status,
                        message: "Resource not found".to_string(),
                    }
                } else {
                    SkipResponseResult::RetryableError {
                        status_code: status,
                        message: "Resource not found".to_string(),
                    }
                }
            }
            413 => {
                // Request Entity Too Large — retryable if server specifies
                // Retry-After, matching aria2-next behavior for "Payload Too
                // Large" with backoff. Without Retry-After, this is fatal.
                if response.header("Retry-After").is_some() {
                    tracing::info!("413 Request Entity Too Large with Retry-After, will retry");
                    SkipResponseResult::RetryableError {
                        status_code: status,
                        message: "Request entity too large, retrying after backoff".to_string(),
                    }
                } else {
                    SkipResponseResult::FatalError {
                        status_code: status,
                        message: "Request entity too large".to_string(),
                    }
                }
            }
            502 | 503 => {
                // Only retry if retry_wait > 0; hammering a busy server is not ideal
                if self.retry_wait_secs > 0 {
                    SkipResponseResult::RetryableError {
                        status_code: status,
                        message: format!("Server returned error: {}", status),
                    }
                } else {
                    SkipResponseResult::FatalError {
                        status_code: status,
                        message: format!("Server returned error: {}", status),
                    }
                }
            }
            504 => {
                // Gateway Timeout — always retryable
                SkipResponseResult::RetryableError {
                    status_code: status,
                    message: format!("Gateway timeout: {}", status),
                }
            }
            _ => SkipResponseResult::FatalError {
                status_code: status,
                message: format!("HTTP error: {}", status),
            },
        }
    }

    /// Parse authentication challenge from 401/407 response.
    fn handle_auth_challenge(&self, response: &HttpResponse, is_proxy: bool) -> SkipResponseResult {
        let header_name = if is_proxy {
            "Proxy-Authenticate"
        } else {
            "WWW-Authenticate"
        };

        let auth_header = response.header(header_name);

        match auth_header {
            Some(value) => {
                let scheme = AuthScheme::from_header(value);
                match scheme {
                    Some(AuthScheme::Digest) => {
                        let digest = DigestAuthChallenge::parse(value).ok();
                        let realm = digest.as_ref().map(|d| d.realm.clone()).unwrap_or_default();
                        SkipResponseResult::AuthChallenge(HttpAuthChallenge {
                            scheme: AuthScheme::Digest,
                            realm,
                            is_proxy,
                            digest_challenge: digest,
                        })
                    }
                    Some(scheme) => {
                        // Basic / Negotiate / NTLM — extract realm from header
                        let realm = Self::extract_realm(value);
                        SkipResponseResult::AuthChallenge(HttpAuthChallenge {
                            scheme,
                            realm,
                            is_proxy,
                            digest_challenge: None,
                        })
                    }
                    None => {
                        tracing::warn!("Unknown auth scheme in {} header: {}", header_name, value);
                        SkipResponseResult::FatalError {
                            status_code: if is_proxy { 407 } else { 401 },
                            message: "Authentication failed: unknown scheme".to_string(),
                        }
                    }
                }
            }
            None => {
                // No auth header — if auth challenge is enabled, this is retryable
                // (basic cred activation); otherwise fatal (matches C++ behavior)
                if self.http_auth_challenge_enabled && !is_proxy {
                    SkipResponseResult::AuthChallenge(HttpAuthChallenge {
                        scheme: AuthScheme::Basic,
                        realm: String::new(),
                        is_proxy: false,
                        digest_challenge: None,
                    })
                } else {
                    SkipResponseResult::FatalError {
                        status_code: if is_proxy { 407 } else { 401 },
                        message: "Authentication failed".to_string(),
                    }
                }
            }
        }
    }

    /// Extract the realm parameter from a WWW-Authenticate / Proxy-Authenticate header.
    pub(crate) fn extract_realm(header_value: &str) -> String {
        // Case-insensitive search for realm= while preserving original case for value
        let lower = header_value.to_lowercase();
        if let Some(idx) = lower.find("realm=") {
            let rest = &header_value[idx + 6..];
            let rest = rest.trim();
            if let Some(stripped) = rest.strip_prefix('"') {
                // Quoted value: find closing quote
                if let Some(end) = stripped.find('"') {
                    return stripped[..end].to_string();
                }
            } else {
                // Unquoted value — ends at whitespace or comma
                let end = rest
                    .find(|c: char| c.is_whitespace() || c == ',')
                    .unwrap_or(rest.len());
                return rest[..end].to_string();
            }
        }
        String::new()
    }
}

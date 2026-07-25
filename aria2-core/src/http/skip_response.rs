//! HTTP skip response handler module
//!
//! Handles HTTP responses that are NOT file data: redirects (3xx),
//! authentication challenges (401/407), and error responses (4xx/5xx).
//! Consumes and discards the response body using NullSinkFilter while
//! extracting relevant metadata for subsequent processing.
//!
//! Based on C++ aria2's `HttpSkipResponseCommand` which skips the response
//! body and processes redirect/auth/error status codes.

use url::Url;

use crate::error::Aria2Error;
use crate::http::digest_auth::DigestAuthChallenge;
use crate::http::request_response::HttpMethod;
use crate::http::stream_filter::{AutoFilterSelector, NullSinkFilter, process_filters};

/// Maximum redirect count before aborting (matches C++ `Request::MAX_REDIRECT`)
pub const MAX_REDIRECT_COUNT: u32 = 20;

/// Classification of redirect type, following RFC 7231 semantics.
///
/// Each variant corresponds to specific HTTP 3xx status codes and carries
/// the RFC-mandated method-change behavior:
///
/// | Variant        | Status codes | Method change rule           |
/// |----------------|-------------|------------------------------|
/// | Permanent      | 301         | POST -> GET, others preserved |
/// | Temporary      | 302         | POST -> GET (historical)      |
/// | SeeOther       | 303         | Always -> GET                 |
/// | PreserveMethod | 307, 308    | Method preserved              |
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RedirectType {
    /// 301 Moved Permanently — permanent redirect; POST changes to GET
    Permanent,
    /// 302 Found — historically changes POST to GET (browser convention)
    Temporary,
    /// 303 See Other — always converts method to GET (RFC 7231 Section 6.4.4)
    SeeOther,
    /// 307 Temporary Redirect / 308 Permanent Redirect — preserves method
    PreserveMethod,
}

impl RedirectType {
    /// Whether the HTTP method should change to GET on this redirect.
    ///
    /// Per RFC 7231:
    /// - 301/302: historically changed POST to GET (most clients do this)
    /// - 303: MUST change method to GET
    /// - 307/308: MUST preserve method
    pub fn should_change_method(&self, original_method: HttpMethod) -> bool {
        match self {
            RedirectType::SeeOther => true,
            RedirectType::Permanent | RedirectType::Temporary => {
                original_method == HttpMethod::Post
            }
            RedirectType::PreserveMethod => false,
        }
    }
}

/// Parsed redirect information extracted from a 3xx response.
#[derive(Debug, Clone)]
pub struct HttpRedirectInfo {
    /// The absolute target URL from the Location header
    pub target_url: Url,
    /// Whether the HTTP method should change (e.g., POST -> GET on 303)
    pub change_method: bool,
    /// Redirect type (permanent / temporary / see-other / preserve-method)
    pub redirect_type: RedirectType,
    /// Current redirect count after this redirect
    pub redirect_count: u32,
}

/// Authentication scheme parsed from WWW-Authenticate / Proxy-Authenticate header.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthScheme {
    /// HTTP Basic authentication (RFC 7617)
    Basic,
    /// HTTP Digest authentication (RFC 7616)
    Digest,
    /// SPNEGO/Kerberos Negotiate
    Negotiate,
    /// NTLM authentication
    Ntlm,
}

impl AuthScheme {
    /// Parse the scheme name from an authentication header value.
    ///
    /// # Examples
    /// - `"Basic realm=\"test\""` -> `AuthScheme::Basic`
    /// - `"Digest realm=\"test\", nonce=\"abc\""` -> `AuthScheme::Digest`
    pub fn from_header(value: &str) -> Option<Self> {
        let scheme = value.split_whitespace().next()?;
        match scheme.to_lowercase().as_str() {
            "basic" => Some(AuthScheme::Basic),
            "digest" => Some(AuthScheme::Digest),
            "negotiate" => Some(AuthScheme::Negotiate),
            "ntlm" => Some(AuthScheme::Ntlm),
            _ => None,
        }
    }
}

/// Parsed authentication challenge from 401/407 responses.
#[derive(Debug, Clone)]
pub struct HttpAuthChallenge {
    /// Authentication scheme (Basic, Digest, Negotiate, NTLM)
    pub scheme: AuthScheme,
    /// Realm identifying the protection space
    pub realm: String,
    /// Whether this is a proxy authentication challenge (407)
    pub is_proxy: bool,
    /// Parsed Digest challenge parameters (only set for Digest scheme)
    pub digest_challenge: Option<DigestAuthChallenge>,
}

/// The outcome of processing a skipped HTTP response.
#[derive(Debug)]
pub enum SkipResponseResult {
    /// A redirect was detected; caller should follow the new URL.
    Redirect(HttpRedirectInfo),
    /// An authentication challenge was detected; caller should retry with credentials.
    AuthChallenge(HttpAuthChallenge),
    /// A server error that may be retryable (502/503/504, or 404 with retries enabled).
    RetryableError {
        /// HTTP status code
        status_code: u16,
        /// Human-readable description
        message: String,
    },
    /// A fatal error (4xx except 401/404, or other non-retryable).
    FatalError {
        /// HTTP status code
        status_code: u16,
        /// Human-readable description
        message: String,
    },
    /// Response was consumed successfully; no special action needed.
    /// Used for 1xx informational and 2xx that somehow end up here,
    /// or when the caller simply wants to retry with the same request.
    Consumed,
}

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
            Aria2Error::Parse(format!("Failed to resolve redirect URL '{}': {}", location, e))
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
                    tracing::info!(
                        "413 Request Entity Too Large with Retry-After, will retry"
                    );
                    SkipResponseResult::RetryableError {
                        status_code: status,
                        message: "Request entity too large, retrying after backoff"
                            .to_string(),
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
                        let realm = digest
                            .as_ref()
                            .map(|d| d.realm.clone())
                            .unwrap_or_default();
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
                        tracing::warn!(
                            "Unknown auth scheme in {} header: {}",
                            header_name,
                            value
                        );
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
    fn extract_realm(header_value: &str) -> String {
        // Case-insensitive search for realm= while preserving original case for value
        let lower = header_value.to_lowercase();
        if let Some(idx) = lower.find("realm=") {
            let rest = &header_value[idx + 6..];
            let rest = rest.trim();
            if rest.starts_with('"') {
                // Quoted value: find closing quote
                if let Some(end) = rest[1..].find('"') {
                    return rest[1..end + 1].to_string();
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

// ---------------------------------------------------------------------------
// Re-export the HttpResponse from aria2-protocol so callers don't need
// to import two different crates for the same type.
// ---------------------------------------------------------------------------
pub use aria2_protocol::http::response::HttpResponse;

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create an HttpResponse with a given status and optional Location header
    fn make_response(status_code: u16, location: Option<&str>) -> HttpResponse {
        let mut resp = HttpResponse::new(status_code, "OK".to_string());
        if let Some(loc) = location {
            resp.headers
                .push(("Location".to_string(), loc.to_string()));
        }
        resp
    }

    /// Helper to create an HttpResponse with a WWW-Authenticate header
    fn make_auth_response(status_code: u16, www_authenticate: &str) -> HttpResponse {
        let mut resp = HttpResponse::new(status_code, "OK".to_string());
        resp.headers.push((
            "WWW-Authenticate".to_string(),
            www_authenticate.to_string(),
        ));
        resp
    }

    /// Helper to create an HttpResponse with a Proxy-Authenticate header
    fn make_proxy_auth_response(status_code: u16, proxy_authenticate: &str) -> HttpResponse {
        let mut resp = HttpResponse::new(status_code, "OK".to_string());
        resp.headers.push((
            "Proxy-Authenticate".to_string(),
            proxy_authenticate.to_string(),
        ));
        resp
    }

    // ==================== Redirect tests ====================

    #[test]
    fn test_redirect_301_permanent() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_response(301, Some("https://example.com/new"));
        let url = Url::parse("http://example.com/old").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::Redirect(info) => {
                assert_eq!(info.target_url.as_str(), "https://example.com/new");
                assert!(!info.change_method); // GET stays GET
                assert_eq!(info.redirect_type, RedirectType::Permanent);
                assert_eq!(info.redirect_count, 1);
            }
            _ => panic!("Expected Redirect result, got {:?}", result),
        }
    }

    #[test]
    fn test_redirect_301_post_changes_to_get() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_response(301, Some("/new"));
        let url = Url::parse("http://example.com/old").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Post, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::Redirect(info) => {
                assert!(info.change_method); // 301 changes POST to GET
                assert_eq!(info.redirect_type, RedirectType::Permanent);
            }
            _ => panic!("Expected Redirect result"),
        }
    }

    #[test]
    fn test_redirect_302_temporary_changes_post() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_response(302, Some("/other"));
        let url = Url::parse("http://example.com/start").unwrap();

        // 302 with POST -> change method
        let result = handler
            .handle(&resp, HttpMethod::Post, &url, 0)
            .unwrap();
        match result {
            SkipResponseResult::Redirect(info) => {
                assert!(info.change_method); // 302 historically changes POST->GET
                assert_eq!(info.redirect_type, RedirectType::Temporary);
            }
            _ => panic!("Expected Redirect result"),
        }

        // 302 with GET -> no change
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();
        match result {
            SkipResponseResult::Redirect(info) => {
                assert!(!info.change_method);
            }
            _ => panic!("Expected Redirect result"),
        }
    }

    #[test]
    fn test_redirect_303_always_changes_method() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_response(303, Some("/result"));
        let url = Url::parse("http://example.com/submit").unwrap();

        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();
        match result {
            SkipResponseResult::Redirect(info) => {
                assert!(info.change_method); // 303 always changes method
                assert_eq!(info.redirect_type, RedirectType::SeeOther);
            }
            _ => panic!("Expected Redirect result"),
        }
    }

    #[test]
    fn test_redirect_307_preserves_method() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_response(307, Some("/temp"));
        let url = Url::parse("http://example.com/submit").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Post, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::Redirect(info) => {
                assert!(!info.change_method); // 307 preserves POST
                assert_eq!(info.redirect_type, RedirectType::PreserveMethod);
            }
            _ => panic!("Expected Redirect result"),
        }
    }

    #[test]
    fn test_redirect_308_preserves_method() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_response(308, Some("/perm"));
        let url = Url::parse("http://example.com/submit").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Post, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::Redirect(info) => {
                assert!(!info.change_method); // 308 preserves POST
                assert_eq!(info.redirect_type, RedirectType::PreserveMethod);
            }
            _ => panic!("Expected Redirect result"),
        }
    }

    #[test]
    fn test_redirect_too_many() {
        let handler = HttpSkipResponseHandler::new(2);
        let resp = make_response(302, Some("/other"));
        let url = Url::parse("http://example.com/start").unwrap();
        let result = handler.handle(&resp, HttpMethod::Get, &url, 2);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Too many redirects"));
    }

    #[test]
    fn test_redirect_missing_location() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_response(302, None); // No Location header
        let url = Url::parse("http://example.com/start").unwrap();
        let result = handler.handle(&resp, HttpMethod::Get, &url, 0);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Location"));
    }

    #[test]
    fn test_redirect_relative_url_resolution() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_response(302, Some("/new-path?q=1"));
        let url = Url::parse("http://example.com/old-path").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::Redirect(info) => {
                assert_eq!(info.target_url.as_str(), "http://example.com/new-path?q=1");
            }
            _ => panic!("Expected Redirect result"),
        }
    }

    // ==================== Auth challenge tests ====================

    #[test]
    fn test_error_401_with_basic_challenge() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT)
            .with_http_auth_challenge(true);
        let resp = make_auth_response(401, r#"Basic realm="Secure Area""#);
        let url = Url::parse("http://example.com/protected").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::AuthChallenge(challenge) => {
                assert_eq!(challenge.scheme, AuthScheme::Basic);
                assert_eq!(challenge.realm, "Secure Area");
                assert!(!challenge.is_proxy);
            }
            _ => panic!("Expected AuthChallenge result, got {:?}", result),
        }
    }

    #[test]
    fn test_error_401_with_digest_challenge() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_auth_response(
            401,
            r#"Digest realm="Downloads", nonce="abc123", qop="auth", algorithm="MD5""#,
        );
        let url = Url::parse("http://example.com/protected").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::AuthChallenge(challenge) => {
                assert_eq!(challenge.scheme, AuthScheme::Digest);
                assert_eq!(challenge.realm, "Downloads");
                assert!(!challenge.is_proxy);
                let digest = challenge.digest_challenge.unwrap();
                assert_eq!(digest.nonce, "abc123");
                assert_eq!(digest.qop.as_deref(), Some("auth"));
            }
            _ => panic!("Expected AuthChallenge result"),
        }
    }

    #[test]
    fn test_error_407_proxy_auth() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_proxy_auth_response(407, r#"Basic realm="Proxy""#);
        let url = Url::parse("http://example.com/file").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::AuthChallenge(challenge) => {
                assert!(challenge.is_proxy);
                assert_eq!(challenge.scheme, AuthScheme::Basic);
                assert_eq!(challenge.realm, "Proxy");
            }
            _ => panic!("Expected AuthChallenge result"),
        }
    }

    #[test]
    fn test_error_401_no_auth_header_with_challenge_enabled() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT)
            .with_http_auth_challenge(true);
        let resp = HttpResponse::new(401, "Unauthorized".to_string());
        let url = Url::parse("http://example.com/protected").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::AuthChallenge(challenge) => {
                assert_eq!(challenge.scheme, AuthScheme::Basic);
                assert!(!challenge.is_proxy);
            }
            _ => panic!("Expected AuthChallenge when http_auth_challenge_enabled=true"),
        }
    }

    #[test]
    fn test_error_401_no_auth_header_without_challenge_enabled() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT)
            .with_http_auth_challenge(false);
        let resp = HttpResponse::new(401, "Unauthorized".to_string());
        let url = Url::parse("http://example.com/protected").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::FatalError { status_code, .. } => {
                assert_eq!(status_code, 401);
            }
            _ => panic!("Expected FatalError when http_auth_challenge_enabled=false"),
        }
    }

    // ==================== Error status tests ====================

    #[test]
    fn test_error_404_fatal_when_max_is_zero() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_max_file_not_found(0);
        let resp = make_response(404, None);
        let url = Url::parse("http://example.com/missing").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::FatalError { status_code, .. } => {
                assert_eq!(status_code, 404);
            }
            _ => panic!("Expected FatalError for 404 when max_file_not_found=0"),
        }
    }

    #[test]
    fn test_error_404_retryable_when_max_is_nonzero() {
        let handler =
            HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_max_file_not_found(3);
        let resp = make_response(404, None);
        let url = Url::parse("http://example.com/missing").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::RetryableError { status_code, .. } => {
                assert_eq!(status_code, 404);
            }
            _ => panic!("Expected RetryableError for 404 when max_file_not_found>0"),
        }
    }

    #[test]
    fn test_error_502_retryable_when_retry_wait_set() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_retry_wait(5);
        let resp = make_response(502, None);
        let url = Url::parse("http://example.com/file").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::RetryableError { status_code, .. } => {
                assert_eq!(status_code, 502);
            }
            _ => panic!("Expected RetryableError for 502 when retry_wait>0"),
        }
    }

    #[test]
    fn test_error_502_fatal_when_retry_wait_zero() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_retry_wait(0);
        let resp = make_response(502, None);
        let url = Url::parse("http://example.com/file").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::FatalError { status_code, .. } => {
                assert_eq!(status_code, 502);
            }
            _ => panic!("Expected FatalError for 502 when retry_wait=0"),
        }
    }

    #[test]
    fn test_error_503_same_as_502() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_retry_wait(2);
        let resp = make_response(503, None);
        let url = Url::parse("http://example.com/file").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::RetryableError { status_code, .. } => {
                assert_eq!(status_code, 503);
            }
            _ => panic!("Expected RetryableError for 503 when retry_wait>0"),
        }
    }

    #[test]
    fn test_error_504_always_retryable() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT).with_retry_wait(0);
        let resp = make_response(504, None);
        let url = Url::parse("http://example.com/file").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::RetryableError { status_code, .. } => {
                assert_eq!(status_code, 504);
            }
            _ => panic!("Expected RetryableError for 504"),
        }
    }

    #[test]
    fn test_error_500_fatal() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_response(500, None);
        let url = Url::parse("http://example.com/file").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::FatalError { status_code, message } => {
                assert_eq!(status_code, 500);
                assert!(message.contains("500"));
            }
            _ => panic!("Expected FatalError for generic 5xx"),
        }
    }

    #[test]
    fn test_error_403_forbidden() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_response(403, None);
        let url = Url::parse("http://example.com/forbidden").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::FatalError { status_code, .. } => {
                assert_eq!(status_code, 403);
            }
            _ => panic!("Expected FatalError for 403"),
        }
    }

    #[test]
    fn test_success_status_consumed() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_response(200, None);
        let url = Url::parse("http://example.com/file").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        assert!(matches!(result, SkipResponseResult::Consumed));
    }

    // ==================== Body consumption tests ====================

    #[test]
    fn test_consume_body_with_data() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let mut resp = HttpResponse::new(500, "Internal Server Error".to_string());
        resp.body = b"Error body content here".to_vec();
        let url = Url::parse("http://example.com/file").unwrap();

        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::FatalError { status_code, .. } => {
                assert_eq!(status_code, 500);
            }
            _ => panic!("Expected FatalError"),
        }
    }

    #[test]
    fn test_consume_body_empty() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = HttpResponse::new(404, "Not Found".to_string());
        let url = Url::parse("http://example.com/missing").unwrap();

        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();
        match result {
            SkipResponseResult::FatalError { status_code, .. } => {
                assert_eq!(status_code, 404);
            }
            _ => panic!("Expected FatalError"),
        }
    }

    // ==================== Utility tests ====================

    #[test]
    fn test_extract_realm_quoted() {
        let realm = HttpSkipResponseHandler::extract_realm(r#"Basic realm="My Realm""#);
        assert_eq!(realm, "My Realm");
    }

    #[test]
    fn test_extract_realm_unquoted() {
        let realm = HttpSkipResponseHandler::extract_realm("Basic realm=testrealm");
        assert_eq!(realm, "testrealm");
    }

    #[test]
    fn test_extract_realm_missing() {
        let realm = HttpSkipResponseHandler::extract_realm("Negotiate");
        assert!(realm.is_empty());
    }

    #[test]
    fn test_extract_realm_with_trailing_params() {
        let realm =
            HttpSkipResponseHandler::extract_realm(r#"Digest realm="test", nonce="abc""#);
        assert_eq!(realm, "test");
    }

    #[test]
    fn test_auth_scheme_from_header() {
        assert_eq!(
            AuthScheme::from_header(r#"Basic realm="x""#),
            Some(AuthScheme::Basic)
        );
        assert_eq!(
            AuthScheme::from_header(r#"Digest realm="x", nonce="y""#),
            Some(AuthScheme::Digest)
        );
        assert_eq!(
            AuthScheme::from_header("Negotiate"),
            Some(AuthScheme::Negotiate)
        );
        assert_eq!(AuthScheme::from_header("NTLM"), Some(AuthScheme::Ntlm));
        assert_eq!(AuthScheme::from_header("UnknownScheme"), None);
        assert_eq!(AuthScheme::from_header(""), None);
    }

    #[test]
    fn test_redirect_type_should_change_method() {
        // SeeOther always changes
        assert!(RedirectType::SeeOther.should_change_method(HttpMethod::Get));
        assert!(RedirectType::SeeOther.should_change_method(HttpMethod::Post));

        // Permanent changes POST -> GET
        assert!(RedirectType::Permanent.should_change_method(HttpMethod::Post));
        assert!(!RedirectType::Permanent.should_change_method(HttpMethod::Get));

        // Temporary (302) changes POST -> GET
        assert!(RedirectType::Temporary.should_change_method(HttpMethod::Post));
        assert!(!RedirectType::Temporary.should_change_method(HttpMethod::Get));

        // PreserveMethod (307/308) never changes
        assert!(!RedirectType::PreserveMethod.should_change_method(HttpMethod::Post));
        assert!(!RedirectType::PreserveMethod.should_change_method(HttpMethod::Get));
    }

    // ==================== 300 Multiple Choices tests ====================

    #[test]
    fn test_300_with_location_redirects() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = make_response(300, Some("http://example.com/choice1"));
        let url = Url::parse("http://example.com/list").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::Redirect(info) => {
                assert_eq!(info.target_url.as_str(), "http://example.com/choice1");
                assert_eq!(info.redirect_type, RedirectType::Permanent);
            }
            _ => panic!("Expected Redirect for 300 with Location, got {:?}", result),
        }
    }

    #[test]
    fn test_300_without_location_is_fatal() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = HttpResponse::new(300, "Multiple Choices".to_string());
        let url = Url::parse("http://example.com/list").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Get, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::FatalError { status_code, .. } => {
                assert_eq!(status_code, 300);
            }
            _ => panic!("Expected FatalError for 300 without Location, got {:?}", result),
        }
    }

    // ==================== 413 Request Entity Too Large tests ====================

    #[test]
    fn test_413_with_retry_after_is_retryable() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let mut resp = HttpResponse::new(413, "Payload Too Large".to_string());
        resp.headers
            .push(("Retry-After".to_string(), "60".to_string()));
        let url = Url::parse("http://example.com/upload").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Post, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::RetryableError { status_code, .. } => {
                assert_eq!(status_code, 413);
            }
            _ => panic!("Expected RetryableError for 413 with Retry-After, got {:?}", result),
        }
    }

    #[test]
    fn test_413_without_retry_after_is_fatal() {
        let handler = HttpSkipResponseHandler::new(MAX_REDIRECT_COUNT);
        let resp = HttpResponse::new(413, "Payload Too Large".to_string());
        let url = Url::parse("http://example.com/upload").unwrap();
        let result = handler
            .handle(&resp, HttpMethod::Post, &url, 0)
            .unwrap();

        match result {
            SkipResponseResult::FatalError { status_code, .. } => {
                assert_eq!(status_code, 413);
            }
            _ => panic!("Expected FatalError for 413 without Retry-After, got {:?}", result),
        }
    }
}

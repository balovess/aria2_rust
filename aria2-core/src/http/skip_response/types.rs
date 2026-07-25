//! Type definitions for the HTTP skip response handler.
//!
//! Contains enums and structs for redirect classification, authentication
//! challenges, and the overall skip-response result.

use url::Url;

use crate::http::digest_auth::DigestAuthChallenge;
use crate::http::request_response::HttpMethod;

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

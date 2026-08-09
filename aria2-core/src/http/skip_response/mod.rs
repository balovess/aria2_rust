//! HTTP skip response handler module
//!
//! Handles HTTP responses that are NOT file data: redirects (3xx),
//! authentication challenges (401/407), and error responses (4xx/5xx).
//! Consumes and discards the response body using NullSinkFilter while
//! extracting relevant metadata for subsequent processing.
//!
//! Based on C++ aria2's `HttpSkipResponseCommand` which skips the response
//! body and processes redirect/auth/error status codes.

pub mod handler;
pub mod types;

#[cfg(test)]
mod tests;

// Re-export ALL public items so that external code using
// `crate::http::skip_response::X` continues to work unchanged.

pub use handler::HttpResponse;
pub use handler::HttpSkipResponseHandler;

pub use types::AuthScheme;
pub use types::HttpAuthChallenge;
pub use types::HttpRedirectInfo;
pub use types::MAX_REDIRECT_COUNT;
pub use types::RedirectType;
pub use types::SkipResponseResult;

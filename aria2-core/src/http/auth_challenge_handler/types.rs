//! Public types for the authentication challenge handler.

/// Outcome of processing an authentication challenge.
///
/// Returned by [`handle_auth_challenge`](super::handle_auth_challenge) to tell
/// the caller what to do next.
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

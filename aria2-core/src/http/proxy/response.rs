//! Proxy response types and classification.

use crate::http::header_processor::HttpResponseHead;

/// Result of parsing a proxy's HTTP response during CONNECT/forward handshake.
#[derive(Debug, Clone)]
pub enum ProxyResponse {
    /// Tunnel/forward connection established successfully (HTTP 200).
    /// The proxy response headers are included for inspection.
    Connected(HttpResponseHead),
    /// Proxy requires authentication (HTTP 407).
    /// Contains the Proxy-Authenticate challenge header value(s).
    AuthRequired {
        /// The parsed response head (contains Proxy-Authenticate headers)
        response: HttpResponseHead,
    },
    /// Proxy returned an error status code.
    Error {
        /// HTTP status code
        status_code: u16,
        /// Reason phrase
        reason: String,
    },
}

impl ProxyResponse {
    /// Classify an [HttpResponseHead] from a proxy into a [ProxyResponse].
    pub(crate) fn from_head(head: HttpResponseHead) -> Self {
        match head.status_code {
            200 => ProxyResponse::Connected(head),
            407 => ProxyResponse::AuthRequired { response: head },
            code => ProxyResponse::Error {
                status_code: code,
                reason: head.reason_phrase.clone(),
            },
        }
    }
}
